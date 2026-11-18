//! Image decoding for the GPU frontend.
//!
//! JPEGs use libjpeg-turbo's scaled decode (1/8..1/1) to skip most of the IDCT
//! work for large photos; everything else falls back to the `image` crate. The
//! result is oriented RGBA, ready to upload as a GPU texture.

use std::{path::Path, sync::OnceLock};

use anyhow::{Context, Result, anyhow};
use image::{DynamicImage, ImageFormat, imageops::FilterType};

use crate::metadata::{apply_orientation, read_exif_metadata};

const MIB: usize = 1024 * 1024;
/// Lower bound for the decode memory limit, so small machines can still open
/// typical camera files.
const MIN_DECODE_MEMORY_LIMIT_BYTES: usize = 512 * MIB;
/// Limit used when the platform cannot report its physical memory size.
const FALLBACK_DECODE_MEMORY_LIMIT_BYTES: usize = 2048 * MIB;

#[derive(Debug, thiserror::Error)]
#[error(
    "image decode would exceed the {} MiB memory limit (half of system memory)",
    .limit / MIB
)]
struct DecodeSafetyLimit {
    limit: usize,
}

/// Peak bytes that in-flight image decodes may hold at once, and therefore
/// the largest single decode. Half of physical memory leaves room for the
/// rest of the system while still bounding malformed or hostile files.
pub(crate) fn decode_memory_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| decode_memory_limit_for(physical_memory_bytes()))
}

fn decode_memory_limit_for(physical_memory_bytes: Option<u64>) -> usize {
    physical_memory_bytes
        .map(|bytes| usize::try_from(bytes / 2).unwrap_or(usize::MAX))
        .unwrap_or(FALLBACK_DECODE_MEMORY_LIMIT_BYTES)
        .max(MIN_DECODE_MEMORY_LIMIT_BYTES)
}

#[cfg(unix)]
fn physical_memory_bytes() -> Option<u64> {
    // SAFETY: sysconf has no preconditions; it only reads system configuration.
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let pages = u64::try_from(pages).ok().filter(|pages| *pages > 0)?;
    let page_size = u64::try_from(page_size).ok().filter(|size| *size > 0)?;
    pages.checked_mul(page_size)
}

#[cfg(windows)]
fn physical_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: MEMORYSTATUSEX is plain old data, so all-zero bytes are valid.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is writable and its dwLength is set as the API requires.
    let succeeded = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
    (succeeded && status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
}

#[cfg(not(any(unix, windows)))]
fn physical_memory_bytes() -> Option<u64> {
    None
}

/// Conservatively estimate the peak bytes a decode may retain, including the
/// second RGBA-sized allocation needed to convert the image into egui's
/// texture input on a worker thread. The GUI reserves this before starting
/// work, so parallel workers cannot each materialize a maximum-sized image at
/// the same time.
pub(crate) fn estimated_color_image_decode_bytes(path: &Path, cap: u32) -> Result<usize> {
    let limit = decode_memory_limit();
    let (decode_bytes, output_bytes) = estimate_image_decode(path, cap, limit)?;
    let conversion_bytes = output_bytes
        .checked_mul(2)
        .filter(|bytes| *bytes <= limit)
        .ok_or(DecodeSafetyLimit { limit })?;
    Ok(decode_bytes.max(conversion_bytes))
}

/// Conservatively estimate the peak bytes a decode may retain, returned with
/// the size of the RGBA output alone.
fn estimate_image_decode(path: &Path, cap: u32, limit: usize) -> Result<(usize, usize)> {
    let encoded_bytes = std::fs::metadata(path)
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        .unwrap_or(0);
    if encoded_bytes > limit {
        return Err(DecodeSafetyLimit { limit }.into());
    }

    let reader = image::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.with_guessed_format().ok());
    let format = reader.as_ref().and_then(image::ImageReader::format);
    let Some((width, height)) = reader.and_then(|reader| reader.into_dimensions().ok()) else {
        return Ok((limit, 0));
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
            .ok_or(DecodeSafetyLimit { limit })?
    } else {
        (target_width as usize)
            .checked_mul(target_height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(DecodeSafetyLimit { limit })?
    };
    let retained_bytes = if matches!(format, Some(ImageFormat::Jpeg)) {
        0
    } else {
        (width as usize)
            .checked_mul(height as usize)
            // The image crate may retain 16-bit RGBA source pixels while it
            // creates the capped 8-bit RGBA output.
            .and_then(|pixels| pixels.checked_mul(8))
            .ok_or(DecodeSafetyLimit { limit })?
    };

    Ok((
        checked_decode_reservation([encoded_bytes, retained_bytes, output_bytes], limit)?,
        output_bytes,
    ))
}

fn checked_decode_reservation(parts: [usize; 3], limit: usize) -> Result<usize> {
    parts
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .filter(|bytes| *bytes <= limit)
        .ok_or_else(|| DecodeSafetyLimit { limit }.into())
}

/// Decode an image to oriented RGBA for display, capped so neither axis exceeds
/// `cap` pixels (use `u32::MAX` for native resolution). JPEGs decode straight
/// down via libjpeg-turbo; other formats decode fully and are then downscaled to
/// the cap. EXIF orientation is read here so callers need no pre-enriched data.
pub fn decode_rgba_capped(path: &Path, cap: u32) -> Result<image::RgbaImage> {
    decode_rgba_capped_cancellable(path, cap, &|| false)
        .map(|image| image.expect("decode with cancellation disabled cannot be cancelled"))
}

pub(crate) fn decode_rgba_capped_cancellable(
    path: &Path,
    cap: u32,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<image::RgbaImage>> {
    decode_rgba_capped_with_limit(path, cap, decode_memory_limit(), cancelled)
}

fn decode_rgba_capped_with_limit(
    path: &Path,
    cap: u32,
    limit: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<image::RgbaImage>> {
    if cancelled() {
        return Ok(None);
    }
    estimate_image_decode(path, cap, limit)?;
    if cancelled() {
        return Ok(None);
    }
    let orientation = read_exif_metadata(path)
        .ok()
        .and_then(|exif| exif.orientation);
    if cancelled() {
        return Ok(None);
    }
    let Some(mut image) = decode_at_most(path, cap, cap, orientation, limit, cancelled)? else {
        return Ok(None);
    };
    if cap != u32::MAX && (image.width() > cap || image.height() > cap) {
        if cancelled() {
            return Ok(None);
        }
        image = image.resize(cap, cap, FilterType::Triangle);
    }
    if cancelled() {
        return Ok(None);
    }
    let image = image.to_rgba8();
    Ok((!cancelled()).then_some(image))
}

/// Decode to an oriented image no smaller than `max_width`x`max_height` while
/// doing as little work as possible (scaled JPEG decode when applicable).
fn decode_at_most(
    path: &Path,
    max_width: u32,
    max_height: u32,
    orientation: Option<u16>,
    limit: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<DynamicImage>> {
    if cancelled() {
        return Ok(None);
    }
    let mut reader = image::ImageReader::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("failed to detect image format for {}", path.display()))?;
    // The image crate applies its own 512 MiB allocation cap by default; keep
    // it in step with the limit the decode was reserved against.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(u64::try_from(limit).unwrap_or(u64::MAX));
    reader.limits(limits);

    if matches!(reader.format(), Some(ImageFormat::Jpeg)) {
        match decode_jpeg_scaled(path, max_width, max_height, orientation, limit, cancelled) {
            Ok(Some(image)) => return Ok(Some(image)),
            Ok(None) => return Ok(None),
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

    if cancelled() {
        return Ok(None);
    }
    let image = reader
        .decode()
        .with_context(|| format!("failed to decode {}", path.display()))?;
    if cancelled() {
        return Ok(None);
    }
    Ok(Some(apply_orientation(image, orientation)))
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
    limit: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<DynamicImage>> {
    if cancelled() {
        return Ok(None);
    }
    let data = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if cancelled() {
        return Ok(None);
    }
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
        .filter(|bytes| *bytes <= limit)
        .ok_or(DecodeSafetyLimit { limit })?;
    checked_decode_reservation([data.len(), 0, buffer_len], limit)?;
    let mut image = turbojpeg::Image {
        pixels: vec![0u8; buffer_len],
        width,
        pitch: width * 4,
        height,
        format: turbojpeg::PixelFormat::RGBA,
    };
    if cancelled() {
        return Ok(None);
    }
    decompressor
        .decompress(&data, image.as_deref_mut())
        .map_err(|error| anyhow!("turbojpeg decompress failed: {error}"))?;

    let rgba = image::RgbaImage::from_raw(width as u32, height as u32, image.pixels)
        .context("turbojpeg produced an unexpected buffer size")?;
    if cancelled() {
        return Ok(None);
    }
    Ok(Some(apply_orientation(
        DynamicImage::ImageRgba8(rgba),
        orientation,
    )))
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.jpg");
        write_jpeg(&path, 256, 256);

        // The scaled JPEG buffer exceeds the limit; the decode must not fall
        // back to the image crate, which would allocate the same buffer.
        let error =
            decode_at_most(&path, u32::MAX, u32::MAX, None, 64 * 1024, &|| false).unwrap_err();

        assert!(error.downcast_ref::<DecodeSafetyLimit>().is_some());
    }

    #[test]
    fn image_crate_allocations_follow_the_decode_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.png");
        image::RgbImage::new(256, 256).save(&path).unwrap();

        let error =
            decode_at_most(&path, u32::MAX, u32::MAX, None, 64 * 1024, &|| false).unwrap_err();

        assert!(error.chain().any(|cause| matches!(
            cause.downcast_ref::<image::ImageError>(),
            Some(image::ImageError::Limits(_))
        )));
    }

    #[test]
    fn rejects_an_oversized_encoded_file_before_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.jpg");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MIB as u64 + 1).unwrap();

        let error = estimate_image_decode(&path, 512, MIB).unwrap_err();

        assert!(error.downcast_ref::<DecodeSafetyLimit>().is_some());
    }

    #[test]
    fn accepts_a_large_16_bit_image_within_the_limit() {
        // An 8000 px 16-bit RGBA TIFF (file + retained source + RGBA output)
        // stays well below the limit of a machine with 8 GiB of memory.
        let limit = decode_memory_limit_for(Some(8 * 1024 * MIB as u64));
        let file = 8000 * 8000 * 8;
        let retained = 8000 * 8000 * 8;
        let output = 8000 * 8000 * 4;

        assert!(checked_decode_reservation([file, retained, output], limit).is_ok());
        assert!(checked_decode_reservation([file, retained, output], 512 * MIB).is_err());
    }

    #[test]
    fn decode_memory_limit_is_half_of_physical_memory() {
        assert_eq!(
            decode_memory_limit_for(Some(16 * 1024 * MIB as u64)),
            8 * 1024 * MIB
        );
        assert_eq!(
            decode_memory_limit_for(Some(256 * MIB as u64)),
            MIN_DECODE_MEMORY_LIMIT_BYTES
        );
        assert_eq!(
            decode_memory_limit_for(None),
            FALLBACK_DECODE_MEMORY_LIMIT_BYTES
        );
    }

    #[test]
    fn reports_physical_memory() {
        assert!(physical_memory_bytes().is_some_and(|bytes| bytes > 0));
        assert!(decode_memory_limit() >= MIN_DECODE_MEMORY_LIMIT_BYTES);
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
