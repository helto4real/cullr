use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use ratatui_image::{
    Image,
    picker::{Picker, ProtocolType, cap_parser::QueryStdioOptions},
    protocol::Protocol,
};

use crate::{
    cli::BackendChoice,
    renderer::{ImageRenderer, RendererCapabilities, backend_label},
};

pub struct NativeRatatuiImageRenderer {
    picker: Option<Picker>,
    choice: BackendChoice,
    allow_symbol_fallback: bool,
    backend_id: String,
}

impl NativeRatatuiImageRenderer {
    pub fn new(choice: BackendChoice, allow_symbol_fallback: bool) -> Self {
        Self {
            picker: None,
            choice,
            allow_symbol_fallback,
            backend_id: backend_label(choice).to_owned(),
        }
    }

    pub fn render_preview_protocol(&self, frame: &mut Frame, area: Rect, protocol: &Protocol) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let target = centered_rect(area, protocol.size());
        frame.render_widget(Image::new(protocol).allow_clipping(true), target);
    }

    pub fn render_thumbnail_protocol(&self, frame: &mut Frame, area: Rect, protocol: &Protocol) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        frame.render_widget(Image::new(protocol).allow_clipping(true), area);
    }

    pub fn picker_clone(&self) -> Result<Picker> {
        self.picker.clone().context("renderer was not initialized")
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
        let started = Instant::now();
        let mut picker = match self.forced_protocol() {
            Some(protocol) => self.forced_picker(protocol),
            None => self.auto_picker(),
        }?;

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
        tracing::debug!(
            backend = %self.backend_id,
            preflight_ms = started.elapsed().as_millis(),
            "renderer preflight complete"
        );
        self.picker = Some(picker);
        Ok(capabilities)
    }

    fn clear(&mut self) -> Result<()> {
        Ok(())
    }
}

impl NativeRatatuiImageRenderer {
    fn auto_picker(&self) -> Result<Picker> {
        let mut options = QueryStdioOptions::default();
        options.timeout = Duration::from_millis(500);
        match Picker::from_query_stdio_with_options(options) {
            Ok(picker) => Ok(picker),
            Err(error) if self.allow_symbol_fallback => {
                tracing::warn!(%error, "falling back to halfblocks");
                Ok(Picker::halfblocks())
            }
            Err(error) => Err(anyhow!(
                "no terminal graphics protocol detected ({error}). Use Kitty/Sixel/iTerm2 support, install Chafa, or pass --allow-symbol-fallback"
            )),
        }
    }

    fn forced_picker(&self, protocol: ProtocolType) -> Result<Picker> {
        let mut options = QueryStdioOptions::default();
        options.timeout = Duration::from_millis(150);
        options.blacklist_protocols = vec![ProtocolType::Kitty, ProtocolType::Sixel];
        match Picker::from_query_stdio_with_options(options) {
            Ok(mut picker) => {
                picker.set_protocol_type(protocol);
                Ok(picker)
            }
            Err(error) => {
                tracing::debug!(
                    %error,
                    ?protocol,
                    "forced backend font-size query failed; using fallback cell size"
                );
                let mut picker = Picker::halfblocks();
                picker.set_protocol_type(protocol);
                Ok(picker)
            }
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
