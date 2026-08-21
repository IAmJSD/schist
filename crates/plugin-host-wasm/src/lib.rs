//! Sandboxed third-party plugin host (M9).
//!
//! Loads `wasm32-unknown-unknown` modules, reads their [`abi::Manifest`],
//! and wraps them as ordinary [`FilterPlugin`]/[`CodecPlugin`]
//! implementations — so the rest of the app can't tell a third-party plugin
//! from a built-in one.
//!
//! Isolation comes from what the sandbox *lacks*: the host supplies exactly
//! one import (`schist::log`), so a plugin has no filesystem, network,
//! clock or randomness. Execution is additionally bounded by fuel, so a
//! plugin that loops forever is unwound instead of hanging the editor.

pub mod abi;

use abi::{Capability, DecodedImage, Manifest, PluginKind};
use anyhow::{anyhow, Context as _, Result};
use schist_color::Depth;
use schist_core::{blit_rgba8, Document, IntRect, Layer};
use schist_plugin_api::{CodecPlugin, FilterParam, FilterPlugin, FilterValues, PluginRegistry};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Instructions a plugin may execute per call before it is unwound.
/// Generous for image work, still bounded.
const FUEL_PER_CALL: u64 = 20_000_000_000;

/// wasmtime carries its own error type; funnel it into `anyhow` at the
/// boundary so the rest of the host reads uniformly.
fn wasm_err(err: wasmtime::Error) -> anyhow::Error {
    anyhow!("{err}")
}

/// Largest buffer a plugin may hand back (guards a hostile decoder from
/// claiming a petabyte image).
const MAX_RETURN_BYTES: usize = 512 * 1024 * 1024;

struct HostState {
    plugin_name: String,
}

/// A loaded plugin: its manifest plus a ready-to-instantiate module.
pub struct LoadedPlugin {
    pub manifest: Manifest,
    pub path: PathBuf,
    engine: wasmtime::Engine,
    module: wasmtime::Module,
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("id", &self.manifest.id)
            .field("kind", &self.manifest.kind)
            .field("path", &self.path)
            .finish()
    }
}

/// One instantiation, used for a single call.
struct Instance {
    store: wasmtime::Store<HostState>,
    instance: wasmtime::Instance,
    memory: wasmtime::Memory,
}

impl Instance {
    fn call_alloc(&mut self, len: usize) -> Result<i32> {
        let f = self
            .instance
            .get_typed_func::<i32, i32>(&mut self.store, abi::EXPORT_ALLOC)
            .map_err(wasm_err)
            .context("plugin does not export schist_alloc")?;
        let ptr = f.call(&mut self.store, len as i32).map_err(wasm_err)?;
        if ptr <= 0 {
            return Err(anyhow!("plugin allocation of {len} bytes failed"));
        }
        Ok(ptr)
    }

    fn write(&mut self, ptr: i32, bytes: &[u8]) -> Result<()> {
        self.memory
            .write(&mut self.store, ptr as usize, bytes)
            .map_err(|e| anyhow!("{e}"))
            .context("writing into plugin memory")
    }

    fn read(&mut self, ptr: i32, len: i32) -> Result<Vec<u8>> {
        let len = len.max(0) as usize;
        if len > MAX_RETURN_BYTES {
            return Err(anyhow!("plugin returned an implausible {len} bytes"));
        }
        let mut out = vec![0u8; len];
        self.memory
            .read(&mut self.store, ptr as usize, &mut out)
            .map_err(|e| anyhow!("{e}"))
            .context("reading plugin memory")?;
        Ok(out)
    }
}

impl LoadedPlugin {
    /// Compile a plugin and read its manifest. Fails closed: a module that
    /// doesn't declare a matching ABI version, or whose manifest doesn't
    /// parse, is refused.
    pub fn load(path: &Path) -> Result<LoadedPlugin> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {path:?}"))?;
        Self::from_bytes(&bytes, path.to_path_buf())
    }

    pub fn from_bytes(bytes: &[u8], path: PathBuf) -> Result<LoadedPlugin> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        // No threads, no SIMD-dependent host features, no cranelift debug.
        let engine = wasmtime::Engine::new(&config)
            .map_err(wasm_err)
            .context("creating wasm engine")?;
        let module = wasmtime::Module::new(&engine, bytes)
            .map_err(wasm_err)
            .context("compiling plugin module")?;

        let plugin = LoadedPlugin {
            manifest: Manifest {
                id: String::new(),
                name: String::new(),
                kind: PluginKind::Filter,
                api_version: 0,
                description: String::new(),
                category: String::new(),
                params: Vec::new(),
                extensions: Vec::new(),
                capabilities: Vec::new(),
            },
            path,
            engine,
            module,
        };

        let mut instance = plugin.instantiate("<loading>")?;
        let version = instance
            .instance
            .get_typed_func::<(), i32>(&mut instance.store, abi::EXPORT_ABI_VERSION)
            .map_err(wasm_err)
            .context("plugin does not export schist_abi_version")?
            .call(&mut instance.store, ())
            .map_err(wasm_err)?;
        if version != abi::ABI_VERSION {
            return Err(anyhow!(
                "plugin targets ABI version {version}, this build speaks {}",
                abi::ABI_VERSION
            ));
        }
        let packed = instance
            .instance
            .get_typed_func::<(), i64>(&mut instance.store, abi::EXPORT_MANIFEST)
            .map_err(wasm_err)
            .context("plugin does not export schist_manifest")?
            .call(&mut instance.store, ())
            .map_err(wasm_err)?;
        let (ptr, len) = abi::unpack(packed);
        let json = instance.read(ptr, len)?;
        let manifest: Manifest =
            serde_json::from_slice(&json).context("plugin manifest is not valid JSON")?;
        if manifest.api_version != abi::ABI_VERSION {
            return Err(anyhow!(
                "manifest declares api_version {}, expected {}",
                manifest.api_version,
                abi::ABI_VERSION
            ));
        }
        if manifest.id.trim().is_empty() {
            return Err(anyhow!("plugin manifest has an empty id"));
        }
        drop(instance);

        Ok(LoadedPlugin { manifest, ..plugin })
    }

    /// Capabilities this plugin asked for, for the manager UI to show.
    pub fn requested_capabilities(&self) -> &[Capability] {
        &self.manifest.capabilities
    }

    fn instantiate(&self, name: &str) -> Result<Instance> {
        let mut store = wasmtime::Store::new(
            &self.engine,
            HostState {
                plugin_name: name.to_string(),
            },
        );
        store.set_fuel(FUEL_PER_CALL).map_err(wasm_err)?;
        let mut linker = wasmtime::Linker::new(&self.engine);
        // The entire host surface: one logging call.
        linker
            .func_wrap(
                "schist",
                "log",
                |mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32| {
                    let Some(wasmtime::Extern::Memory(memory)) = caller.get_export("memory") else {
                        return;
                    };
                    let len = (len.max(0) as usize).min(4096);
                    let mut buf = vec![0u8; len];
                    if memory.read(&mut caller, ptr as usize, &mut buf).is_ok() {
                        log::info!(
                            "[plugin {}] {}",
                            caller.data().plugin_name,
                            String::from_utf8_lossy(&buf)
                        );
                    }
                },
            )
            .map_err(wasm_err)?;
        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(wasm_err)
            .context("instantiating plugin (unresolved imports are refused)")?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("plugin does not export its memory"))?;
        Ok(Instance {
            store,
            instance,
            memory,
        })
    }

    /// Run a filter plugin over a straight-alpha f32 RGBA buffer.
    pub fn run_filter(
        &self,
        pixels: &mut [f32],
        width: usize,
        height: usize,
        values: &FilterValues,
    ) -> Result<()> {
        if width == 0 || height == 0 || pixels.is_empty() {
            return Ok(());
        }
        let mut inst = self.instantiate(&self.manifest.name)?;

        let bytes: Vec<u8> = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
        let pixel_ptr = inst.call_alloc(bytes.len())?;
        inst.write(pixel_ptr, &bytes)?;

        // Parameters travel as JSON: simple, versionable, and it keeps the
        // ABI from growing a type for every future control.
        let params: serde_json::Map<String, serde_json::Value> = values
            .0
            .iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::Value::from(f64::from(*v))))
            .collect();
        let params_json = serde_json::to_vec(&params)?;
        let params_ptr = inst.call_alloc(params_json.len())?;
        inst.write(params_ptr, &params_json)?;

        inst.instance
            .get_typed_func::<(i32, i32, i32, i32, i32), ()>(
                &mut inst.store,
                abi::EXPORT_FILTER_APPLY,
            )
            .map_err(wasm_err)
            .context("filter plugin does not export schist_filter_apply")?
            .call(
                &mut inst.store,
                (
                    pixel_ptr,
                    width as i32,
                    height as i32,
                    params_ptr,
                    params_json.len() as i32,
                ),
            )
            .map_err(wasm_err)
            .context("plugin trapped or ran out of fuel")?;

        let out = inst.read(pixel_ptr, bytes.len() as i32)?;
        for (dst, chunk) in pixels.iter_mut().zip(out.chunks_exact(4)) {
            let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            // A plugin returning NaN would poison the document.
            *dst = if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            };
        }
        Ok(())
    }

    /// Ask a codec plugin whether it recognises these bytes.
    pub fn probe(&self, bytes: &[u8]) -> Result<bool> {
        let mut inst = self.instantiate(&self.manifest.name)?;
        let ptr = inst.call_alloc(bytes.len().max(1))?;
        inst.write(ptr, bytes)?;
        let verdict = inst
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut inst.store, abi::EXPORT_CODEC_PROBE)
            .map_err(wasm_err)
            .context("codec plugin does not export schist_codec_probe")?
            .call(&mut inst.store, (ptr, bytes.len() as i32))
            .map_err(wasm_err)?;
        Ok(verdict != 0)
    }

    /// Decode an image through a codec plugin.
    pub fn decode(&self, bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
        let mut inst = self.instantiate(&self.manifest.name)?;
        let ptr = inst.call_alloc(bytes.len().max(1))?;
        inst.write(ptr, bytes)?;
        let packed = inst
            .instance
            .get_typed_func::<(i32, i32), i64>(&mut inst.store, abi::EXPORT_CODEC_DECODE)
            .map_err(wasm_err)
            .context("codec plugin does not export schist_codec_decode")?
            .call(&mut inst.store, (ptr, bytes.len() as i32))
            .map_err(wasm_err)
            .context("plugin trapped or ran out of fuel")?;
        let (out_ptr, out_len) = abi::unpack(packed);
        if out_ptr == 0 || out_len <= 0 {
            return Err(anyhow!("plugin could not decode the image"));
        }
        let blob = inst.read(out_ptr, out_len)?;
        // Layout: 4-byte header length, JSON header, then RGBA8 pixels.
        if blob.len() < 4 {
            return Err(anyhow!("decoded blob is too short"));
        }
        let header_len = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
        let header_end = 4usize
            .checked_add(header_len)
            .filter(|end| *end <= blob.len())
            .ok_or_else(|| anyhow!("decoded header length is out of range"))?;
        let header: DecodedImage = serde_json::from_slice(&blob[4..header_end])
            .context("decoded header is not valid JSON")?;
        let expected = (header.width as usize)
            .checked_mul(header.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| anyhow!("decoded dimensions overflow"))?;
        let pixels = &blob[header_end..];
        if header.width == 0 || header.height == 0 || pixels.len() < expected {
            return Err(anyhow!(
                "decoded image claims {}x{} but carries {} bytes",
                header.width,
                header.height,
                pixels.len()
            ));
        }
        Ok((header.width, header.height, pixels[..expected].to_vec()))
    }
}

/// Adapts a WASM filter to the internal [`FilterPlugin`] trait.
pub struct WasmFilter {
    plugin: LoadedPlugin,
    // `FilterPlugin` hands out `&'static str`; plugin metadata is owned, so
    // it is leaked once at load time (bounded by the number of plugins).
    id: &'static str,
    name: &'static str,
    category: &'static str,
    params: Vec<FilterParam>,
}

impl WasmFilter {
    pub fn new(plugin: LoadedPlugin) -> WasmFilter {
        let id: &'static str = Box::leak(plugin.manifest.id.clone().into_boxed_str());
        let name: &'static str = Box::leak(plugin.manifest.name.clone().into_boxed_str());
        let category: &'static str = Box::leak(
            if plugin.manifest.category.is_empty() {
                "Plugins".to_string()
            } else {
                plugin.manifest.category.clone()
            }
            .into_boxed_str(),
        );
        let params = plugin
            .manifest
            .params
            .iter()
            .map(|p| FilterParam {
                key: Box::leak(p.key.clone().into_boxed_str()),
                label: Box::leak(p.label.clone().into_boxed_str()),
                min: p.min,
                max: p.max,
                default: p.default,
                suffix: Box::leak(p.suffix.clone().into_boxed_str()),
                // The wasm ABI has no way to declare a list of names yet,
                // so a plugin's enumerated parameter shows as a number.
                choices: &[],
            })
            .collect();
        WasmFilter {
            plugin,
            id,
            name,
            category,
            params,
        }
    }
}

impl FilterPlugin for WasmFilter {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn category(&self) -> &'static str {
        self.category
    }
    fn params(&self) -> Vec<FilterParam> {
        self.params.clone()
    }
    fn apply(&self, pixels: &mut [f32], width: usize, height: usize, values: &FilterValues) {
        // A misbehaving plugin must not take the editor down with it.
        if let Err(err) = self.plugin.run_filter(pixels, width, height, values) {
            log::error!("plugin {} failed: {err:#}", self.plugin.manifest.id);
        }
    }
}

/// Adapts a WASM codec to the internal [`CodecPlugin`] trait.
pub struct WasmCodec {
    plugin: LoadedPlugin,
    id: &'static str,
    name: &'static str,
    extensions: &'static [&'static str],
}

impl WasmCodec {
    pub fn new(plugin: LoadedPlugin) -> WasmCodec {
        let id: &'static str = Box::leak(plugin.manifest.id.clone().into_boxed_str());
        let name: &'static str = Box::leak(plugin.manifest.name.clone().into_boxed_str());
        let exts: Vec<&'static str> = plugin
            .manifest
            .extensions
            .iter()
            .map(|e| &*Box::leak(e.to_ascii_lowercase().into_boxed_str()))
            .collect();
        WasmCodec {
            plugin,
            id,
            name,
            extensions: Box::leak(exts.into_boxed_slice()),
        }
    }
}

impl CodecPlugin for WasmCodec {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn extensions(&self) -> &'static [&'static str] {
        self.extensions
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        self.plugin.probe(bytes).unwrap_or(false)
    }
    fn import(&self, bytes: &[u8]) -> anyhow::Result<Document> {
        let (width, height, rgba) = self.plugin.decode(bytes)?;
        let mut doc = Document::new(self.name, width, height, Depth::Eight);
        let mut layer = Layer::new_raster("Background");
        blit_rgba8(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Eight,
            IntRect::from_size(width, height),
            &rgba,
        );
        doc.push_layer(layer);
        doc.dirty = false;
        Ok(doc)
    }
}

/// A plugin the manager knows about, whether or not it loaded.
#[derive(Debug)]
pub struct PluginEntry {
    pub path: PathBuf,
    pub id: String,
    pub name: String,
    pub kind: Option<PluginKind>,
    pub enabled: bool,
    /// Why it failed to load, if it did.
    pub error: Option<String>,
}

/// Scans a plugin directory and owns the enable/disable state.
#[derive(Debug, Default)]
pub struct PluginManager {
    pub entries: Vec<PluginEntry>,
    disabled: Mutex<Vec<String>>,
}

impl PluginManager {
    /// Where third-party plugins live.
    pub fn plugin_dir() -> Option<PathBuf> {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            })?;
        Some(base.join("schist/plugins"))
    }

    /// Load every `.wasm` in `dir`, registering the ones that pass
    /// validation and recording the ones that don't.
    pub fn load_dir(dir: &Path, registry: &mut PluginRegistry) -> PluginManager {
        let mut manager = PluginManager::default();
        let disabled = read_disabled_list(dir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            log::debug!("no plugin directory at {dir:?}");
            return manager;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("wasm"))
            .collect();
        paths.sort();
        for path in paths {
            match LoadedPlugin::load(&path) {
                Ok(plugin) => {
                    let id = plugin.manifest.id.clone();
                    let enabled = !disabled.contains(&id);
                    manager.entries.push(PluginEntry {
                        path: path.clone(),
                        id: id.clone(),
                        name: plugin.manifest.name.clone(),
                        kind: Some(plugin.manifest.kind.clone()),
                        enabled,
                        error: None,
                    });
                    if !enabled {
                        log::info!("plugin {id} is disabled; skipping");
                        continue;
                    }
                    log::info!("loaded plugin {id} from {path:?}");
                    match plugin.manifest.kind {
                        PluginKind::Filter => {
                            registry.register_filter(Box::new(WasmFilter::new(plugin)))
                        }
                        PluginKind::Codec => {
                            registry.register_codec(Box::new(WasmCodec::new(plugin)))
                        }
                    }
                }
                Err(err) => {
                    log::warn!("refusing plugin {path:?}: {err:#}");
                    manager.entries.push(PluginEntry {
                        path: path.clone(),
                        id: path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        name: path
                            .file_stem()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        kind: None,
                        enabled: false,
                        error: Some(format!("{err:#}")),
                    });
                }
            }
        }
        *manager.disabled.lock().unwrap() = disabled;
        manager
    }

    /// Toggle a plugin. Takes effect on the next launch, which the UI says.
    pub fn set_enabled(&mut self, id: &str, enabled: bool, dir: &Path) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.enabled = enabled;
        }
        let mut disabled = self.disabled.lock().unwrap();
        disabled.retain(|d| d != id);
        if !enabled {
            disabled.push(id.to_string());
        }
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(dir.join("disabled.txt"), disabled.join("\n"));
    }

    /// Copy a plugin file into the plugin directory.
    pub fn install(source: &Path, dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let name = source
            .file_name()
            .ok_or_else(|| anyhow!("source has no file name"))?;
        // Validate before installing: a file that won't load never lands
        // in the plugin directory.
        LoadedPlugin::load(source)?;
        let dest = dir.join(name);
        std::fs::copy(source, &dest)?;
        Ok(dest)
    }
}

fn read_disabled_list(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("disabled.txt"))
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
