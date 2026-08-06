//! Image decoding for the GPU frontend.
//!
//! JPEGs use libjpeg-turbo's scaled decode (1/8..1/1) to skip most of the IDCT
//! work for large photos; everything else falls back to the `image` crate. The
//! result is oriented RGBA, ready to upload as a GPU texture.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, ImageFormat, imageops::FilterType};

use crate::metadata::{apply_orientation, read_exif_metadata};

const MAX_RGBA_DECODE_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("image decode would exceed the 512 MiB safety limit")]
struct DecodeSafetyLimit;

/// Conservatively estimate the peak bytes a decode may retain. The GUI uses
/// this to reserve capacity before starting work so parallel workers cannot
/// each materialize a maximum-sized image at the same time.
pub(crate) fn estimated_image_decode_bytes(path: &Path, cap: u32) -> Result<usize> {
    estimate_image_decode(path, cap).map(|(decode_bytes, _)| decode_bytes)
}

/// Include the second RGBA-sized allocation needed to convert a decoded image
/// into egui's texture input on a worker thread.
pub(crate) fn estimated_color_image_decode_bytes(path: &Path, cap: u32) -> Result<usize> {
    let (decode_bytes, output_bytes) = estimate_image_decode(path, cap)?;
    let conversion_bytes = output_bytes
        .checked_mul(2)
        .filter(|bytes| *bytes <= MAX_RGBA_DECODE_BYTES)
        .ok_or(DecodeSafetyLimit)?;
    Ok(decode_bytes.max(conversion_bytes))
}

fn estimate_image_decode(path: &Path, cap: u32) -> Result<(usize, usize)> {
    let encoded_bytes = std::fs::metadata(path)
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        .unwrap_or(0);
    if encoded_bytes > MAX_RGBA_DECODE_BYTES {
        return Err(DecodeSafetyLimit.into());
    }

    let reader = image::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.with_guessed_format().ok());
    let format = reader.as_ref().and_then(image::ImageReader::format);
    let Some((width, height)) = reader.and_then(|reader| reader.into_dimensions().ok()) else {
        return Ok((MAX_RGBA_DECODE_BYTES, 0));
    };

    let (target_width, target_height) = if cap == u32::MAX {
        (width, height)
    } else {
        let (width, height) = contained_dimensions(width as usize, height as usize, cap, cap);
        (width, height)
    };
    let output_bytes = if matches!(format, Some(ImageFormat::Jpeg)) {
        let factor =
            pick_scaling_factor(width as usize, height as usize, target_width, target_height);
        factor
            .scale(width as usize)
            .checked_mul(factor.scale(height as usize))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(DecodeSafetyLimit)?
    } else {
        (target_width as usize)
            .checked_mul(target_height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(DecodeSafetyLimit)?
    };
    let retained_bytes = if matches!(format, Some(ImageFormat::Jpeg)) {
        0
    } else {
        (width as usize)
            .checked_mul(height as usize)
            // The image crate may retain 16-bit RGBA source pixels while it
            // creates the capped 8-bit RGBA output.
            .and_then(|pixels| pixels.checked_mul(8))
            .ok_or(DecodeSafetyLimit)?
    };

    Ok((
        checked_decode_reservation([encoded_bytes, retained_bytes, output_bytes])?,
        output_bytes,
    ))
}

fn checked_decode_reservation(parts: [usize; 3]) -> Result<usize> {
    parts
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .filter(|bytes| *bytes <= MAX_RGBA_DECODE_BYTES)
        .ok_or_else(|| DecodeSafetyLimit.into())
}

/// Decode an image to oriented RGBA for display, capped so neither axis exceeds
/// `cap` pixels (use `u32::MAX` for native resolution). JPEGs decode straight
/// down via libjpeg-turbo; other formats decode fully and are then downscaled to
/// the cap. EXIF orientation is read here so callers need no pre-enriched data.
pub fn decode_rgba_capped(path: &Path, cap: u32) -> Result<image::RgbaImage> {
    estimated_image_decode_bytes(path, cap)?;
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
                if error.downcast_ref::<DecodeSafetyLimit>().is_some() {
                    return Err(error);
                }
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
    let (source_max_width, source_max_height) = if swaps_axes(orientation) {
        (max_height, max_width)
    } else {
        (max_width, max_height)
    };
    let (target_width, target_height) = contained_dimensions(
        header.width,
        header.height,
        source_max_width,
        source_max_height,
    );
    let factor = pick_scaling_factor(header.width, header.height, target_width, target_height);
    decompressor
        .set_scaling_factor(factor)
        .map_err(|error| anyhow!("turbojpeg rejected scaling factor: {error}"))?;

    let width = factor.scale(header.width);
    let height = factor.scale(header.height);
    let buffer_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes <= MAX_RGBA_DECODE_BYTES)
        .ok_or(DecodeSafetyLimit)?;
    checked_decode_reservation([data.len(), 0, buffer_len])?;
    let mut image = turbojpeg::Image {
        pixels: vec![0u8; buffer_len],
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

fn contained_dimensions(
    width: usize,
    height: usize,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    let width = width.max(1) as f64;
    let height = height.max(1) as f64;
    let scale = (max_width.max(1) as f64 / width)
        .min(max_height.max(1) as f64 / height)
        .min(1.0);
    (
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    )
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

    #[test]
    fn jpeg_scaling_target_preserves_aspect_ratio() {
        assert_eq!(contained_dimensions(8000, 4000, 3840, 3840), (3840, 1920));
        let factor = pick_scaling_factor(8000, 4000, 3840, 1920);
        assert!(factor.scale(8000) < 8000);
        assert!(factor.scale(8000) >= 3840);
        assert!(factor.scale(4000) >= 1920);
    }

    #[test]
    fn jpeg_safety_limit_is_a_terminal_decode_error() {
        let error = 20_000usize
            .checked_mul(20_000)
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|bytes| *bytes <= MAX_RGBA_DECODE_BYTES)
            .ok_or(DecodeSafetyLimit)
            .map_err(anyhow::Error::from)
            .unwrap_err();

        assert!(error.downcast_ref::<DecodeSafetyLimit>().is_some());
    }

    #[test]
    fn rejects_an_oversized_encoded_file_before_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.jpg");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_RGBA_DECODE_BYTES as u64 + 1).unwrap();

        let error = estimated_image_decode_bytes(&path, 512).unwrap_err();

        assert!(error.downcast_ref::<DecodeSafetyLimit>().is_some());
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
