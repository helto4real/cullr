//! Image decoding for the GPU frontend.
//!
//! JPEGs use libjpeg-turbo's scaled decode (1/8..1/1) to skip most of the IDCT
//! work for large photos; everything else falls back to the `image` crate. The
//! result is oriented RGBA, ready to upload as a GPU texture.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, ImageFormat, imageops::FilterType};

use crate::metadata::{apply_orientation, read_exif_metadata};

/// Decode an image to oriented RGBA for display, capped so neither axis exceeds
/// `cap` pixels (use `u32::MAX` for native resolution). JPEGs decode straight
/// down via libjpeg-turbo; other formats decode fully and are then downscaled to
/// the cap. EXIF orientation is read here so callers need no pre-enriched data.
pub fn decode_rgba_capped(path: &Path, cap: u32) -> Result<image::RgbaImage> {
    let orientation = read_exif_metadata(path)
        .ok()
        .and_then(|exif| exif.orientation);
    let mut image = decode_at_most(path, cap, cap, orientation)?;
    if cap != u32::MAX && (image.width() > cap || image.height() > cap) {
        image = image.resize(cap, cap, FilterType::Triangle);
    }
    Ok(image.to_rgba8())
}

/// Decode to an oriented image no smaller than `max_width`x`max_height` while
/// doing as little work as possible (scaled JPEG decode when applicable).
fn decode_at_most(
    path: &Path,
    max_width: u32,
    max_height: u32,
    orientation: Option<u16>,
) -> Result<DynamicImage> {
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("failed to detect image format for {}", path.display()))?;

    if matches!(reader.format(), Some(ImageFormat::Jpeg)) {
        match decode_jpeg_scaled(path, max_width, max_height, orientation) {
            Ok(image) => return Ok(image),
            Err(error) => {
                tracing::debug!(
                    path = %path.display(),
                    %error,
                    "turbojpeg scaled decode failed; falling back to image crate"
                );
            }
        }
    }

    let image = reader
        .decode()
        .with_context(|| format!("failed to decode {}", path.display()))?;
    Ok(apply_orientation(image, orientation))
}

/// EXIF orientations 5..=8 rotate the image by 90°, swapping its display axes.
fn swaps_axes(orientation: Option<u16>) -> bool {
    matches!(orientation, Some(5..=8))
}

fn decode_jpeg_scaled(
    path: &Path,
    max_width: u32,
    max_height: u32,
    orientation: Option<u16>,
) -> Result<DynamicImage> {
    let data = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut decompressor = turbojpeg::Decompressor::new()
        .map_err(|error| anyhow!("turbojpeg init failed: {error}"))?;
    let header = decompressor
        .read_header(&data)
        .map_err(|error| anyhow!("turbojpeg header read failed: {error}"))?;

    // A 90° EXIF rotation swaps which source axis maps to the displayed width.
    let (target_width, target_height) = if swaps_axes(orientation) {
        (max_height, max_width)
    } else {
        (max_width, max_height)
    };
    let factor = pick_scaling_factor(header.width, header.height, target_width, target_height);
    decompressor
        .set_scaling_factor(factor)
        .map_err(|error| anyhow!("turbojpeg rejected scaling factor: {error}"))?;

    let width = factor.scale(header.width);
    let height = factor.scale(header.height);
    let mut image = turbojpeg::Image {
        pixels: vec![0u8; width * height * 4],
        width,
        pitch: width * 4,
        height,
        format: turbojpeg::PixelFormat::RGBA,
    };
    decompressor
        .decompress(&data, image.as_deref_mut())
        .map_err(|error| anyhow!("turbojpeg decompress failed: {error}"))?;

    let rgba = image::RgbaImage::from_raw(width as u32, height as u32, image.pixels)
        .context("turbojpeg produced an unexpected buffer size")?;
    Ok(apply_orientation(
        DynamicImage::ImageRgba8(rgba),
        orientation,
    ))
}

/// Pick the most-downscaled supported factor whose output still covers the
/// target in both axes (so we never upscale), falling back to full resolution.
fn pick_scaling_factor(
    width: usize,
    height: usize,
    target_width: u32,
    target_height: u32,
) -> turbojpeg::ScalingFactor {
    let target_width = (target_width.max(1) as usize).min(width.max(1));
    let target_height = (target_height.max(1) as usize).min(height.max(1));

    let mut best = turbojpeg::ScalingFactor::ONE;
    let mut best_pixels = width.saturating_mul(height);
    for factor in turbojpeg::Decompressor::supported_scaling_factors() {
        if factor.num() > factor.denom() {
            continue; // skip upscaling factors
        }
        let scaled_width = factor.scale(width);
        let scaled_height = factor.scale(height);
        if scaled_width >= target_width && scaled_height >= target_height {
            let pixels = scaled_width.saturating_mul(scaled_height);
            if pixels < best_pixels {
                best_pixels = pixels;
                best = factor;
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_oversized_images_on_both_axes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.png");
        image::RgbImage::new(2000, 1000).save(&path).unwrap();

        let rgba = decode_rgba_capped(&path, 512).unwrap();

        assert!(rgba.width() <= 512 && rgba.height() <= 512);
        assert_eq!(rgba.width(), 512); // long edge hits the cap
    }

    #[test]
    fn keeps_small_images_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.png");
        image::RgbImage::new(100, 80).save(&path).unwrap();

        let rgba = decode_rgba_capped(&path, 512).unwrap();

        assert_eq!((rgba.width(), rgba.height()), (100, 80));
    }

    #[test]
    fn decodes_jpeg_content_with_png_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actually_jpeg.png");
        write_jpeg(&path, 32, 16);

        let rgba = decode_rgba_capped(&path, 512).unwrap();

        assert_eq!((rgba.width(), rgba.height()), (32, 16));
    }

    fn write_jpeg(path: &Path, width: u32, height: u32) {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % u8::MAX as u32) as u8, (y % u8::MAX as u32) as u8, 180])
        });
        let mut file = std::fs::File::create(path).unwrap();
        image::codecs::jpeg::JpegEncoder::new(&mut file)
            .encode_image(&image)
            .unwrap();
    }
}
