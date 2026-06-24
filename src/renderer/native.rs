use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use image::DynamicImage;
use ratatui::{
    Frame,
    layout::{Rect, Size},
    widgets::Paragraph,
};
use ratatui_image::{
    Image, Resize,
    picker::{Picker, ProtocolType},
    protocol::Protocol,
};

use crate::{
    cli::BackendChoice,
    metadata::load_oriented_image,
    renderer::{ImageRenderer, RendererCapabilities, backend_label},
    state::{ImageEntry, ZoomMode},
    thumbnail::ThumbKey,
};

pub struct NativeRatatuiImageRenderer {
    picker: Option<Picker>,
    choice: BackendChoice,
    allow_symbol_fallback: bool,
    backend_id: String,
    preview_cache: HashMap<PreviewKey, Protocol>,
    thumbnail_cache: HashMap<ThumbKey, Protocol>,
}

impl NativeRatatuiImageRenderer {
    pub fn new(choice: BackendChoice, allow_symbol_fallback: bool) -> Self {
        Self {
            picker: None,
            choice,
            allow_symbol_fallback,
            backend_id: backend_label(choice).to_owned(),
            preview_cache: HashMap::new(),
            thumbnail_cache: HashMap::new(),
        }
    }

    pub fn render_preview(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        entry: &ImageEntry,
        zoom: ZoomMode,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        match self.preview_protocol(entry, area, zoom) {
            Ok((key, target)) => {
                if let Some(protocol) = self.preview_cache.get(&key) {
                    let image = Image::new(protocol);
                    frame.render_widget(image, target);
                }
            }
            Err(error) => {
                frame.render_widget(
                    Paragraph::new(format!("Failed to render image\n{error:#}")),
                    area,
                );
            }
        }
    }

    pub fn render_thumbnail(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        key: &ThumbKey,
        image: Arc<DynamicImage>,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let Ok(picker) = self.picker().cloned() else {
            frame.render_widget(Paragraph::new("renderer unavailable"), area);
            return;
        };

        if !self.thumbnail_cache.contains_key(key) {
            let size = Size::new(area.width, area.height);
            match picker.new_protocol((*image).clone(), size, Resize::Fit(None)) {
                Ok(protocol) => {
                    self.thumbnail_cache.insert(key.clone(), protocol);
                }
                Err(error) => {
                    frame.render_widget(Paragraph::new(format!("thumb error\n{error}")), area);
                    return;
                }
            }
        }

        if let Some(protocol) = self.thumbnail_cache.get(key) {
            let image = Image::new(protocol);
            frame.render_widget(image, area);
        }
    }

    pub fn reset_image_protocols(&mut self) {
        self.preview_cache.clear();
        self.thumbnail_cache.clear();
    }

    fn picker(&mut self) -> Result<&Picker> {
        if self.picker.is_none() {
            self.preflight()?;
        }
        self.picker.as_ref().context("renderer was not initialized")
    }

    fn preview_protocol(
        &mut self,
        entry: &ImageEntry,
        area: Rect,
        zoom: ZoomMode,
    ) -> Result<(PreviewKey, Rect)> {
        let key_base = PreviewKeyBase::from_entry(entry, area, zoom);
        if let Some((key, target)) = self
            .preview_cache
            .keys()
            .find(|key| key.base == key_base)
            .cloned()
            .map(|key| {
                let target = centered_rect(area, key.protocol_size);
                (key, target)
            })
        {
            return Ok((key, target));
        }

        let image = load_oriented_image(entry)?;
        let picker = self.picker()?.clone();
        let available = Size::new(area.width, area.height);
        let resize = match zoom {
            ZoomMode::Fit => Resize::Fit(None),
            ZoomMode::OriginalPixels => Resize::Crop(None),
        };
        let protocol_size = match zoom {
            ZoomMode::Fit => resize.size_for(&image, picker.font_size(), available),
            ZoomMode::OriginalPixels => {
                let natural = Resize::natural_size(&image, picker.font_size());
                Size::new(
                    natural.width.min(available.width),
                    natural.height.min(available.height),
                )
            }
        };
        let protocol = picker
            .new_protocol(image, protocol_size, resize)
            .context("failed to build terminal image protocol")?;
        let key = PreviewKey {
            base: key_base,
            protocol_size,
        };
        let target = centered_rect(area, protocol_size);
        self.preview_cache.insert(key.clone(), protocol);
        Ok((key, target))
    }

    fn forced_protocol(&self) -> Option<ProtocolType> {
        match self.choice {
            BackendChoice::Kitty => Some(ProtocolType::Kitty),
            BackendChoice::Sixel => Some(ProtocolType::Sixel),
            _ => None,
        }
    }
}

impl ImageRenderer for NativeRatatuiImageRenderer {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    fn preflight(&mut self) -> Result<RendererCapabilities> {
        let mut picker = match Picker::from_query_stdio() {
            Ok(picker) => picker,
            Err(error) if self.allow_symbol_fallback => {
                tracing::warn!(%error, "falling back to halfblocks");
                Picker::halfblocks()
            }
            Err(error) => {
                return Err(anyhow!(
                    "no terminal graphics protocol detected ({error}). Use Kitty/Sixel/iTerm2 support, install Chafa, or pass --allow-symbol-fallback"
                ));
            }
        };

        if let Some(protocol) = self.forced_protocol() {
            picker.set_protocol_type(protocol);
        }

        let protocol = picker.protocol_type();
        if protocol == ProtocolType::Halfblocks && !self.allow_symbol_fallback {
            return Err(anyhow!(
                "terminal graphics detection resolved to halfblocks. Pass --allow-symbol-fallback to allow text fallback"
            ));
        }

        self.backend_id = format!("native:{protocol:?}");
        let capabilities = RendererCapabilities {
            backend_id: self.backend_id.clone(),
            protocol: format!("{protocol:?}"),
            graphics_protocol: protocol != ProtocolType::Halfblocks,
        };
        self.picker = Some(picker);
        Ok(capabilities)
    }

    fn clear(&mut self) -> Result<()> {
        self.reset_image_protocols();
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewKey {
    base: PreviewKeyBase,
    protocol_size: Size,
}

impl Hash for PreviewKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.base.hash(state);
        self.protocol_size.width.hash(state);
        self.protocol_size.height.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PreviewKeyBase {
    path: PathBuf,
    file_len: u64,
    modified_nanos: Option<u128>,
    area_width: u16,
    area_height: u16,
    zoom: ZoomMode,
}

impl PreviewKeyBase {
    fn from_entry(entry: &ImageEntry, area: Rect, zoom: ZoomMode) -> Self {
        Self {
            path: entry.path.clone(),
            file_len: entry.file_len,
            modified_nanos: entry
                .modified
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| value.as_nanos()),
            area_width: area.width,
            area_height: area.height,
            zoom,
        }
    }
}

fn centered_rect(area: Rect, size: Size) -> Rect {
    let width = size.width.min(area.width).max(1);
    let height = size.height.min(area.height).max(1);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}
