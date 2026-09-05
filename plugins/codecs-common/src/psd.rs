use schist_plugin_api::{CodecPlugin, PluginManifest, PluginRegistry};

/// PSD/PSB import and export via `schist-codec-psd`.
pub struct PsdCodec;

impl CodecPlugin for PsdCodec {
    fn id(&self) -> &'static str {
        "codec.psd"
    }
    fn name(&self) -> &'static str {
        "Photoshop PSD"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["psd", "psb"]
    }
    fn probe(&self, bytes: &[u8]) -> bool {
        schist_codec_psd::is_psd(bytes)
    }
    fn import(&self, bytes: &[u8]) -> anyhow::Result<schist_core::Document> {
        Ok(schist_codec_psd::read_psd(bytes)?)
    }
    fn can_export(&self) -> bool {
        true
    }
    fn export(&self, doc: &schist_core::Document) -> anyhow::Result<Vec<u8>> {
        Ok(schist_codec_psd::write_psd(doc)?)
    }
}

pub struct PsdPlugin;

impl PluginManifest for PsdPlugin {
    fn id(&self) -> &'static str {
        "schist.codec-psd"
    }
    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_codec(Box::new(PsdCodec));
    }
}
