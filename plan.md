# Cullr architecture and maintenance plan

This document describes the current application. The original terminal-based
prototype plan was superseded when Cullr moved to a native `eframe`/`egui`
frontend.

## Product contract

Cullr is a native desktop media viewer and culling tool. It opens a directory,
one media file within its parent directory, or an explicit set of files. Users
can review images and videos, queue rejects, inspect the queue, and delete only
after confirmation.

The core safety and privacy invariants are:

- Decoded previews and thumbnails stay in memory; input directories receive no
  cache files.
- Directory scans and the browser skip symlinks.
- Direct file and multi-file launches reject symlinks before canonicalization.
- A successful rescan invalidates decoded media and clears the delete queue, so
  a replacement file must be reviewed and queued again.
- Deletion is limited to the active canonical directory or explicit selected
  file set.
- Deletion refuses symlinks, non-files, and files whose size or modification
  time changed after the scan.

## Runtime architecture

`src/main.rs`
: Parses CLI arguments, initializes tracing, and starts the GUI.

`src/cli.rs`
: Defines the `clap` contract, media filters, extensions, and initial sorting.

`src/gui.rs`
: Owns the `eframe` application, keyboard routing, browser/right-pane
  coordination, texture caches, decode workers, video event handling, and
  overlays.

`src/state.rs`
: Holds media entries, browser state, view/sort/zoom modes, selection, and the
  delete queue. Queue-view navigation operates on queued indices rather than
  the complete library.

`src/scanner.rs`
: Performs flat scans with `read_dir`, recursive scans with `jwalk`, extension
  filtering, symlink rejection, and MPEG-TS probing for ambiguous `.ts` files.

`src/browser.rs`
: Reads and classifies one directory for the file-browser pane, preserves
  selection, and sorts folders before files.

`src/sorter.rs` and `src/metadata.rs`
: Provide locale-aware filename sorting and EXIF/filesystem time sorting.

`src/decode.rs`
: Decodes images. JPEGs use libjpeg-turbo scaled decoding against an
  aspect-preserving target; other formats use the `image` crate. EXIF
  orientation is applied before upload.

`src/video.rs`
: Uses linked FFmpeg libraries for thumbnails and playback, applies sample
  aspect ratio and display-matrix rotation, resamples audio for Rodio, and
  exposes bounded playback events to the GUI.

`src/delete.rs`
: Performs the final scope, type, size, and modification-time checks immediately
  before removal.

## Decode and cache model

Image/video thumbnail work runs on a small worker pool. Requests and results use
bounded channels so rapid navigation cannot create unbounded work or decoded
RGBA buffers. Each request carries a scan generation; results from an older
generation are discarded.

Preview and thumbnail texture caches are bounded by both entry count and
estimated RGBA bytes. Fit-mode previews prefetch nearby entries. Original-pixel
mode decodes only the current entry because a single source image can already be
hundreds of megabytes.

Video playback uses a small bounded event channel. If rendering falls behind,
superseded frames are dropped while terminal metadata, error, and ended events
remain reliable. This keeps playback near the current timeline instead of
building a backlog.

## Browser state transitions

Browser listings and right-pane media scans are separate operations. A right
pane scan is built in local values and committed only after success. Failures
leave the previous state intact and show an explicit pane error. Moving between
files in an already-scanned directory uses a no-rescan fast path; an explicit
rescan bypasses it.

## Launch modes

- No path: scan the current working directory and open the grid.
- Directory: scan that directory and open the grid.
- One media file: scan its parent and open that file in preview.
- Multiple files: retain argument order, deduplicate canonical paths, and open
  only accepted media as a focused grid.
- `--view browser`: open the browser for directory or single-file launches.

A requested single file that is unsupported or excluded by `--media` or
`--file_ext` is an error; Cullr never silently substitutes another file from the
same directory.

## Validation contract

The local and CI quality gate is:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

Tests cover CLI resolution, scanning/filtering, MPEG-TS detection, browser state,
sorting, decode sizing/orientation, queue navigation, rescan invalidation,
deletion safety, FFmpeg first-frame decoding, sample-aspect correction, and
dry-run deletion.

## Release maintenance

GitHub Actions builds Linux tarball, deb, RPM, AppImage, Windows zip, and MSI
artifacts. Executable packaging tools downloaded during a release must be pinned
to immutable release tags and verified against repository-owned or
GitHub-published SHA-256 digests before execution.

When changing media or deletion behavior, add a regression test at the narrowest
module seam first, then run the complete validation contract before release.
