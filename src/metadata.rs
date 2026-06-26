use std::{
    fs::File,
    io::BufReader,
    path::Path,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use exif::{In, Reader, Tag, Value};
use image::ImageReader;
use time::{PrimitiveDateTime, macros::format_description};

use crate::state::MediaEntry;

pub fn enrich_entry(entry: &mut MediaEntry) {
    if !entry.media_kind.is_image() {
        entry.dimensions_attempted = true;
        entry.exif_attempted = true;
        return;
    }

    if entry.dimensions.is_none() && !entry.dimensions_attempted {
        entry.dimensions_attempted = true;
        if let Ok((width, height)) = image::image_dimensions(&entry.path) {
            entry.dimensions = Some((width, height));
        }
    }

    if (entry.exif_date.is_none() || entry.exif_orientation.is_none()) && !entry.exif_attempted {
        entry.exif_attempted = true;
        if let Ok(exif) = read_exif_metadata(&entry.path) {
            entry.exif_date = entry.exif_date.or(exif.date);
            entry.exif_orientation = entry.exif_orientation.or(exif.orientation);
        }
    }
}

pub fn enrich_entries_for_time_sort(entries: &mut [MediaEntry]) {
    for entry in entries {
        if !entry.media_kind.is_image() {
            entry.exif_attempted = true;
            continue;
        }
        if entry.exif_date.is_none() && !entry.exif_attempted {
            entry.exif_attempted = true;
            if let Ok(exif) = read_exif_metadata(&entry.path) {
                entry.exif_date = exif.date;
                entry.exif_orientation = entry.exif_orientation.or(exif.orientation);
            }
        }
    }
}

pub fn load_oriented_image(entry: &MediaEntry) -> Result<image::DynamicImage> {
    let image = ImageReader::open(&entry.path)
        .with_context(|| format!("failed to open {}", entry.path.display()))?
        .decode()
        .with_context(|| format!("failed to decode {}", entry.path.display()))?;

    Ok(apply_orientation(image, entry.exif_orientation))
}

pub fn apply_orientation(
    image: image::DynamicImage,
    orientation: Option<u16>,
) -> image::DynamicImage {
    match orientation.unwrap_or(1) {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExifMetadata {
    pub date: Option<SystemTime>,
    pub orientation: Option<u16>,
}

pub fn read_exif_metadata(path: &Path) -> Result<ExifMetadata> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let exif = Reader::new().read_from_container(&mut reader)?;

    let orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| (1..=8).contains(value));

    let date = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .and_then(|field| ascii_value(&field.value))
        .and_then(parse_exif_datetime);

    Ok(ExifMetadata { date, orientation })
}

pub fn effective_date(entry: &MediaEntry) -> Option<SystemTime> {
    entry.exif_date.or(entry.created).or(entry.modified)
}

fn ascii_value(value: &Value) -> Option<String> {
    match value {
        Value::Ascii(values) => values
            .first()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(|value| value.trim_matches(char::from(0)).trim().to_owned()),
        _ => None,
    }
}

fn parse_exif_datetime(value: String) -> Option<SystemTime> {
    let format = format_description!("[year]:[month]:[day] [hour]:[minute]:[second]");
    let parsed = PrimitiveDateTime::parse(&value, &format).ok()?;
    let timestamp = parsed.assume_utc().unix_timestamp();
    if timestamp >= 0 {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp as u64))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(timestamp.unsigned_abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ImageKind, MediaKind, SortMode};
    use std::{ffi::OsString, path::PathBuf};

    #[test]
    fn exif_datetime_parses() {
        assert!(parse_exif_datetime("2026:06:24 12:34:56".to_owned()).is_some());
    }

    #[test]
    fn effective_date_prefers_exif() {
        let exif = SystemTime::UNIX_EPOCH + Duration::from_secs(30);
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let entry = MediaEntry {
            path: PathBuf::from("a.jpg"),
            file_name: OsString::from("a.jpg"),
            display_name: "a.jpg".to_owned(),
            extension: Some("jpg".to_owned()),
            file_len: 0,
            created: Some(created),
            modified: None,
            discovered_order: 0,
            dimensions: None,
            media_kind: MediaKind::Image(ImageKind::Jpeg),
            exif_date: Some(exif),
            exif_orientation: None,
            dimensions_attempted: false,
            exif_attempted: true,
        };

        assert_eq!(effective_date(&entry), Some(exif));
        let _ = SortMode::Newest;
    }
}
