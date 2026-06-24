use std::{collections::HashMap, fs, path::PathBuf};

use crate::state::AppState;

#[derive(Debug, Clone, Default)]
pub struct DeleteReport {
    pub deleted: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub dry_run: bool,
}

pub fn delete_queued(state: &mut AppState, dry_run: bool) -> DeleteReport {
    let mut report = DeleteReport {
        dry_run,
        ..DeleteReport::default()
    };
    let discovered: HashMap<PathBuf, _> = state
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect();
    let queued: Vec<PathBuf> = state.delete_queue.iter().cloned().collect();

    for path in queued {
        let Some(entry) = discovered.get(&path) else {
            report
                .failed
                .push((path, "not discovered in current scan".to_owned()));
            continue;
        };

        if let Err(error) = safety_check(state, entry) {
            report.failed.push((path, error));
            continue;
        }

        if dry_run {
            report.deleted.push(path);
            continue;
        }

        match fs::remove_file(&path) {
            Ok(()) => report.deleted.push(path),
            Err(error) => report.failed.push((path, error.to_string())),
        }
    }

    if !dry_run {
        for path in &report.deleted {
            state.delete_queue.shift_remove(path);
        }
        state
            .entries
            .retain(|entry| !report.deleted.iter().any(|path| path == &entry.path));
        state.clamp_current_index();
        state.bump_generation();
    }

    report
}

fn safety_check(state: &AppState, entry: &crate::state::ImageEntry) -> Result<(), String> {
    let canonical = entry
        .path
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize path: {error}"))?;
    if !canonical.starts_with(&state.directory) {
        return Err("path is outside selected directory".to_owned());
    }

    let metadata = fs::symlink_metadata(&entry.path)
        .map_err(|error| format!("cannot read metadata: {error}"))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err("refusing to delete symlink".to_owned());
    }
    if !file_type.is_file() {
        return Err("refusing to delete non-file".to_owned());
    }
    if metadata.len() != entry.file_len {
        return Err("file size changed since scan".to_owned());
    }
    if let (Some(scanned), Ok(current)) = (entry.modified, metadata.modified()) {
        if scanned != current {
            return Err("modified time changed since scan".to_owned());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ImageEntry, ImageKind, SortMode};
    use std::{ffi::OsString, time::SystemTime};
    use tempfile::tempdir;

    fn make_state(path: PathBuf) -> AppState {
        let metadata = fs::symlink_metadata(&path).unwrap();
        let entry = ImageEntry {
            path: path.clone(),
            file_name: OsString::from("a.jpg"),
            display_name: "a.jpg".to_owned(),
            extension: Some("jpg".to_owned()),
            file_len: metadata.len(),
            created: None,
            modified: metadata.modified().ok(),
            discovered_order: 0,
            dimensions: None,
            image_type: Some(ImageKind::Jpeg),
            exif_date: None,
            exif_orientation: None,
            dimensions_attempted: false,
            exif_attempted: false,
        };
        let directory = path.parent().unwrap().canonicalize().unwrap();
        let mut state = AppState::new(
            directory,
            false,
            false,
            vec!["jpg".to_owned()],
            SortMode::Discovered,
            vec![entry],
        );
        state.delete_queue.insert(path);
        state
    }

    #[test]
    fn dry_run_keeps_file_and_entries() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("a.jpg");
        fs::write(&path, b"x").unwrap();
        let mut state = make_state(path.clone());

        let report = delete_queued(&mut state, true);

        assert!(report.dry_run);
        assert!(path.exists());
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.delete_queue.len(), 1);
    }

    #[test]
    fn real_delete_removes_file_and_entry() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("a.jpg");
        fs::write(&path, b"x").unwrap();
        let mut state = make_state(path.clone());

        let report = delete_queued(&mut state, false);

        assert!(report.failed.is_empty());
        assert!(!path.exists());
        assert!(state.entries.is_empty());
        assert!(state.delete_queue.is_empty());
    }

    #[test]
    fn modified_file_is_not_deleted() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("a.jpg");
        fs::write(&path, b"x").unwrap();
        let mut state = make_state(path.clone());
        state.entries[0].modified = Some(SystemTime::UNIX_EPOCH);

        let report = delete_queued(&mut state, false);

        assert!(!report.failed.is_empty());
        assert!(path.exists());
    }
}
