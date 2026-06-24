use std::{collections::BTreeSet, fs, path::Path};

use cullr::{
    decode::decode_rgba_capped,
    delete::delete_queued,
    scanner::{ScanOptions, scan_directory},
    state::{AppState, SortMode},
};
use image::{ImageBuffer, Rgba};
use tempfile::tempdir;

#[test]
fn decode_writes_no_files_to_input_directory() {
    let temp = tempdir().unwrap();
    let image_path = temp.path().join("sample.png");
    write_sample_image(&image_path);
    let before = file_set(temp.path());

    let rgba = decode_rgba_capped(&image_path, 3840).unwrap();

    let after = file_set(temp.path());
    assert!(rgba.width() > 0);
    assert_eq!(before, after);
}

#[test]
fn decode_caps_large_images_for_display() {
    let temp = tempdir().unwrap();
    let image_path = temp.path().join("big.png");
    image::RgbImage::new(1600, 900).save(&image_path).unwrap();

    let rgba = decode_rgba_capped(&image_path, 512).unwrap();

    assert!(rgba.width() <= 512 && rgba.height() <= 512);
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
