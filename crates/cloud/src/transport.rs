use crate::{auth, protocol::*};
use anyhow::{anyhow, ensure, Result};
use futures::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc as channel;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

type Reply = std::result::Result<Value, String>;
#[derive(Clone)]
pub struct Handle {
    tx: channel::UnboundedSender<Command>,
}
pub struct Client {
    pub handle: Handle,
    pub events: mpsc::Receiver<Event>,
}
pub struct Upload<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
    pub mime: &'a str,
    pub folder: Option<&'a str>,
    pub asset: Option<(&'a str, u64)>,
    pub relative: Option<&'a str>,
    pub mutation: &'a str,
}
pub enum Event {
    Connected,
    Disconnected(String),
    Credentials(Account),
    Snapshot {
        subscription_id: String,
        snapshot: Snapshot,
    },
    WatchError {
        subscription_id: String,
        error: String,
    },
    Reply {
        id: String,
        result: Reply,
    },
    DocumentUpdate {
        asset_id: String,
        bytes: Vec<u8>,
    },
    DocumentError {
        asset_id: String,
        error: String,
    },
}
struct Pending {
    started: Instant,
    reply: Option<mpsc::Sender<Reply>>,
}
enum Command {
    FrameLimit(usize),
    Watch(String, WatchQuery),
    Unwatch(String),
    Request {
        id: String,
        method: String,
        params: Value,
        reply: Option<mpsc::Sender<Reply>>,
    },
    Stop,
}
impl Client {
    pub fn start(account: Account) -> Self {
        let (tx, rx) = channel::unbounded_channel();
        let (events, out) = mpsc::channel();
        std::thread::spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(run(account, rx, events)),
                Err(e) => {
                    let _ = events.send(Event::Disconnected(e.to_string()));
                }
            }
        });
        Self {
            handle: Handle { tx },
            events: out,
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.handle.tx.send(Command::Stop);
    }
}
impl Handle {
    pub fn set_frame_limit(&self, limit: usize) {
        let _ = self.tx.send(Command::FrameLimit(limit.min(MAX_FRAME)));
    }
    pub fn watch(&self, id: &str, query: WatchQuery) {
        let _ = self.tx.send(Command::Watch(id.into(), query));
    }
    pub fn unwatch(&self, id: &str) {
        let _ = self.tx.send(Command::Unwatch(id.into()));
    }
    pub fn call(&self, method: &str, params: Value) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let _ = self.tx.send(Command::Request {
            id: id.clone(),
            method: method.into(),
            params,
            reply: None,
        });
        id
    }
    /// For HTTP transfer workers, never the UI thread. Ambiguous mutations are not replayed.
    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Command::Request {
            id: uuid::Uuid::new_v4().to_string(),
            method: method.into(),
            params,
            reply: Some(tx),
        })?;
        rx.recv_timeout(Duration::from_secs(35))?
            .map_err(|e| anyhow!(e))
    }
    pub fn upload(&self, upload: Upload<'_>) -> Result<Asset> {
        let Upload {
            name,
            bytes,
            mime,
            folder,
            asset,
            relative,
            mutation,
        } = upload;
        if let Some(path) = relative {
            ensure!(
                !path.contains(['\\', '\0', ':'])
                    && path
                        .split('/')
                        .all(|p| !p.is_empty() && p != "." && p != ".."),
                "Invalid relative import path"
            );
        }
        let mut fields = vec![
            ("name", name.into()),
            ("mime_type", mime.into()),
            ("size", (bytes.len() as u64).into()),
            ("mutation_id", format!("{mutation}:prepare").into()),
        ];
        if let Some(folder) = folder {
            fields.push(("folder_id", folder.into()));
        }
        if let Some((id, revision)) = asset {
            fields.push(("asset_id", id.into()));
            fields.push(("revision", revision.into()));
        }
        if let Some(path) = relative {
            fields.push(("relative_path", path.into()));
        }
        let ticket = self.request("asset.prepare_upload", map(fields))?;
        auth::upload(&string(&ticket, "put_url")?, mime, bytes)?;
        parse(self.request(
            "asset.commit_upload",
            map([
                ("upload_id", string(&ticket, "upload_id")?.into()),
                ("mutation_id", format!("{mutation}:commit").into()),
            ]),
        )?)
    }
}
fn finish(events: &mpsc::Sender<Event>, id: String, p: Pending, result: Reply) {
    if let Some(reply) = p.reply {
        let _ = reply.send(result);
    } else {
        let _ = events.send(Event::Reply { id, result });
    }
}
fn offline(
    cmd: Command,
    watches: &mut HashMap<String, WatchQuery>,
    events: &mpsc::Sender<Event>,
) -> bool {
    match cmd {
        Command::FrameLimit(_) => {}
        Command::Watch(id, q) => {
            watches.insert(id, q);
        }
        Command::Unwatch(id) => {
            watches.remove(&id);
        }
        Command::Request { id, reply, .. } => finish(
            events,
            id,
            Pending {
                started: Instant::now(),
                reply,
            },
            Err("Cloud is disconnected".into()),
        ),
        Command::Stop => return false,
    }
    true
}
async fn run(
    mut account: Account,
    mut commands: channel::UnboundedReceiver<Command>,
    events: mpsc::Sender<Event>,
) {
    let mut watches = HashMap::new();
    let mut retry = 0u32;
    let mut force_refresh = false;
    loop {
        while let Ok(cmd) = commands.try_recv() {
            if !offline(cmd, &mut watches, &events) {
                return;
            }
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        if force_refresh || account.credentials.expires_at <= now + 30.0 {
            let copy = account.clone();
            match tokio::task::spawn_blocking(move || auth::refresh(&copy)).await {
                Ok(Ok(c)) => {
                    account.credentials = c;
                    force_refresh = false;
                    let _ = events.send(Event::Credentials(account.clone()));
                }
                result => {
                    let error = match result {
                        Ok(Err(e)) => e.to_string(),
                        Err(e) => e.to_string(),
                        _ => unreachable!(),
                    };
                    let _ = events.send(Event::Disconnected(format!(
                        "Token refresh failed: {error}"
                    )));
                    if !backoff(&mut commands, &mut watches, &events, &mut retry).await {
                        return;
                    }
                    continue;
                }
            }
        }
        match connected(&account, &mut commands, &mut watches, &events, &mut retry).await {
            Ok(Outcome::Stop) => return,
            Ok(Outcome::Refresh) => {
                force_refresh = true;
                let _ = events.send(Event::Disconnected("Renewing cloud login…".into()));
            }
            Err(e) => {
                let _ = events.send(Event::Disconnected(e.to_string()));
            }
        }
        if !backoff(&mut commands, &mut watches, &events, &mut retry).await {
            return;
        }
    }
}
async fn backoff(
    commands: &mut channel::UnboundedReceiver<Command>,
    watches: &mut HashMap<String, WatchQuery>,
    events: &mpsc::Sender<Event>,
    retry: &mut u32,
) -> bool {
    let jitter = uuid::Uuid::new_v4().as_bytes()[0] as u64;
    let delay = Duration::from_millis((500u64 << (*retry).min(6)).min(30_000) + jitter);
    *retry = retry.saturating_add(1);
    let wait = tokio::time::sleep(delay);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            _ = &mut wait => return true,
            command = commands.recv() => {
                let Some(command) = command else { return false };
                if !offline(command, watches, events) {
                    return false;
                }
            }
        }
    }
}
enum Outcome {
    Stop,
    Refresh,
}
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
struct Session<'a> {
    frame_limit: usize,
    socket: Socket,
    watches: &'a mut HashMap<String, WatchQuery>,
    events: &'a mpsc::Sender<Event>,
    pending: HashMap<String, Pending>,
    revisions: HashMap<String, u64>,
    ready: bool,
    started: Instant,
    received: Instant,
    ping: Instant,
}
async fn connected(
    account: &Account,
    commands: &mut channel::UnboundedReceiver<Command>,
    watches: &mut HashMap<String, WatchQuery>,
    events: &mpsc::Sender<Event>,
    retry: &mut u32,
) -> Result<Outcome> {
    let url = account
        .credentials
        .workspace_websocket_url
        .as_deref()
        .ok_or_else(|| anyhow!("No workspace endpoint"))?;
    #[cfg(not(test))]
    auth::secure_url(url, "wss")?;
    // Unit tests use a loopback peer; production builds always require TLS.
    #[cfg(test)]
    if !url.starts_with("ws://127.0.0.1:") {
        auth::secure_url(url, "wss")?;
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(15),
        connect_async_with_config(url, Some(config), true),
    )
    .await??;
    let now = Instant::now();
    let mut session = Session {
        frame_limit: MAX_FRAME,
        socket,
        watches,
        events,
        pending: HashMap::new(),
        revisions: HashMap::new(),
        ready: false,
        started: now,
        received: now,
        ping: now,
    };
    session
        .send(map([
            ("type", "hello".into()),
            ("protocol", 1.into()),
            (
                "access_token",
                account.credentials.access_token.clone().into(),
            ),
        ]))
        .await?;
    let result = session.run(commands, retry).await;
    // No mutation is automatically retried when its acknowledgement is lost.
    for (id, pending) in session.pending.drain() {
        finish(
            events,
            id,
            pending,
            Err("Cloud disconnected; mutation outcome may be unknown".into()),
        );
    }
    result
}
impl Session<'_> {
    async fn send(&mut self, value: Value) -> Result<()> {
        let bytes = encode_with_limit(&value, self.frame_limit)?;
        tokio::time::timeout(
            Duration::from_secs(10),
            self.socket.send(Message::Binary(bytes.into())),
        )
        .await??;
        Ok(())
    }
    async fn run(
        &mut self,
        commands: &mut channel::UnboundedReceiver<Command>,
        retry: &mut u32,
    ) -> Result<Outcome> {
        enum Wake {
            Command(Option<Command>),
            Wire(Option<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>),
            Tick,
        }
        let mut timer = tokio::time::interval(Duration::from_millis(100));
        loop {
            let wake = tokio::select! {
                command = commands.recv() => Wake::Command(command),
                message = self.socket.next() => Wake::Wire(message),
                _ = timer.tick() => Wake::Tick,
            };
            let outcome = match wake {
                Wake::Command(command) => self.command(command).await?,
                Wake::Wire(message) => {
                    self.message(message.ok_or_else(|| anyhow!("Cloud connection closed"))??)
                        .await?
                }
                Wake::Tick => {
                    self.tick().await?;
                    None
                }
            };
            if self.ready {
                *retry = 0;
            }
            if let Some(outcome) = outcome {
                return Ok(outcome);
            }
        }
    }
    async fn command(&mut self, command: Option<Command>) -> Result<Option<Outcome>> {
        match command {
            Some(Command::FrameLimit(limit)) => self.frame_limit = limit,
            None | Some(Command::Stop) => {
                let _ = self.socket.close(None).await;
                return Ok(Some(Outcome::Stop));
            }
            Some(Command::Watch(id, query)) => {
                self.watches.insert(id.clone(), query.clone());
                self.revisions.remove(&id);
                if self.ready {
                    self.subscribe(&id, &query).await?;
                }
            }
            Some(Command::Unwatch(id)) => {
                self.watches.remove(&id);
                self.revisions.remove(&id);
                if self.ready {
                    self.send(map([
                        ("type", "unsubscribe".into()),
                        ("subscription_id", id.into()),
                    ]))
                    .await?;
                }
            }
            Some(Command::Request {
                id,
                method,
                params,
                reply,
            }) => {
                let pending = Pending {
                    started: Instant::now(),
                    reply,
                };
                if !self.ready {
                    finish(self.events, id, pending, Err("Cloud is connecting".into()));
                    return Ok(None);
                }
                let value = map([
                    ("type", "request".into()),
                    ("id", id.clone().into()),
                    ("method", method.into()),
                    ("params", params),
                ]);
                if let Err(error) = encode_with_limit(&value, self.frame_limit) {
                    finish(self.events, id, pending, Err(error.to_string()));
                    return Ok(None);
                }
                self.pending.insert(id, pending);
                self.send(value).await?;
            }
        }
        Ok(None)
    }
    async fn subscribe(&mut self, id: &str, query: &WatchQuery) -> Result<()> {
        self.send(map([
            ("type", "subscribe".into()),
            ("subscription_id", id.into()),
            ("query", value(query)),
        ]))
        .await
    }
    async fn message(&mut self, message: Message) -> Result<Option<Outcome>> {
        let bytes = match message {
            Message::Binary(bytes) => bytes,
            Message::Ping(_) => {
                self.socket.flush().await?;
                return Ok(None);
            }
            Message::Pong(_) => {
                self.received = Instant::now();
                return Ok(None);
            }
            Message::Close(frame) => {
                if frame.is_some_and(|f| u16::from(f.code) == 4401) {
                    return Ok(Some(Outcome::Refresh));
                }
                return Err(anyhow!("Cloud connection closed"));
            }
            _ => return Err(anyhow!("Cloud requires binary MessagePack frames")),
        };
        ensure!(
            bytes.len() <= self.frame_limit,
            "Incoming workspace frame exceeds the negotiated limit"
        );
        self.received = Instant::now();
        let message = decode(&bytes)?;
        if !self.ready {
            ensure!(
                matches!(message, ServerMessage::Ready { protocol: 1 }),
                "Expected cloud ready protocol 1"
            );
            self.ready = true;
            for (id, query) in self.watches.clone() {
                self.subscribe(&id, &query).await?;
            }
            let _ = self.events.send(Event::Connected);
            return Ok(None);
        }
        match message {
            ServerMessage::Ready { .. } => return Err(anyhow!("Duplicate cloud handshake")),
            ServerMessage::Result { id, value } => self.reply(id, Ok(value)),
            ServerMessage::Error { id, error } => {
                self.reply(id, Err(format!("{}: {}", error.code, error.message)))
            }
            ServerMessage::Snapshot {
                subscription_id,
                snapshot,
            } => self.snapshot(subscription_id, snapshot)?,
            ServerMessage::WatchError {
                subscription_id,
                error,
            } => {
                self.watches.remove(&subscription_id);
                let _ = self.events.send(Event::WatchError {
                    subscription_id,
                    error: error.message,
                });
            }
            ServerMessage::DocumentUpdate {
                document_id,
                update,
            } => {
                let _ = self.events.send(Event::DocumentUpdate {
                    asset_id: document_id,
                    bytes: update,
                });
            }
            ServerMessage::DocumentError { document_id, error } => {
                let _ = self.events.send(Event::DocumentError {
                    asset_id: document_id,
                    error: error.message,
                });
            }
            ServerMessage::AuthExpiring => return Ok(Some(Outcome::Refresh)),
            ServerMessage::Pong => {}
        }
        Ok(None)
    }
    fn reply(&mut self, id: String, result: Reply) {
        if let Some(pending) = self.pending.remove(&id) {
            finish(self.events, id, pending, result);
        }
    }
    fn snapshot(&mut self, id: String, snapshot: Snapshot) -> Result<()> {
        let Some(query) = self.watches.get(&id) else {
            return Ok(());
        };
        if self
            .revisions
            .get(&id)
            .is_some_and(|rev| *rev >= snapshot.revision)
        {
            return Ok(());
        }
        let expected = match query {
            WatchQuery::Folders { .. } => "folders",
            WatchQuery::Buckets { .. } => "buckets",
            WatchQuery::Assets { .. } => "assets",
        };
        ensure!(
            snapshot.kind == expected && snapshot.items.len() <= 500,
            "Invalid subscription snapshot"
        );
        self.revisions.insert(id.clone(), snapshot.revision);
        let _ = self.events.send(Event::Snapshot {
            subscription_id: id,
            snapshot,
        });
        Ok(())
    }
    async fn tick(&mut self) -> Result<()> {
        ensure!(
            self.ready || self.started.elapsed() < Duration::from_secs(15),
            "Cloud handshake timed out"
        );
        ensure!(
            self.received.elapsed() < Duration::from_secs(60),
            "Cloud heartbeat timed out"
        );
        if self.ready && self.ping.elapsed() > Duration::from_secs(20) {
            self.send(map([("type", "ping".into())])).await?;
            self.ping = Instant::now();
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.started.elapsed() > Duration::from_secs(30))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.reply(
                id,
                Err("Cloud request timed out; mutation outcome may be unknown".into()),
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use tokio_tungstenite::tungstenite::{accept, WebSocket};
    fn read(ws: &mut WebSocket<std::net::TcpStream>) -> Value {
        match ws.read().unwrap() {
            Message::Binary(b) => rmp_serde::from_slice(&b).unwrap(),
            other => panic!("expected binary, got {other:?}"),
        }
    }
    fn send(ws: &mut WebSocket<std::net::TcpStream>, v: Value) {
        ws.send(Message::Binary(encode(&v).unwrap().into()))
            .unwrap();
    }
    fn account(url: String) -> Account {
        Account {
            domain: "https://schist.app".into(),
            exchange_url: "https://schist.app/exchange".into(),
            credentials: Credentials {
                access_token: "test".into(),
                refresh_token: "refresh".into(),
                expires_at: 9_999_999_999.0,
                generation_endpoint_url: "https://schist.app/generate".into(),
                logout_url: "https://schist.app/logout".into(),
                workspace_websocket_url: Some(url),
            },
        }
    }
    #[test]
    fn one_socket_multiplexes_and_resubscribes_without_replaying_mutations() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for connection in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut ws = accept(stream).unwrap();
                assert_eq!(string(&read(&mut ws), "type").unwrap(), "hello");
                send(
                    &mut ws,
                    map([("type", "ready".into()), ("protocol", 1.into())]),
                );
                let mut subscriptions = 0;
                while subscriptions < 2 {
                    let m = read(&mut ws);
                    assert_eq!(string(&m, "type").unwrap(), "subscribe");
                    let id = string(&m, "subscription_id").unwrap();
                    let kind = string(field(&m, "query").unwrap(), "kind").unwrap();
                    let q = field(field(&m, "query").unwrap(), "query").unwrap();
                    assert!(q.as_map().is_some());
                    send(
                        &mut ws,
                        map([
                            ("type", "snapshot".into()),
                            ("subscription_id", id.into()),
                            (
                                "snapshot",
                                map([
                                    ("kind", kind.into()),
                                    ("revision", 1.into()),
                                    ("total", 0.into()),
                                    ("offset", 0.into()),
                                    ("items", Value::Array(vec![])),
                                ]),
                            ),
                        ]),
                    );
                    subscriptions += 1;
                }
                if connection == 0 {
                    let m = read(&mut ws);
                    assert_eq!(string(&m, "method").unwrap(), "bucket.create"); /* Drop before ack: must not replay. */
                } else {
                    assert!(matches!(ws.read(), Ok(Message::Close(_))));
                }
            }
        });
        let client = Client::start(account(url));
        client.handle.watch(
            "folders",
            WatchQuery::Folders {
                query: Default::default(),
            },
        );
        client.handle.watch(
            "buckets",
            WatchQuery::Buckets {
                query: Default::default(),
            },
        );
        let mut snapshots = 0;
        while snapshots < 2 {
            if matches!(
                client.events.recv_timeout(Duration::from_secs(5)).unwrap(),
                Event::Snapshot { .. }
            ) {
                snapshots += 1;
            }
        }
        let mutation = client.handle.call(
            "bucket.create",
            map([("name", "New".into()), ("mutation_id", "stable".into())]),
        );
        let mut failed = false;
        while snapshots < 4 {
            match client.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Snapshot { .. } => snapshots += 1,
                Event::Reply { id, result } if id == mutation => {
                    assert!(result.is_err());
                    failed = true;
                }
                _ => {}
            }
        }
        assert!(failed);
        drop(client);
        server.join().unwrap();
    }
    #[test]
    fn advertised_frame_limit_rejects_oversize_request_without_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut ws = accept(stream).unwrap();
            assert_eq!(string(&read(&mut ws), "type").unwrap(), "hello");
            send(
                &mut ws,
                map([("type", "ready".into()), ("protocol", 1.into())]),
            );
            let request = read(&mut ws);
            assert_eq!(string(&request, "method").unwrap(), "small");
            send(
                &mut ws,
                map([
                    ("type", "result".into()),
                    ("id", string(&request, "id").unwrap().into()),
                    ("value", Value::Nil),
                ]),
            );
            assert!(matches!(ws.read(), Ok(Message::Close(_))));
        });
        let client = Client::start(account(url));
        assert!(matches!(
            client.events.recv_timeout(Duration::from_secs(5)).unwrap(),
            Event::Connected
        ));
        client.handle.set_frame_limit(128);
        let large = client
            .handle
            .call("large", map([("data", Value::Binary(vec![0; 128]))]));
        match client.events.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Reply { id, result } => {
                assert_eq!(id, large);
                assert!(result.unwrap_err().contains("128 byte"));
            }
            _ => panic!("Expected local size rejection"),
        }
        assert_eq!(client.handle.request("small", map([])).unwrap(), Value::Nil);
        drop(client);
        server.join().unwrap();
    }
}
