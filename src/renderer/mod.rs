use anyhow::Result;

use crate::cli::BackendChoice;

pub mod chafa;
pub mod native;

pub use native::NativeRatatuiImageRenderer;

#[derive(Debug, Clone)]
pub struct RendererCapabilities {
    pub backend_id: String,
    pub protocol: String,
    pub graphics_protocol: bool,
}

pub trait ImageRenderer {
    fn backend_id(&self) -> &str;
    fn preflight(&mut self) -> Result<RendererCapabilities>;
    fn clear(&mut self) -> Result<()>;
}

pub fn backend_label(choice: BackendChoice) -> &'static str {
    match choice {
        BackendChoice::Auto => "auto",
        BackendChoice::Native => "native",
        BackendChoice::Chafa => "chafa",
        BackendChoice::Kitty => "kitty",
        BackendChoice::Sixel => "sixel",
    }
}
