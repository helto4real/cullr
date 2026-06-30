use std::{collections::BTreeSet, fs, path::Path};

use cullr::{
    decode::decode_rgba_capped,
    delete::delete_queued,
    scanner::{ScanOptions, scan_directory},
    state::{AppState, MediaMode, SortMode},
    video::decode_first_frame_rgba,
};
use image::{ImageBuffer, Rgba};
use std::process::Command;
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
    queue_first_entry(&mut state);

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
    queue_first_entry(&mut state);

    let report = delete_queued(&mut state, false);

    assert!(report.failed.is_empty());
    assert!(!image_path.exists());
    assert!(state.entries.is_empty());
}

#[test]
fn real_delete_removes_queued_video_file() {
    let temp = tempdir().unwrap();
    let video_path = temp.path().join("sample.mp4");
    fs::write(&video_path, b"not actually decoded for delete").unwrap();
    let mut state = state_for_ext(temp.path(), "mp4");
    queue_first_entry(&mut state);

    let report = delete_queued(&mut state, false);

    assert!(report.failed.is_empty());
    assert!(!video_path.exists());
    assert!(state.entries.is_empty());
}

#[test]
fn ffmpeg_video_decode_reads_first_frame_when_available() {
    let temp = tempdir().unwrap();
    let video_path = temp.path().join("tiny.mp4");
    let Ok(status) = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=16x8:d=0.2",
            "-frames:v",
            "2",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(&video_path)
        .status()
    else {
        return;
    };
    if !status.success() {
        return;
    }

    let rgba = decode_first_frame_rgba(&video_path, 32).unwrap();

    assert_eq!((rgba.width(), rgba.height()), (16, 8));
}

fn state_for(path: &Path) -> AppState {
    state_for_ext(path, "png")
}

fn state_for_ext(path: &Path, ext: &str) -> AppState {
    let entries = scan_directory(ScanOptions {
        root: path.canonicalize().unwrap(),
        recursive: false,
        include_hidden: false,
        extensions: vec![ext.to_owned()],
    })
    .unwrap();
    AppState::new(
        path.canonicalize().unwrap(),
        false,
        false,
        MediaMode::Image,
        vec![ext.to_owned()],
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

fn queue_first_entry(state: &mut AppState) {
    let path = state.entries[0].path.clone();
    state.delete_queue.insert(path);
}

fn file_set(path: &Path) -> BTreeSet<String> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}
