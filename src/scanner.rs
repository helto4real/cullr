use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use jwalk::{Parallelism, WalkDir};

use crate::state::{MediaEntry, MediaKind};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub root: PathBuf,
    pub recursive: bool,
    pub include_hidden: bool,
    pub extensions: Vec<String>,
}

pub fn scan_directory(opts: ScanOptions) -> Result<Vec<MediaEntry>> {
    if opts.recursive {
        scan_recursive_with_jwalk(opts)
    } else {
        scan_flat_with_read_dir(opts)
    }
}

fn scan_flat_with_read_dir(opts: ScanOptions) -> Result<Vec<MediaEntry>> {
    let extensions = extension_set(&opts.extensions);
    let mut entries = Vec::new();

    for dir_entry in fs::read_dir(&opts.root)
        .with_context(|| format!("failed to read {}", opts.root.display()))?
    {
        let dir_entry = match dir_entry {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "skipping unreadable directory entry");
                continue;
            }
        };
        let path = dir_entry.path();
        if !opts.include_hidden && is_hidden_path(&path) {
            continue;
        }
        if let Some(entry) = build_entry(path, entries.len(), &extensions)? {
            entries.push(entry);
        }
    }

    Ok(entries)
}

fn scan_recursive_with_jwalk(opts: ScanOptions) -> Result<Vec<MediaEntry>> {
    let extensions = extension_set(&opts.extensions);
    let walker = WalkDir::new(&opts.root)
        .parallelism(Parallelism::RayonDefaultPool {
            busy_timeout: std::time::Duration::from_millis(500),
        })
        .skip_hidden(!opts.include_hidden);

    let mut entries = Vec::new();
    for dir_entry in walker {
        let dir_entry = match dir_entry {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "skipping unreadable recursive entry");
                continue;
            }
        };
        let path = dir_entry.path();
        if path == opts.root {
            continue;
        }
        if !opts.include_hidden && is_hidden_path(&path) {
            continue;
        }
        if let Some(entry) = build_entry(path, entries.len(), &extensions)? {
            entries.push(entry);
        }
    }

    Ok(entries)
}

fn build_entry(
    path: PathBuf,
    discovered_order: usize,
    extensions: &HashSet<String>,
) -> Result<Option<MediaEntry>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipping unreadable file metadata");
            return Ok(None);
        }
    };

    let file_type = metadata.file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        return Ok(None);
    }

    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);

    if !extension
        .as_ref()
        .map(|ext| extensions.contains(ext))
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let file_name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from(""));
    let display_name = file_name.to_string_lossy().into_owned();

    let Some(media_kind) = MediaKind::from_extension(extension.as_deref()) else {
        return Ok(None);
    };

    Ok(Some(MediaEntry {
        path,
        file_name,
        display_name,
        extension: extension.clone(),
        file_len: metadata.len(),
        created: metadata.created().ok(),
        modified: metadata.modified().ok(),
        discovered_order,
        dimensions: None,
        media_kind,
        exif_date: None,
        exif_orientation: None,
        dimensions_attempted: false,
        exif_attempted: false,
    }))
}

fn extension_set(extensions: &[String]) -> HashSet<String> {
    extensions
        .iter()
        .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect()
}

fn is_hidden_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn touch(path: &Path) {
        fs::write(path, b"not actually decoded").unwrap();
    }

    #[test]
    fn non_recursive_excludes_subfolders() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        touch(&temp.path().join("a.jpg"));
        touch(&temp.path().join("nested").join("b.jpg"));

        let entries = scan_directory(ScanOptions {
            root: temp.path().to_path_buf(),
            recursive: false,
            include_hidden: false,
            extensions: vec!["jpg".to_owned()],
        })
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].display_name, "a.jpg");
    }

    #[test]
    fn recursive_includes_subfolders() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        touch(&temp.path().join("a.jpg"));
        touch(&temp.path().join("nested").join("b.jpg"));

        let entries = scan_directory(ScanOptions {
            root: temp.path().to_path_buf(),
            recursive: true,
            include_hidden: false,
            extensions: vec!["jpg".to_owned()],
        })
        .unwrap();

        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn hidden_handling_and_extension_filter_work() {
        let temp = tempdir().unwrap();
        touch(&temp.path().join(".hidden.jpg"));
        touch(&temp.path().join("visible.png"));
        touch(&temp.path().join("ignored.txt"));

        let entries = scan_directory(ScanOptions {
            root: temp.path().to_path_buf(),
            recursive: false,
            include_hidden: false,
            extensions: vec!["png".to_owned(), "jpg".to_owned()],
        })
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].display_name, "visible.png");
    }

    #[test]
    fn scans_mixed_image_and_video_extensions() {
        let temp = tempdir().unwrap();
        touch(&temp.path().join("image.jpg"));
        touch(&temp.path().join("clip.mp4"));
        touch(&temp.path().join("ignored.txt"));

        let entries = scan_directory(ScanOptions {
            root: temp.path().to_path_buf(),
            recursive: false,
            include_hidden: false,
            extensions: vec!["jpg".to_owned(), "mp4".to_owned(), "txt".to_owned()],
        })
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| entry.media_kind.is_image()));
        assert!(entries.iter().any(|entry| entry.media_kind.is_video()));
    }

    #[test]
    fn explicit_extensions_still_require_known_media_kind() {
        let temp = tempdir().unwrap();
        touch(&temp.path().join("unknown.custom"));

        let entries = scan_directory(ScanOptions {
            root: temp.path().to_path_buf(),
            recursive: false,
            include_hidden: false,
            extensions: vec!["custom".to_owned()],
        })
        .unwrap();

        assert!(entries.is_empty());
    }
}
