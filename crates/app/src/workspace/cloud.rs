//! Cloud account, live queries and the editor/provider binding. The socket lives
//! in schist-cloud; this module translates its events into GPUI state changes.
use super::*;
use anyhow::{anyhow, Result};
use remote::transfer::DownloadedAsset;
use schist_cloud::{
    self as remote,
    protocol::{bytes, map, parse, value},
    Account, Asset, AssetQuery, Bucket, CatalogueQuery, Client, Event, Filters, Folder, Scope,
    Value, WatchQuery,
};
use schist_core::DocumentId;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

#[cfg(not(target_arch = "wasm32"))]
const CREDENTIAL_KEY: &str = "https://schist.app/schist-cloud";
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn state_dir() -> PathBuf {
    schist_gallery::state_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("schist/cloud")
}

pub(crate) struct RemoteDocument {
    pub asset: Asset,
    pub shared: remote::document::SharedDocument,
    pub joined: bool,
    pub detached: bool,
    pub vector: Vec<u8>,
    pub sending: bool,
    pub changed: bool,
    pub generation: u64,
    pub saved: u64,
    pub render: bool,
}
enum Pending {
    Capabilities,
    Join(DocumentId),
    Update {
        doc: DocumentId,
        vector: Vec<u8>,
        generation: u64,
    },
    Mutation,
}
impl Pending {
    fn belongs_to(&self, id: DocumentId) -> bool {
        matches!(self, Self::Join(doc) | Self::Update { doc, .. } if *doc == id)
    }
}
impl RemoteDocument {
    fn detach(&mut self) {
        self.detached = true;
        self.joined = false;
        self.sending = false;
        self.render = false;
    }
}
#[cfg(not(target_arch = "wasm32"))]
enum RecoveryTask {
    Write {
        epoch: u64,
        files: Vec<(PathBuf, Vec<u8>)>,
    },
    Remove(PathBuf),
}
enum Job {
    Thumbnail {
        epoch: u64,
        id: String,
        revision: u64,
        image: Option<Arc<RenderImage>>,
    },
    #[cfg(not(target_arch = "wasm32"))]
    Browser {
        epoch: u64,
        url: String,
    },
    SignedIn {
        epoch: u64,
        account: Account,
    },
    Error {
        epoch: u64,
        error: String,
    },
    Opened {
        epoch: u64,
        asset: Asset,
        download: DownloadedAsset,
    },
    Downloaded {
        epoch: u64,
        name: String,
        download: DownloadedAsset,
    },
    Uploaded {
        epoch: u64,
        doc: DocumentId,
        asset: Asset,
    },
    Done {
        epoch: u64,
        message: String,
    },
}
pub(crate) struct CloudState {
    pub generation: super::cloud_generation::GenerationState,
    pub account: Option<Account>,
    pub client: Option<Client>,
    pub connected: bool,
    pub capabilities: Option<remote::Capabilities>,
    pub capabilities_ready: bool,
    pub download_target: Option<Asset>,
    pub show: bool,
    pub message: String,
    pub thumbnails: HashMap<String, (u64, Arc<RenderImage>)>,
    thumbnail_jobs: HashSet<(String, u64)>,
    thumbnail_active: usize,
    pub folders: Vec<Folder>,
    pub buckets: Vec<Bucket>,
    pub assets: Vec<Asset>,
    pub total: u64,
    pub query: AssetQuery,
    pub selected: HashSet<String>,
    pub catalogue: String,
    pub folders_offset: u64,
    pub buckets_offset: u64,
    pub folders_total: u64,
    pub buckets_total: u64,
    pub docs: HashMap<DocumentId, RemoteDocument>,
    pending: HashMap<String, Pending>,
    pub epoch: u64,
    jobs: mpsc::Receiver<Job>,
    sender: mpsc::Sender<Job>,
    cancel: Arc<AtomicBool>,
    writes: VecDeque<Option<Account>>,
    #[cfg(not(target_arch = "wasm32"))]
    writing: bool,
    #[cfg(not(target_arch = "wasm32"))]
    recovery: mpsc::Sender<RecoveryTask>,
    pub watching: String,
    folders_watch: String,
    buckets_watch: String,
    pub form_target: Option<(String, u64)>,
    pub form_scope: Scope,
}
impl Default for CloudState {
    fn default() -> Self {
        let (sender, jobs) = mpsc::channel();
        #[cfg(not(target_arch = "wasm32"))]
        let recovery = {
            let (recovery, tasks) = mpsc::channel();
            let errors = sender.clone();
            std::thread::spawn(move || {
                while let Ok(task) = tasks.recv() {
                    match task {
                        RecoveryTask::Write { epoch, files } => {
                            for (path, bytes) in files {
                                if let Err(e) = remote::auth::private_write(&path, &bytes) {
                                    let _ = errors.send(Job::Error {
                                        epoch,
                                        error: format!("Cloud recovery failed: {e}"),
                                    });
                                }
                            }
                        }
                        RecoveryTask::Remove(path) => {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
            });
            recovery
        };
        Self {
            generation: Default::default(),
            account: None,
            client: None,
            connected: false,
            capabilities: None,
            capabilities_ready: false,
            download_target: None,
            show: false,
            message: "Not signed in".into(),
            thumbnails: HashMap::new(),
            thumbnail_jobs: HashSet::new(),
            thumbnail_active: 0,
            folders: vec![],
            buckets: vec![],
            assets: vec![],
            total: 0,
            query: Default::default(),
            selected: HashSet::new(),
            catalogue: String::new(),
            folders_offset: 0,
            buckets_offset: 0,
            folders_total: 0,
            buckets_total: 0,
            docs: HashMap::new(),
            pending: HashMap::new(),
            epoch: 0,
            jobs,
            sender,
            cancel: Arc::new(AtomicBool::new(false)),
            writes: VecDeque::new(),
            #[cfg(not(target_arch = "wasm32"))]
            writing: false,
            #[cfg(not(target_arch = "wasm32"))]
            recovery,
            watching: String::new(),
            folders_watch: String::new(),
            buckets_watch: String::new(),
            form_target: None,
            form_scope: Scope::Library,
        }
    }
}
impl Drop for CloudState {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl CloudState {
    fn joinable_documents(&self) -> Vec<DocumentId> {
        self.docs
            .iter()
            .filter(|(_, doc)| !doc.detached)
            .map(|(id, _)| *id)
            .collect()
    }
    fn disconnect(&mut self) {
        self.connected = false;
        self.capabilities_ready = false;
        for document in self.docs.values_mut() {
            document.joined = false;
            document.sending = false;
        }
        self.pending.clear();
    }
    fn detach_document(&mut self, asset: &str) -> Option<DocumentId> {
        let id = self
            .docs
            .iter()
            .find(|(_, document)| document.asset.id == asset)
            .map(|(id, _)| *id)?;
        self.docs.get_mut(&id)?.detach();
        self.pending.retain(|_, pending| !pending.belongs_to(id));
        Some(id)
    }
    fn apply_document_update(&mut self, asset: &str, update: &[u8]) -> Result<()> {
        if let Some(document) = self
            .docs
            .values_mut()
            .find(|doc| doc.asset.id == asset && !doc.detached)
        {
            document.shared.apply(update)?;
            document.render = true;
        }
        Ok(())
    }
    fn reopen_document(&mut self, id: DocumentId) -> bool {
        let Some(document) = self.docs.get_mut(&id) else {
            return false;
        };
        std::mem::replace(&mut document.detached, false)
    }
}
impl Workspace {
    pub(crate) fn cloud_start(&mut self, cx: &mut Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let read = cx.read_credentials(CREDENTIAL_KEY);
            let epoch = self.cloud.epoch;
            cx.spawn(async move |this, cx| {
                let result = read.await;
                let _ = this.update(cx, |ws, cx| {
                    if ws.cloud.epoch != epoch {
                        return;
                    }
                    match result {
                        Ok(Some((_, data))) => match serde_json::from_slice::<Account>(&data) {
                            Ok(account) => ws.cloud_connect(account, cx),
                            Err(e) => ws.cloud_error(format!("Stored cloud login is invalid: {e}")),
                        },
                        Ok(None) => {}
                        Err(e) => ws.cloud_error(format!("Could not read cloud login: {e}")),
                    }
                });
            })
            .detach();
        }
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(75))
                .await;
            if this.update(cx, |ws, cx| ws.cloud_tick(cx)).is_err() {
                break;
            }
        })
        .detach();
    }
    fn cloud_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.status = error.clone().into();
        self.cloud.message = error;
    }
    pub(crate) fn cloud_sign_in(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            self.cloud_login("https://schist.app".into(), cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.open_modal(
                Modal::Cloud {
                    kind: "sign-in",
                    fields: vec![(
                        "cloud-domain",
                        "Domain".into(),
                        remote::DEFAULT_DOMAIN.into(),
                    )],
                },
                cx,
            );
            self.focus_field("cloud-domain", remote::DEFAULT_DOMAIN);
        }
    }
    fn cloud_login(&mut self, domain: String, cx: &mut Context<Self>) {
        self.cloud.epoch += 1;
        let epoch = self.cloud.epoch;
        self.cloud.cancel.store(true, Ordering::Relaxed);
        self.cloud.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cloud.cancel.clone();
        let sender = self.cloud.sender.clone();
        self.cloud.message = "Opening sign-in in your browser…".into();
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let login = remote::auth::Login::discover(&domain, &state_dir())?;
                let _ = sender.send(Job::Browser {
                    epoch,
                    url: login.browser_url.clone(),
                });
                while !cancel.load(Ordering::Relaxed) {
                    if let Some(callback) = login.poll()? {
                        match login.exchange(&callback) {
                            Ok(account) => {
                                let _ = sender.send(Job::SignedIn { epoch, account });
                                return Ok(());
                            }
                            Err(e) => {
                                let _ = sender.send(Job::Error {
                                    epoch,
                                    error: e.to_string(),
                                });
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(())
            })();
            if let Err(e) = result {
                let _ = sender.send(Job::Error {
                    epoch,
                    error: format!("Cloud sign-in failed: {e}"),
                });
            }
        });
        #[cfg(target_arch = "wasm32")]
        {
            let result = remote::auth::domain(&domain).and_then(|_| remote::auth::Login::open());
            match result {
                Ok(login) => remote::runtime::spawn(async move {
                    let job = match login.finish(&cancel).await {
                        Ok(account) => Job::SignedIn { epoch, account },
                        Err(e) => Job::Error {
                            epoch,
                            error: e.to_string(),
                        },
                    };
                    let _ = sender.send(job);
                }),
                Err(e) => self.cloud_error(e.to_string()),
            }
        }
        cx.notify();
    }
    fn cloud_connect(&mut self, account: Account, cx: &mut Context<Self>) {
        self.cloud.client = Some(Client::start(account.clone()));
        self.cloud.account = Some(account.clone());
        self.cloud.writes.push_back(Some(account));
        self.cloud.message = "Connecting to Schist Cloud…".into();
        self.cloud.pending.clear();
        self.cloud_refresh_catalogue();
        self.cloud_browse(Scope::Library, cx);
    }
    pub(crate) fn cloud_sign_out(&mut self, cx: &mut Context<Self>) {
        self.cloud_capture_edit();
        self.cloud_checkpoint();
        self.cloud.epoch += 1;
        self.cloud.cancel.store(true, Ordering::Relaxed);
        self.cloud.generation.cancel.store(true, Ordering::Relaxed);
        self.cloud.generation = Default::default();
        let account = self.cloud.account.take();
        self.cloud.client = None;
        self.cloud.connected = false;
        self.cloud.capabilities = None;
        self.cloud.capabilities_ready = false;
        self.cloud.download_target = None;
        self.cloud.show = false;
        self.cloud.pending.clear();
        self.cloud.docs.clear();
        self.cloud.assets.clear();
        self.cloud.thumbnails.clear();
        self.cloud.thumbnail_jobs.clear();
        self.cloud.thumbnail_active = 0;
        self.cloud.folders.clear();
        self.cloud.buckets.clear();
        self.cloud.writes.push_back(None);
        self.cloud.message = "Signed out".into();
        if let Some(account) = account {
            let sender = self.cloud.sender.clone();
            let epoch = self.cloud.epoch;
            remote::runtime::spawn(async move {
                if let Err(e) = remote::auth::logout_async(&account).await {
                    let _ = sender.send(Job::Error {
                        epoch,
                        error: format!("Signed out locally; server logout failed: {e}"),
                    });
                }
            });
        }
        cx.notify();
    }
    pub(crate) fn cloud_set_visible(&mut self, visible: bool) {
        self.cloud.show = visible;
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.library.open = visible;
        }
    }
    pub(crate) fn cloud_browse(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.cloud.account.is_none() {
            self.cloud_sign_in(cx);
            return;
        }
        self.cloud.show = true;
        self.cloud_set_visible(true);
        self.cloud.query.scope = scope;
        self.cloud.query.offset = 0;
        self.cloud.selected.clear();
        self.cloud_watch_assets();
        cx.notify();
    }
    pub(crate) fn cloud_watch_assets(&mut self) {
        if let Some(c) = &self.cloud.client {
            if !self.cloud.watching.is_empty() {
                c.handle.unwatch(&self.cloud.watching);
            }
            self.cloud.watching = remote::Uuid::new_v4().to_string();
            self.cloud.assets.clear();
            self.cloud.total = 0;
            c.handle.watch(
                &self.cloud.watching,
                WatchQuery::Assets {
                    query: Box::new(self.cloud.query.clone()),
                },
            );
        }
    }
    pub(crate) fn cloud_refresh_catalogue(&mut self) {
        if let Some(c) = &self.cloud.client {
            c.handle.unwatch(&self.cloud.folders_watch);
            c.handle.unwatch(&self.cloud.buckets_watch);
            self.cloud.folders_watch = remote::Uuid::new_v4().to_string();
            self.cloud.buckets_watch = remote::Uuid::new_v4().to_string();
            c.handle.watch(
                &self.cloud.folders_watch,
                WatchQuery::Folders {
                    query: CatalogueQuery {
                        text: self.cloud.catalogue.clone(),
                        offset: self.cloud.folders_offset,
                        limit: 500,
                    },
                },
            );
            c.handle.watch(
                &self.cloud.buckets_watch,
                WatchQuery::Buckets {
                    query: CatalogueQuery {
                        text: self.cloud.catalogue.clone(),
                        offset: self.cloud.buckets_offset,
                        limit: 500,
                    },
                },
            );
        }
    }
    fn cloud_load_thumbnails(&mut self) {
        if !self.cloud.show {
            return;
        }
        // A small worker pool and one page of retained images bound network/texture use.
        let visible: HashSet<_> = self.cloud.assets.iter().map(|a| a.id.clone()).collect();
        self.cloud.thumbnails.retain(|id, _| visible.contains(id));
        self.cloud
            .thumbnail_jobs
            .retain(|(id, _)| visible.contains(id));
        for asset in &self.cloud.assets {
            if self.cloud.thumbnail_active >= 4 {
                break;
            }
            if self
                .cloud
                .thumbnails
                .get(&asset.id)
                .is_some_and(|(r, _)| *r == asset.revision)
            {
                continue;
            }
            let Some(url) = asset.thumbnail_url.clone() else {
                continue;
            };
            if !self
                .cloud
                .thumbnail_jobs
                .insert((asset.id.clone(), asset.revision))
            {
                continue;
            }
            self.cloud.thumbnail_active += 1;
            let sender = self.cloud.sender.clone();
            let epoch = self.cloud.epoch;
            let id = asset.id.clone();
            let revision = asset.revision;
            remote::runtime::spawn(async move {
                let image = (async {
                    let bytes = remote::auth::download_limited_async(&url, 8 * 1024 * 1024).await?;
                    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
                        .with_guessed_format()?;
                    let mut limits = image::Limits::default();
                    limits.max_image_width = Some(4096);
                    limits.max_image_height = Some(4096);
                    limits.max_alloc = Some(64 * 1024 * 1024);
                    reader.limits(limits);
                    let image = reader.decode()?.thumbnail(256, 256).to_rgba8();
                    rgba_to_render_image(image.width(), image.height(), image.into_raw())
                        .ok_or_else(|| anyhow!("Invalid thumbnail"))
                })
                .await
                .ok();
                let _ = sender.send(Job::Thumbnail {
                    epoch,
                    id,
                    revision,
                    image,
                });
            });
        }
    }
    fn cloud_tick(&mut self, cx: &mut Context<Self>) {
        self.cloud_capture_edit();
        self.cloud_generation_tick(cx);
        let mut changed = false;
        while let Ok(job) = self.cloud.jobs.try_recv() {
            changed = true;
            match job {
                Job::Thumbnail {
                    epoch,
                    id,
                    revision,
                    image,
                } if epoch == self.cloud.epoch => {
                    self.cloud.thumbnail_active = self.cloud.thumbnail_active.saturating_sub(1);
                    if let Some(image) = image {
                        self.cloud.thumbnails.insert(id, (revision, image));
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                Job::Browser { epoch, url } if epoch == self.cloud.epoch => cx.open_url(&url),
                Job::SignedIn { epoch, account } if epoch == self.cloud.epoch => {
                    self.cloud_connect(account, cx)
                }
                Job::Error { epoch, error } if epoch == self.cloud.epoch => self.cloud_error(error),
                Job::Done { epoch, message } if epoch == self.cloud.epoch => {
                    self.status = message.clone().into();
                    self.cloud.message = message;
                }
                Job::Opened {
                    epoch,
                    mut asset,
                    download,
                } if epoch == self.cloud.epoch => {
                    asset.name = download.suggested_name(&asset.name);
                    asset.revision = download.revision;
                    if let Some(mime) = &download.content_type {
                        asset.mime_type = mime.clone();
                    }
                    if let Err(e) = self.cloud_install(asset, download.bytes, cx) {
                        self.cloud_error(format!("Open cloud document: {e}"));
                    }
                }
                Job::Downloaded {
                    epoch,
                    name,
                    download,
                } if epoch == self.cloud.epoch => {
                    self.cloud_save_download(name, download, cx);
                }
                Job::Uploaded { epoch, doc, asset } if epoch == self.cloud.epoch => {
                    if let Some(source) = self.cloud_doc(doc) {
                        match remote::document::SharedDocument::unseeded(source) {
                            Ok(shared) => {
                                self.cloud_close_document(doc);
                                self.cloud.docs.insert(
                                    doc,
                                    RemoteDocument {
                                        asset,
                                        shared,
                                        joined: false,
                                        detached: false,
                                        vector: vec![0],
                                        sending: false,
                                        changed: true,
                                        generation: 0,
                                        saved: 0,
                                        render: false,
                                    },
                                );
                                self.cloud_join(doc);
                            }
                            Err(e) => self.cloud_error(e.to_string()),
                        }
                    }
                }
                _ => {}
            }
        }
        let events: Vec<_> = self
            .cloud
            .client
            .as_ref()
            .map(|c| c.events.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            changed = true;
            if let Err(e) = self.cloud_event(event, cx) {
                self.cloud_error(e.to_string());
            }
        }
        // Capture before applying remote state, so local changes made in the same UI tick survive.
        self.cloud_capture_edit();
        if !self.pointer_down && self.modal.is_none() {
            let ids: Vec<_> = self
                .cloud
                .docs
                .iter()
                .filter(|(_, d)| d.render)
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                if let Err(e) = self.cloud_render_document(id) {
                    self.cloud_error(e.to_string());
                }
                changed = true;
            }
        }
        let ids: Vec<_> = self.cloud.docs.keys().copied().collect();
        for id in ids {
            self.cloud_send_document(id);
        }
        self.cloud_load_thumbnails();
        self.cloud_persist_credentials(cx);
        if changed {
            cx.notify();
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn cloud_persist_credentials(&mut self, cx: &mut Context<Self>) {
        if self.cloud.writing {
            return;
        }
        let Some(account) = self.cloud.writes.pop_front() else {
            return;
        };
        self.cloud.writing = true;
        let task = match account {
            Some(account) => match serde_json::to_vec(&account) {
                Ok(data) => cx.write_credentials(CREDENTIAL_KEY, &account.domain, &data),
                Err(e) => {
                    self.cloud.writing = false;
                    self.cloud_error(e.to_string());
                    return;
                }
            },
            None => cx.delete_credentials(CREDENTIAL_KEY),
        };
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |ws, cx| {
                ws.cloud.writing = false;
                if let Err(e) = result {
                    ws.cloud_error(format!("Could not persist cloud login: {e}"));
                }
                cx.notify();
            });
        })
        .detach();
    }
    #[cfg(target_arch = "wasm32")]
    fn cloud_persist_credentials(&mut self, _: &mut Context<Self>) {
        self.cloud.writes.clear();
    }
    fn cloud_event(&mut self, event: Event, cx: &mut Context<Self>) -> Result<()> {
        match event {
            Event::Connected => {
                self.cloud.connected = true;
                self.cloud.capabilities = None;
                self.cloud.capabilities_ready = false;
                self.cloud.message = "Connected to Schist Cloud".into();
                if let Some(client) = &self.cloud.client {
                    let id = client.handle.call("workspace.capabilities", map([]));
                    self.cloud.pending.insert(id, Pending::Capabilities);
                }
            }
            Event::Disconnected(error) => {
                self.cloud.disconnect();
                self.cloud.message = error;
            }
            Event::Credentials(account) => {
                self.cloud.account = Some(account.clone());
                self.cloud.writes.push_back(Some(account));
            }
            Event::Snapshot {
                subscription_id,
                snapshot,
            } => match subscription_id.as_str() {
                id if id == self.cloud.folders_watch => {
                    self.cloud.folders = snapshot
                        .items
                        .into_iter()
                        .map(parse)
                        .collect::<Result<_>>()?;
                    self.cloud.folders_total = snapshot.total;
                }
                id if id == self.cloud.buckets_watch => {
                    self.cloud.buckets = snapshot
                        .items
                        .into_iter()
                        .map(parse)
                        .collect::<Result<_>>()?;
                    self.cloud.buckets_total = snapshot.total;
                }
                id if id == self.cloud.watching => {
                    self.cloud.assets = snapshot
                        .items
                        .into_iter()
                        .map(parse)
                        .collect::<Result<_>>()?;
                    self.cloud.total = snapshot.total;
                    let ids: HashSet<_> = self.cloud.assets.iter().map(|a| a.id.clone()).collect();
                    self.cloud.selected.retain(|id| ids.contains(id));
                }
                _ => {}
            },
            Event::WatchError {
                subscription_id,
                error,
            } => {
                if subscription_id == self.cloud.watching {
                    self.cloud.assets.clear();
                    self.cloud.total = 0;
                }
                self.cloud_error(error);
            }
            Event::DocumentUpdate { asset_id, bytes } => {
                self.cloud.apply_document_update(&asset_id, &bytes)?;
            }
            Event::DocumentError { asset_id, error } => {
                if let Some(id) = self.cloud.detach_document(&asset_id) {
                    if self.close_after_save == Some(id) {
                        self.close_after_save = None;
                        self.cancel_quit();
                    }
                }
                self.cloud_error(error);
            }
            Event::Reply { id, result } => {
                let Some(p) = self.cloud.pending.remove(&id) else {
                    return Ok(());
                };
                match (p, result) {
                    (Pending::Capabilities, result) => {
                        let capabilities = remote::Capabilities::from_reply(result)?;
                        if let Some(client) = &self.cloud.client {
                            client.handle.set_frame_limit(
                                capabilities
                                    .as_ref()
                                    .map_or(remote::MAX_FRAME, |c| c.frame_limit()),
                            );
                        }
                        self.cloud.capabilities = capabilities;
                        self.cloud.capabilities_ready = true;
                        let ids = self.cloud.joinable_documents();
                        for id in ids {
                            self.cloud_join(id);
                        }
                    }
                    (Pending::Join(id), Ok(result)) => {
                        if let Some(d) = self.cloud.docs.get_mut(&id) {
                            d.shared.apply(&bytes(&result, "update")?)?;
                            d.shared.seed_if_empty()?;
                            d.vector = bytes(&result, "state_vector")?;
                            d.joined = true;
                            d.changed = true;
                            d.render = true;
                        }
                    }
                    (
                        Pending::Update {
                            doc,
                            vector,
                            generation,
                        },
                        Ok(_),
                    ) => {
                        if let Some(d) = self.cloud.docs.get_mut(&doc) {
                            d.sending = false;
                            d.vector = vector;
                            d.saved = generation;
                            let saved = d.saved == d.generation;
                            if saved {
                                if let Some(doc) = self.cloud_doc_mut(doc) {
                                    doc.mark_saved();
                                }
                            }
                        }
                        if self.close_after_save == Some(doc)
                            && self
                                .cloud
                                .docs
                                .get(&doc)
                                .is_some_and(|d| d.saved == d.generation)
                        {
                            self.cloud_finish_save(doc, cx);
                        }
                    }
                    (Pending::Update { doc, .. }, Err(error)) => {
                        if let Some(d) = self.cloud.docs.get_mut(&doc) {
                            d.sending = false;
                            d.joined = false;
                        }
                        self.cloud_error(error);
                    }
                    (_, Err(error)) => self.cloud_error(error),
                    (_, Ok(_)) => {
                        self.cloud.message = "Cloud updated".into();
                    }
                }
            }
        }
        Ok(())
    }
    fn cloud_doc(&self, id: DocumentId) -> Option<&Document> {
        self.doc.as_ref().filter(|d| d.id == id).or_else(|| {
            self.background_tabs
                .iter()
                .find(|t| t.doc.id == id)
                .map(|t| &t.doc)
        })
    }
    fn cloud_doc_mut(&mut self, id: DocumentId) -> Option<&mut Document> {
        if self.doc.as_ref().is_some_and(|d| d.id == id) {
            self.doc.as_mut()
        } else {
            self.background_tabs
                .iter_mut()
                .find(|t| t.doc.id == id)
                .map(|t| &mut t.doc)
        }
    }
    fn cloud_install(&mut self, asset: Asset, data: Vec<u8>, cx: &mut Context<Self>) -> Result<()> {
        let mut doc = self
            .registry
            .codecs()
            .find(|c| c.probe(&data))
            .ok_or_else(|| anyhow!("Unsupported remote file format"))?
            .import(&data)?;
        doc.title = asset.name.clone();
        doc.path = None;
        #[allow(unused_mut)]
        let mut shared = remote::document::SharedDocument::unseeded(&doc)?;
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(bytes) = std::fs::read(self.cloud_recovery_path(&asset.id)) {
            match shared.restore(&bytes, &doc) {
                Ok(mut recovered) => {
                    recovered.id = doc.id;
                    doc = recovered;
                }
                Err(e) => self.cloud_error(format!("Could not restore cloud edits: {e}")),
            }
        }
        let id = doc.id;
        let generation = u64::from(doc.dirty);
        self.open_in_tab(doc, true);
        self.cloud_set_visible(false);
        self.cloud.docs.insert(
            id,
            RemoteDocument {
                asset,
                shared,
                joined: false,
                detached: false,
                vector: vec![0],
                sending: false,
                changed: true,
                generation,
                saved: 0,
                render: false,
            },
        );
        self.cloud_join(id);
        cx.notify();
        Ok(())
    }
    pub(crate) fn cloud_open(&mut self, asset: Asset, cx: &mut Context<Self>) {
        if let Some(id) = self
            .cloud
            .docs
            .iter()
            .find(|(_, d)| d.asset.id == asset.id)
            .map(|(id, _)| *id)
        {
            if self.cloud.reopen_document(id) {
                self.cloud_join(id);
            }
            if self.doc.as_ref().is_some_and(|d| d.id == id) {
                self.cloud_set_visible(false);
            } else if let Some(i) = self.background_tabs.iter().position(|t| t.doc.id == id) {
                let index = if i >= self.active_tab { i + 1 } else { i };
                self.select_tab(index, cx);
                self.cloud_set_visible(false);
            }
            cx.notify();
            return;
        }
        let Some(c) = &self.cloud.client else {
            return;
        };
        let handle = c.handle.clone();
        let sender = self.cloud.sender.clone();
        let epoch = self.cloud.epoch;
        self.cloud.message = format!("Opening {}…", asset.name);
        remote::runtime::spawn(async move {
            let result = handle.download_asset_async(&asset.id, None, None).await;
            let job = match result {
                Ok(download) => Job::Opened {
                    epoch,
                    asset,
                    download,
                },
                Err(e) => Job::Error {
                    epoch,
                    error: e.to_string(),
                },
            };
            let _ = sender.send(job);
        });
        cx.notify();
    }
    fn cloud_join(&mut self, id: DocumentId) {
        if !self.cloud.connected || !self.cloud.capabilities_ready {
            return;
        }
        if self
            .cloud
            .capabilities
            .as_ref()
            .is_some_and(|c| !c.supports_image_model())
        {
            self.cloud_error("This provider does not support collaborative image editing");
            return;
        }
        let Some(d) = self.cloud.docs.get_mut(&id) else {
            return;
        };
        if d.detached {
            return;
        }
        if self
            .cloud
            .pending
            .values()
            .any(|p| matches!(p,Pending::Join(doc) if *doc==id))
        {
            return;
        }
        d.joined = false;
        d.sending = false;
        if let Some(c) = &self.cloud.client {
            let req = c.handle.call(
                "document.join",
                map([
                    ("document_id", d.asset.id.clone().into()),
                    ("state_vector", Value::Binary(d.shared.state_vector())),
                ]),
            );
            self.cloud.pending.insert(req, Pending::Join(id));
        }
    }
    pub(crate) fn cloud_undo(&mut self, redo: bool, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.doc.as_ref().map(|d| d.id) else {
            return false;
        };
        self.cloud_capture_edit();
        let Some(d) = self.cloud.docs.get_mut(&id) else {
            return false;
        };
        if !d.joined {
            self.cloud_error("Connect before undoing cloud edits");
            return true;
        }
        if d.shared.undo(redo) {
            d.render = true;
            d.changed = true;
            d.generation += 1;
            if let Err(e) = self.cloud_render_document(id) {
                self.cloud_error(e.to_string());
            }
            self.cloud_send_document(id);
            cx.notify();
        }
        true
    }
    pub(crate) fn cloud_capture_edit(&mut self) {
        let Some(doc) = &self.doc else {
            return;
        };
        let id = doc.id;
        let Some(d) = self.cloud.docs.get_mut(&id) else {
            return;
        };
        if d.shared.revision == doc.revision {
            return;
        }
        match d.shared.local_changes(doc) {
            Ok(Some(_)) => {
                d.changed = true;
                d.generation += 1;
            }
            Ok(None) => {}
            Err(e) => {
                self.cloud_error(format!("Cloud edit is not saved: {e}"));
            }
        }
    }
    fn cloud_send_document(&mut self, id: DocumentId) {
        let Some(d) = self.cloud.docs.get_mut(&id) else {
            return;
        };
        if d.detached || !d.joined || d.sending || !d.changed {
            return;
        }
        if let Some(capabilities) = &self.cloud.capabilities {
            if let Err(error) = capabilities.check_document(d.shared.full_state().len()) {
                d.joined = false;
                self.cloud_error(error.to_string());
                return;
            }
        }
        let update = match d.shared.diff(&d.vector) {
            Ok(b) => b,
            Err(e) => {
                d.joined = false;
                self.cloud_error(e.to_string());
                return;
            }
        };
        if let Some(c) = &self.cloud.client {
            let vector = d.shared.state_vector();
            let generation = d.generation;
            let req = c.handle.call(
                "document.update",
                map([
                    ("document_id", d.asset.id.clone().into()),
                    ("update", Value::Binary(update)),
                ]),
            );
            d.sending = true;
            d.changed = false;
            self.cloud.pending.insert(
                req,
                Pending::Update {
                    doc: id,
                    vector,
                    generation,
                },
            );
        }
    }
    fn cloud_render_document(&mut self, id: DocumentId) -> Result<()> {
        let d = self
            .cloud
            .docs
            .get_mut(&id)
            .ok_or_else(|| anyhow!("No cloud document"))?;
        let mut next = d.shared.render()?;
        next.id = id;
        next.dirty = d.generation > d.saved;
        d.render = false;
        if let Some(doc) = self.cloud_doc_mut(id) {
            next.revision = doc.revision + 1;
            next.active_layer = doc
                .active_layer
                .filter(|id| next.tree.find(*id).is_some())
                .or(next.active_layer);
            next.selection = std::mem::take(&mut doc.selection);
            next.last_selection = doc.last_selection.take();
            next.saved_selections = std::mem::take(&mut doc.saved_selections);
            next.history_source = std::mem::take(&mut doc.history_source);
            next.selected = doc
                .selected
                .iter()
                .copied()
                .filter(|id| next.tree.find(*id).is_some())
                .collect();
            next.active_path = doc.active_path.filter(|i| *i < next.paths.len());
            std::mem::swap(&mut next.history, &mut doc.history);
            *doc = next;
        }
        if let Some(doc) = self.cloud_doc(id) {
            let r = doc.revision;
            if let Some(d) = self.cloud.docs.get_mut(&id) {
                d.shared.revision = r;
            }
        }
        if self.doc.as_ref().is_some_and(|d| d.id == id) {
            self.reset_per_document_caches();
            self.refresh_layer_styles();
        }
        Ok(())
    }
    pub(crate) fn cloud_save(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.doc.as_ref().map(|d| d.id) else {
            return false;
        };
        if !self.cloud.docs.contains_key(&id) {
            return false;
        }
        if self
            .cloud
            .capabilities
            .as_ref()
            .is_some_and(|c| !c.supports_image_model())
        {
            self.cloud_error("This provider does not support collaborative image editing; save a local copy instead");
            cx.notify();
            return true;
        }
        self.cloud_capture_edit();
        let remote = &self.cloud.docs[&id];
        if remote.detached {
            self.cloud_error("Cloud sync has stopped for this document. Reopen it from the cloud gallery to reconnect; edits remain local.");
            cx.notify();
            return true;
        }
        if remote.joined && !remote.sending && !remote.changed && remote.saved == remote.generation
        {
            if let Some(doc) = self.cloud_doc_mut(id) {
                doc.mark_saved();
            }
            self.cloud_finish_save(id, cx);
            self.status = "Saved to Schist Cloud".into();
            cx.notify();
            return true;
        }
        if !self.cloud.docs[&id].joined {
            self.cloud_join(id);
        }
        self.cloud_send_document(id);
        self.status = if self.cloud.connected {
            "Saving to Schist Cloud…"
        } else {
            "Offline — cloud edits will sync after reconnecting"
        }
        .into();
        cx.notify();
        true
    }
    fn cloud_finish_save(&mut self, id: DocumentId, cx: &mut Context<Self>) {
        if self.close_after_save != Some(id) {
            return;
        }
        self.close_after_save = None;
        let index = if self.doc.as_ref().is_some_and(|doc| doc.id == id) {
            Some(self.active_tab)
        } else {
            self.background_tabs
                .iter()
                .position(|tab| tab.doc.id == id)
                .map(|i| if i >= self.active_tab { i + 1 } else { i })
        };
        if let Some(index) = index {
            self.close_tab(index, cx);
            self.resume_quit(cx);
        }
    }
    pub(crate) fn cloud_close_document(&mut self, id: DocumentId) {
        self.cloud
            .pending
            .retain(|_, pending| !pending.belongs_to(id));
        if let Some(d) = self.cloud.docs.remove(&id) {
            #[cfg(not(target_arch = "wasm32"))]
            let _ = self
                .cloud
                .recovery
                .send(RecoveryTask::Remove(self.cloud_recovery_path(&d.asset.id)));
            if let Some(c) = &self.cloud.client {
                c.handle
                    .call("document.leave", map([("document_id", d.asset.id.into())]));
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn cloud_recovery_path(&self, asset: &str) -> PathBuf {
        use sha2::Digest;
        let domain = self
            .cloud
            .account
            .as_ref()
            .map(|a| a.domain.as_str())
            .unwrap_or("");
        state_dir().join(format!(
            "{:x}.msgpack",
            sha2::Sha256::digest(format!("{domain}\n{asset}"))
        ))
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn cloud_checkpoint(&mut self) {
        self.cloud_capture_edit();
        let snapshots = self
            .cloud
            .docs
            .values()
            .map(|d| {
                Ok((
                    self.cloud_recovery_path(&d.asset.id),
                    d.shared.checkpoint()?,
                ))
            })
            .collect::<Result<Vec<_>>>();
        match snapshots {
            Ok(files) => {
                let _ = self.cloud.recovery.send(RecoveryTask::Write {
                    epoch: self.cloud.epoch,
                    files,
                });
            }
            Err(e) => self.cloud_error(format!("Cloud recovery failed: {e}")),
        }
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn cloud_checkpoint(&mut self) {
        self.cloud_capture_edit();
    }
    pub(crate) fn cloud_mutate(&mut self, method: &str, fields: Vec<(&'static str, Value)>) {
        if let Some(c) = &self.cloud.client {
            let mut fields = fields;
            fields.push(("mutation_id", remote::Uuid::new_v4().to_string().into()));
            let id = c.handle.call(method, map(fields));
            self.cloud.pending.insert(id, Pending::Mutation);
        }
    }
    pub(crate) fn cloud_drop_remote(&mut self, bucket: String, items: Vec<Value>) {
        self.cloud_mutate(
            "bucket.add",
            vec![("id", bucket.into()), ("items", Value::Array(items))],
        );
    }
    pub(crate) fn cloud_pick_upload(&mut self, directory: bool, cx: &mut Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        let prompt = {
            let picker = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: !directory,
                directories: directory,
                multiple: true,
                prompt: Some("Upload to Schist Cloud".into()),
            });
            async move { picker.await? }
        };
        #[cfg(target_arch = "wasm32")]
        let prompt = crate::web::pick_cloud_files(directory);
        let (bucket, folder) = match &self.cloud.query.scope {
            Scope::Bucket { id } => (Some(id.clone()), None),
            Scope::Folder { id, .. } => (None, Some(id.clone())),
            _ => (None, None),
        };
        cx.spawn(async move |this, cx| {
            let result = prompt.await;
            let _ = this.update(cx, |ws, cx| {
                match result {
                    Ok(Some(paths)) => ws.cloud_drop_local(bucket, folder, paths, cx),
                    Ok(None) => {}
                    Err(error) => ws.cloud_error(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(crate) fn cloud_drop_local(
        &mut self,
        bucket: Option<String>,
        folder: Option<String>,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(c) = &self.cloud.client else {
            return;
        };
        let handle = c.handle.clone();
        let sender = self.cloud.sender.clone();
        let epoch = self.cloud.epoch;
        self.cloud.message = "Uploading files…".into();
        remote::runtime::spawn(async move {
            let result: Result<()> = (async {
                let mut files = Vec::new();
                for path in paths {
                    if path.is_dir() {
                        enumerate_files(&path, &path, &mut files)?;
                    } else {
                        #[cfg(not(target_arch = "wasm32"))]
                        let relative = None;
                        #[cfg(target_arch = "wasm32")]
                        let relative = crate::web::cloud_relative_path(&path);
                        files.push((path, relative));
                    }
                }
                let mut assets = Vec::new();
                for (path, relative) in files {
                    #[cfg(not(target_arch = "wasm32"))]
                    let bytes = std::fs::read(&path)?;
                    #[cfg(target_arch = "wasm32")]
                    let bytes = crate::web::read_file(&path)?;
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let mime = mime(&path);
                    let asset = handle
                        .upload_async(remote::Upload {
                            name: &name,
                            bytes: &bytes,
                            mime,
                            folder: folder.as_deref(),
                            asset: None,
                            relative: relative.as_deref(),
                            mutation: &remote::Uuid::new_v4().to_string(),
                        })
                        .await?;
                    assets.push(map([("kind", "asset".into()), ("id", asset.id.into())]));
                }
                if let Some(bucket) = bucket {
                    handle
                        .request_async(
                            "bucket.add",
                            map([
                                ("id", bucket.into()),
                                ("items", Value::Array(assets)),
                                ("mutation_id", remote::Uuid::new_v4().to_string().into()),
                            ]),
                        )
                        .await?;
                }
                Ok(())
            })
            .await;
            let job = match result {
                Ok(()) => Job::Done {
                    epoch,
                    message: "Upload complete".into(),
                },
                Err(e) => Job::Error {
                    epoch,
                    error: format!("Upload failed (completed files remain in Cloud): {e}"),
                },
            };
            let _ = sender.send(job);
        });
        cx.notify();
    }
    pub(crate) fn cloud_upload_document(&mut self, cx: &mut Context<Self>) {
        if self.cloud.account.is_none() {
            self.cloud_sign_in(cx);
            return;
        }
        let folder = match &self.cloud.query.scope {
            Scope::Folder { id, .. } => id.clone(),
            _ => String::new(),
        };
        self.open_modal(
            Modal::Cloud {
                kind: "upload-document",
                fields: vec![(
                    "cloud-folder",
                    "Folder ID (empty for unfiled)".into(),
                    folder,
                )],
            },
            cx,
        );
    }
    pub(crate) fn cloud_download_selected(&mut self, cx: &mut Context<Self>) {
        if !self.cloud.connected || !self.cloud.capabilities_ready {
            self.cloud_error("Wait for the cloud connection before downloading");
            cx.notify();
            return;
        }
        if self.cloud.selected.len() != 1 {
            self.cloud_error("Select one cloud photo to download");
            cx.notify();
            return;
        }
        self.cloud.download_target = self
            .cloud
            .assets
            .iter()
            .find(|asset| self.cloud.selected.contains(&asset.id))
            .cloned();
        self.open_modal(
            Modal::Cloud {
                kind: "download",
                fields: vec![("cloud-download-format", "Format".into(), String::new())],
            },
            cx,
        );
    }
    fn cloud_start_download(&mut self, format: Option<String>) -> Result<()> {
        let asset = self
            .cloud
            .download_target
            .clone()
            .ok_or_else(|| anyhow!("Select a cloud photo first"))?;
        let handle = self
            .cloud
            .client
            .as_ref()
            .ok_or_else(|| anyhow!("Sign in first"))?
            .handle
            .clone();
        let capabilities = self.cloud.capabilities.clone();
        let epoch = self.cloud.epoch;
        let sender = self.cloud.sender.clone();
        self.cloud.message = format!("Downloading {}…", asset.name);
        remote::runtime::spawn(async move {
            let job = match handle
                .download_asset_async(&asset.id, format.as_deref(), capabilities.as_ref())
                .await
            {
                Ok(download) => Job::Downloaded {
                    epoch,
                    name: download.suggested_name(&asset.name),
                    download,
                },
                Err(error) => Job::Error {
                    epoch,
                    error: format!("Cloud download failed: {error}"),
                },
            };
            let _ = sender.send(job);
        });
        Ok(())
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn cloud_save_download(
        &mut self,
        name: String,
        download: DownloadedAsset,
        cx: &mut Context<Self>,
    ) {
        let directory = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let prompt = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = prompt.await else {
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move { std::fs::write(&path, download.bytes).map(|()| path) })
                .await;
            let _ = this.update(cx, |ws, cx| {
                match result {
                    Ok(path) => {
                        ws.cloud.message = format!("Downloaded {}", path.display());
                        ws.status = ws.cloud.message.clone().into();
                    }
                    Err(error) => ws.cloud_error(format!("Could not save download: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }
    #[cfg(target_arch = "wasm32")]
    fn cloud_save_download(
        &mut self,
        name: String,
        download: DownloadedAsset,
        cx: &mut Context<Self>,
    ) {
        match crate::web::download_bytes(&name, &download.bytes) {
            Ok(()) => self.cloud.message = format!("Downloaded {name}"),
            Err(e) => self.cloud_error(e.to_string()),
        }
        cx.notify();
    }
    fn cloud_upload_current(&mut self, folder: Option<String>) -> Result<()> {
        let doc = self
            .doc
            .as_ref()
            .ok_or_else(|| anyhow!("Open a document first"))?;
        let data = schist_codec_psd::write_psd(doc)?;
        let id = doc.id;
        let name = format!("{}.psd", doc.title.trim_end_matches(".psd"));
        let handle = self
            .cloud
            .client
            .as_ref()
            .ok_or_else(|| anyhow!("Sign in first"))?
            .handle
            .clone();
        let sender = self.cloud.sender.clone();
        let epoch = self.cloud.epoch;
        remote::runtime::spawn(async move {
            let job = match handle
                .upload_async(remote::Upload {
                    name: &name,
                    bytes: &data,
                    mime: "image/vnd.adobe.photoshop",
                    folder: folder.as_deref(),
                    asset: None,
                    relative: None,
                    mutation: &remote::Uuid::new_v4().to_string(),
                })
                .await
            {
                Ok(asset) => Job::Uploaded {
                    epoch,
                    doc: id,
                    asset,
                },
                Err(e) => Job::Error {
                    epoch,
                    error: e.to_string(),
                },
            };
            let _ = sender.send(job);
        });
        Ok(())
    }
    pub(crate) fn cloud_submit(
        &mut self,
        kind: &str,
        fields: Vec<(&'static str, String, String)>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, _, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        match kind {
            "download" => {
                let format = get("cloud-download-format");
                self.cloud_start_download((!format.is_empty()).then_some(format))?;
            }
            "sign-in" => {
                let domain = remote::auth::domain(&get("cloud-domain"))?;
                self.cloud_login(domain, cx);
            }
            "search" => {
                let mut q = self.cloud.query.clone();
                q.text = get("cloud-query");
                q.offset = 0;
                q.filters = parse_filters(&fields)?;
                if let Some(r) = q.filters.min_rating {
                    anyhow::ensure!(r <= 5, "Rating must be 0–5");
                }
                if let Some(c) = &q.filters.content {
                    anyhow::ensure!(
                        ["all", "safe", "flagged"].contains(&c.as_str()),
                        "Invalid content filter"
                    );
                }
                self.cloud.query = q;
                self.cloud_watch_assets();
            }
            "catalogue" => {
                self.cloud.catalogue = get("cloud-query");
                self.cloud.folders_offset = 0;
                self.cloud.buckets_offset = 0;
                self.cloud_refresh_catalogue();
            }
            "new-folder" => {
                self.cloud_mutate(
                    "folder.create",
                    vec![
                        ("name", get("cloud-name").into()),
                        (
                            "parent_id",
                            match &self.cloud.query.scope {
                                Scope::Folder { id, .. } => id.clone().into(),
                                _ => Value::Nil,
                            },
                        ),
                    ],
                );
            }
            "new-bucket" | "edit-bucket" => {
                let filters = parse_filters(&fields)?;
                let text = get("cloud-query");
                let rule = if text.is_empty() && filters == Filters::default() {
                    Value::Nil
                } else {
                    let scope = if kind == "edit-bucket" {
                        self.cloud.form_scope.clone()
                    } else {
                        match &self.cloud.query.scope {
                            Scope::Bucket { .. } => Scope::Library,
                            s => s.clone(),
                        }
                    };
                    value(remote::Rule {
                        scope,
                        text,
                        filters,
                    })
                };
                let mut params = vec![("name", get("cloud-name").into()), ("rule", rule)];
                let method = if kind == "edit-bucket" {
                    let (id, revision) = self
                        .cloud
                        .form_target
                        .clone()
                        .ok_or_else(|| anyhow!("No bucket selected"))?;
                    params.extend([("id", id.into()), ("revision", revision.into())]);
                    "bucket.update"
                } else {
                    "bucket.create"
                };
                self.cloud_mutate(method, params);
            }
            "rename-folder" => {
                let (id, revision) = self
                    .cloud
                    .form_target
                    .clone()
                    .ok_or_else(|| anyhow!("No folder selected"))?;
                self.cloud_mutate(
                    "folder.update",
                    vec![
                        ("id", id.into()),
                        ("revision", revision.into()),
                        ("name", get("cloud-name").into()),
                    ],
                );
            }
            "delete-folder" | "delete-bucket" => {
                let (id, revision) = self
                    .cloud
                    .form_target
                    .clone()
                    .ok_or_else(|| anyhow!("No item selected"))?;
                self.cloud_mutate(
                    if kind == "delete-folder" {
                        "folder.delete"
                    } else {
                        "bucket.delete"
                    },
                    vec![("id", id.into()), ("revision", revision.into())],
                );
            }
            "upload-document" => {
                let folder = get("cloud-folder");
                self.cloud_upload_current((!folder.is_empty()).then_some(folder))?;
            }
            _ => return Err(anyhow!("Unknown cloud action")),
        }
        Ok(())
    }
}
fn mime(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "psd" | "psb" => "image/vnd.adobe.photoshop",
        _ => "application/octet-stream",
    }
}
fn enumerate_files(
    root: &std::path::Path,
    path: &std::path::Path,
    out: &mut Vec<(PathBuf, Option<String>)>,
) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_symlink() {
            continue;
        }
        if ty.is_dir() {
            enumerate_files(root, &entry.path(), out)?;
        } else if ty.is_file() {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            out.push((path, Some(relative)));
        }
    }
    Ok(())
}

fn parse_filters(fields: &[(&'static str, String, String)]) -> Result<Filters> {
    let get = |key: &str| {
        fields
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, _, v)| v.trim())
            .unwrap_or("")
    };
    let list = |key: &str| {
        let s = get(key);
        (!s.is_empty()).then(|| {
            s.split(',')
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect()
        })
    };
    let edited = match get("cloud-edited") {
        "any" | "" => None,
        "yes" => Some(true),
        "no" => Some(false),
        _ => return Err(anyhow!("Edited must be any, yes or no")),
    };
    let content = match get("cloud-content") {
        "all" | "" => None,
        "safe" => Some("safe".into()),
        "flagged" => Some("flagged".into()),
        _ => return Err(anyhow!("Content must be all, safe or flagged")),
    };
    let rating = get("cloud-rating");
    let min_rating = if rating.is_empty() {
        None
    } else {
        let r: u8 = rating.parse()?;
        anyhow::ensure!(r <= 5, "Rating must be between 0 and 5");
        Some(r)
    };
    let b = get("cloud-bounds");
    let bounds = if b.is_empty() {
        None
    } else {
        let c = b
            .split(',')
            .map(|s| s.trim().parse::<f64>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            c.len() == 4
                && c.iter().all(|v| v.is_finite())
                && c[0] >= -90.0
                && c[2] <= 90.0
                && c[0] <= c[2]
                && c[1].abs() <= 180.0
                && c[3].abs() <= 180.0,
            "Enter valid south, west, north, east coordinates"
        );
        Some(remote::Bounds {
            south: c[0],
            west: c[1],
            north: c[2],
            east: c[3],
        })
    };
    let captured_after = remote::parse_date(get("cloud-after"), false)?;
    let captured_before = remote::parse_date(get("cloud-before"), true)?;
    if let (Some(a), Some(b)) = (captured_after, captured_before) {
        anyhow::ensure!(a <= b, "End date must follow start date");
    }
    Ok(Filters {
        mime_types: list("cloud-types"),
        tags: list("cloud-tags"),
        edited,
        content,
        min_rating,
        bounds,
        captured_after,
        captured_before,
    })
}

pub(super) fn rgba_to_render_image(
    width: u32,
    height: u32,
    mut rgba: Vec<u8>,
) -> Option<Arc<RenderImage>> {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba)?;
    Some(Arc::new(RenderImage::new(smallvec![image::Frame::new(
        buffer
    )])))
}

#[cfg(test)]
mod cloud_lifecycle_tests {
    use super::*;
    fn binding(asset: &str) -> RemoteDocument {
        let mut source = Document::new("Original", 1, 1, schist_color::Depth::Eight);
        let mut shared = remote::document::SharedDocument::new(&source).unwrap();
        source.title = "Unsynced local edit".into();
        shared.local_changes(&source).unwrap();
        RemoteDocument {
            asset: Asset {
                id: asset.into(),
                folder_id: None,
                name: "Original".into(),
                mime_type: "image/png".into(),
                revision: 1,
                size: 1,
                edited: false,
                tags: vec![],
                rating: 0,
                captured_at: None,
                modified_at: 0,
                thumbnail_url: None,
            },
            shared,
            joined: true,
            detached: false,
            vector: vec![0],
            sending: true,
            changed: true,
            generation: 1,
            saved: 0,
            render: true,
        }
    }
    #[test]
    fn terminated_document_keeps_edits_ignores_late_events_and_requires_explicit_reopen() {
        let mut state = CloudState::default();
        let id = DocumentId(41);
        let other = DocumentId(42);
        state.docs.insert(id, binding("revoked"));
        state.docs.insert(other, binding("other"));
        let before = state.docs[&id].shared.full_state();
        state.pending.insert("late-join".into(), Pending::Join(id));
        state.pending.insert(
            "late-update".into(),
            Pending::Update {
                doc: id,
                vector: vec![0],
                generation: 1,
            },
        );
        state
            .pending
            .insert("other-join".into(), Pending::Join(other));
        assert_eq!(state.detach_document("revoked"), Some(id));
        assert!(!state.pending.contains_key("late-join"));
        assert!(!state.pending.contains_key("late-update"));
        assert!(state.pending.contains_key("other-join"));
        // Even a malformed late push is discarded before touching the detached CRDT.
        state.apply_document_update("revoked", &[255]).unwrap();
        state.disconnect();
        assert_eq!(state.joinable_documents(), vec![other]);
        let document = &state.docs[&id];
        assert_eq!(document.shared.full_state(), before);
        assert_eq!((document.generation, document.saved), (1, 0));
        assert!(!document.joined && !document.sending && !document.render);
        assert!(state.reopen_document(id));
        assert!(!state.reopen_document(id));
        assert!(state.joinable_documents().contains(&id));
        assert!(state.apply_document_update("revoked", &[255]).is_err());
    }
}
