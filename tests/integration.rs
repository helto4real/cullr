use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use cullr::{
    delete::delete_queued,
    scanner::{ScanOptions, scan_directory},
    state::{AppState, SortMode, ZoomMode},
    thumbnail::{
        PreviewStatus, ThumbnailService, ThumbnailStatus, generate_preview_protocol_for_test,
        generate_thumbnail_for_test,
    },
};
use image::{ImageBuffer, Rgba};
use ratatui_image::picker::Picker;
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
fn preview_generation_writes_no_files_to_input_directory() {
    let temp = tempdir().unwrap();
    let image_path = temp.path().join("sample.png");
    write_sample_image(&image_path);
    let before = file_set(temp.path());

    let protocol = generate_preview_protocol_for_test(image_path, 80, 24, ZoomMode::Fit).unwrap();

    let after = file_set(temp.path());
    assert!(protocol.size().width > 0);
    assert_eq!(before, after);
}

#[test]
#[ignore = "synthetic prefetch timing check for manual performance runs"]
fn synthetic_grid_prefetch_produces_cached_protocols() {
    let temp = tempdir().unwrap();
    for index in 0..20 {
        write_sample_image(&temp.path().join(format!("{index:02}.png")));
    }
    let state = state_for(temp.path());
    let mut thumbnails = ThumbnailService::new(64);
    thumbnails.configure_renderer(Picker::halfblocks(), "native:Halfblocks".to_owned());

    for entry in &state.entries {
        thumbnails.prefetch_thumbnail(entry, 18, 10, state.thumbnail_generation);
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        thumbnails.poll_finished(state.thumbnail_generation);
        let ready = state
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    thumbnails.get_or_request_thumbnail(entry, 18, 10, state.thumbnail_generation),
                    ThumbnailStatus::Ready { .. }
                )
            })
            .count();

        if ready >= 8 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "prefetch did not produce enough thumbnails"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "synthetic first-preview latency check for manual performance runs"]
fn synthetic_small_folder_preview_is_not_timer_bound() {
    let temp = tempdir().unwrap();
    for index in 0..5 {
        write_sample_image(&temp.path().join(format!("{index:02}.png")));
    }
    let state = state_for(temp.path());
    let mut thumbnails = ThumbnailService::new(64);
    thumbnails.configure_renderer(Picker::halfblocks(), "native:Halfblocks".to_owned());

    let started = Instant::now();
    thumbnails.prefetch_preview(
        &state.entries[0],
        80,
        24,
        state.zoom_mode,
        state.thumbnail_generation,
    );
    let deadline = started + Duration::from_millis(50);
    loop {
        thumbnails.poll_finished(state.thumbnail_generation);
        if matches!(
            thumbnails.get_or_request_preview(
                &state.entries[0],
                80,
                24,
                state.zoom_mode,
                state.thumbnail_generation,
            ),
            PreviewStatus::Ready { .. }
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "first preview waited at least as long as the old 50ms loop"
        );
        thread::sleep(Duration::from_millis(1));
    }
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
