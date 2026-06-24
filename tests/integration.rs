use std::{collections::BTreeSet, fs, path::Path};

use cullr::{
    delete::delete_queued,
    scanner::{ScanOptions, scan_directory},
    state::{AppState, SortMode},
    thumbnail::generate_thumbnail_for_test,
};
use image::{ImageBuffer, Rgba};
use tempfile::tempdir;

#[test]
fn thumbnail_generation_writes_no_files_to_input_directory() {
    let temp = tempdir().unwrap();
    let image_path = temp.path().join("sample.png");
    write_sample_image(&image_path);
    let before = file_set(temp.path());

    let thumbnail = generate_thumbnail_for_test(image_path, 12, 8).unwrap();

    let after = file_set(temp.path());
    assert!(thumbnail.width() > 0);
    assert_eq!(before, after);
}

#[test]
fn dry_run_delete_keeps_files() {
    let temp = tempdir().unwrap();
    let image_path = temp.path().join("sample.png");
    write_sample_image(&image_path);
    let mut state = state_for(temp.path());
    state.delete_queue.insert(image_path.clone());

    let report = delete_queued(&mut state, true);

    assert!(report.dry_run);
    assert!(image_path.exists());
    assert_eq!(state.entries.len(), 1);
}

#[test]
fn real_delete_removes_queued_file() {
    let temp = tempdir().unwrap();
    let image_path = temp.path().join("sample.png");
    write_sample_image(&image_path);
    let mut state = state_for(temp.path());
    state.delete_queue.insert(image_path.clone());

    let report = delete_queued(&mut state, false);

    assert!(report.failed.is_empty());
    assert!(!image_path.exists());
    assert!(state.entries.is_empty());
}

fn state_for(path: &Path) -> AppState {
    let entries = scan_directory(ScanOptions {
        root: path.canonicalize().unwrap(),
        recursive: false,
        include_hidden: false,
        extensions: vec!["png".to_owned()],
    })
    .unwrap();
    AppState::new(
        path.canonicalize().unwrap(),
        false,
        false,
        vec!["png".to_owned()],
        SortMode::Discovered,
        entries,
    )
}

fn write_sample_image(path: &Path) {
    let image = ImageBuffer::from_fn(8, 8, |x, y| {
        if (x + y) % 2 == 0 {
            Rgba([255u8, 0, 0, 255])
        } else {
            Rgba([0u8, 0, 255, 255])
        }
    });
    image.save(path).unwrap();
}

fn file_set(path: &Path) -> BTreeSet<String> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}
