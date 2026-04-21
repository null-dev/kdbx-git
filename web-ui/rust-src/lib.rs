#[derive(Clone, Copy)]
pub struct EmbeddedAsset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/generated_assets.rs"));

pub fn get_asset(path: &str) -> Option<EmbeddedAsset> {
    generated_asset(path)
}

pub fn index_asset() -> EmbeddedAsset {
    generated_asset("index.html").expect("embedded web UI must include index.html")
}
