# Terminal-Based Fast Image Viewer/Culler in Rust — Implementation Plan

## 1. Product definition

Build a Rust binary, for example `riv`, `rust-image-vim`, or `cullr`, that opens a directory of images and supports two main views:

**Preview mode**: one image fills the available terminal area while preserving aspect ratio.

**Grid mode**: many in-memory thumbnails are shown; the current image is highlighted; pages can be changed with `ctrl+d` and `ctrl+u`.

**Delete queue mode**: a grid view containing only queued images.

No thumbnails, protocol payloads, or caches may be written to disk. Source images are read from disk, but all decoded/resized thumbnail data must live only in memory.

---

## 2. Recommended technical stack

Use `ratatui` + `crossterm` for the TUI and keyboard/event layer. Ratatui is designed around terminal backends, raw mode, alternate screen, key/mouse events, and terminal sizing; Crossterm is the practical cross-platform backend choice.

Use `ratatui-image` as the primary image-rendering integration. It unifies terminal image rendering across Sixel, Kitty, and iTerm2 protocols, queries terminal capabilities/font size, and has a `ThreadProtocol` path for offloading resize/encode work so the UI does not block.

Use `chafa` as an optional external renderer/backend and as the “common optimized tool” preflight target. Chafa can convert image data to terminal graphics formats or ANSI/Unicode art, supports formats including Kitty/Sixel/iTerm2/symbols, supports terminal probing, sizing, grid layout, and multi-threaded work options.

Use Kitty/Sixel protocol support as the high-quality terminal graphics path. Kitty’s graphics protocol is intended for performant raster graphics in terminals. Windows Terminal stable 1.22 added Sixel support; Sixel output needs an encoder such as libsixel or Chafa.

Use `image` for decoding and dimensions. It provides native Rust image decoding/encoding, a high-level `ImageReader`, `DynamicImage`, and support for common image formats with default features.

Use `fast_image_resize` with its `rayon` feature for thumbnail generation. Its docs specifically mention enabling the `rayon` feature for image processing in Rayon’s thread pool.

Use `walkdir` or `jwalk` for traversal. `walkdir` is cross-platform and exposes controls for recursion, symlink following, file descriptor limits, and skipping directories.

Use `clap` for CLI parsing. It supports derive-based parsers and automatically gives the expected `-h` / `--help` behavior.

Use ICU collation for filename sorting when filenames are valid Unicode. The Unicode Collation Algorithm exists specifically to compare Unicode strings in a language-aware way, and `icu_collator` exposes Rust collation APIs.

Use `kamadak-exif` or `nom-exif` for EXIF date/orientation metadata. `kamadak-exif` is pure Rust and can read EXIF from TIFF, JPEG, HEIF, PNG, and WebP containers.

Reference docs:

- Ratatui backends: <https://ratatui.rs/concepts/backends/>
- `ratatui-image`: <https://docs.rs/ratatui-image/latest/ratatui_image/>
- Chafa man page: <https://man.archlinux.org/man/extra/chafa/chafa.1.en>
- Kitty graphics protocol: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>
- Rust `image` crate: <https://docs.rs/image>
- `fast_image_resize`: <https://docs.rs/fast_image_resize>
- `walkdir`: <https://docs.rs/walkdir/>
- `clap`: <https://docs.rs/clap>
- Unicode Collation Algorithm: <https://www.unicode.org/reports/tr10/>
- `kamadak-exif`: <https://docs.rs/kamadak-exif>

---

## 3. Cargo dependencies

```toml
[package]
name = "cullr"
version = "0.1.0"
edition = "2024"

[dependencies]
anyhow = "1"
thiserror = "2"
clap = { version = "4", features = ["derive"] }

ratatui = "0.30"
crossterm = "0.29"
ratatui-image = { version = "11", default-features = true, features = ["crossterm"] }

image = { version = "0.25", default-features = true }
fast_image_resize = { version = "6", features = ["rayon"] }

rayon = "1"
walkdir = "2"
jwalk = "0.8"
flume = "0.11"
lru = "0.14"
indexmap = "2"
parking_lot = "0.12"

which = "7"
time = { version = "0.3", features = ["formatting", "macros"] }
kamadak-exif = "0.6"

icu = { version = "2", features = ["compiled_data"] }

tracing = "0.1"
tracing-subscriber = "0.3"
```


---

## 4. CLI contract

Required:

```text
cullr -d <directory>
cullr --directory <directory>
cullr
cullr -h
cullr --help
```

Default directory is current working directory.

Suggested extra flags:

```text
--recursive                 Start in recursive mode
--file_ext                  User defined file extensions to include, comma-separated
--sort newest|oldest|name|name-desc
--backend auto|native|chafa|kitty|sixel
--allow-symbol-fallback     Allow ANSI/Unicode fallback; default false
--locale <locale>           Locale for filename sorting, default from environment
--cache-mb <number>         Memory-only thumbnail cache budget
--dry-run-delete            Confirm flow works but does not delete files
--hidden                    Include hidden files
```

Important: do not manually bind `-h`; let `clap` reserve it for help.

---

## 5. Preflight behavior

At startup:

1. Resolve and validate the directory.
2. Confirm stdout is an interactive terminal.
3. Query terminal size.
4. Detect a graphics-capable backend:
   - Try `ratatui_image::picker::Picker::from_query_stdio()`.
   - If it detects Kitty/Sixel/iTerm2, use native backend.
   - If native detection fails, check for `chafa` with `which::which("chafa")`.
   - If terminal graphics are unavailable and `--allow-symbol-fallback` is false, warn and exit.
5. If Windows is detected, prefer Sixel through Windows Terminal 1.22+ or Chafa. If probing fails, show a clear message: “Install/use a terminal with Kitty/Sixel/iTerm2 graphics support, or install Chafa.”
6. If no images are found, exit with a friendly message.

Do not silently fall back to low-quality ASCII unless the user opted in with `--allow-symbol-fallback`.

---

## 6. Core data model

Have Codex create these modules:

```text
src/
  main.rs
  cli.rs
  app.rs
  state.rs
  input.rs
  scanner.rs
  sorter.rs
  metadata.rs
  image_cache.rs
  thumbnail.rs
  renderer/
    mod.rs
    native.rs
    chafa.rs
  ui/
    mod.rs
    preview.rs
    grid.rs
    overlays.rs
    confirm.rs
  delete.rs
```

Core structs:

```rust
pub struct ImageEntry {
    pub path: PathBuf,
    pub file_name: OsString,
    pub display_name: String,
    pub extension: Option<String>,
    pub file_len: u64,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub discovered_order: usize,

    // Filled lazily
    pub dimensions: Option<(u32, u32)>,
    pub image_type: Option<ImageKind>,
    pub exif_date: Option<SystemTime>,
    pub exif_orientation: Option<u16>,
}

pub enum ViewMode {
    Preview,
    Grid,
    DeleteQueueGrid,
}

pub enum SortMode {
    Newest,
    Oldest,
    NameAsc,
    NameDesc,
}

pub enum ZoomMode {
    Fit,
    OriginalPixels,
}

pub struct AppState {
    pub directory: PathBuf,
    pub recursive: bool,
    pub entries: Vec<ImageEntry>,
    pub current_index: usize,

    pub mode: ViewMode,
    pub sort_mode: SortMode,
    pub zoom_mode: ZoomMode,

    pub delete_queue: IndexSet<PathBuf>,
    pub show_info_overlay: bool,
    pub show_help_overlay: bool,
    pub fullscreen_ui: bool,

    pub grid_page: usize,
    pub status_message: Option<String>,
    pub confirm_delete: bool,
}
```

Use `PathBuf`/`OsString` internally so foreign filenames survive round-trips. Only convert to display strings at the UI edge.

---

## 7. Scanning and filtering
## 7. Scanning and filtering

Initial scan should be fast and metadata-first.

Use two scan paths:

```text
non-recursive scan:
  std::fs::read_dir

recursive scan:
  jwalk

Initial scan should be fast and metadata-first:

1. Read file entries from the selected directory.
2. If recursive is false, only scan direct children.
3. If recursive is true, use `jwalk`.
4. Skip directories.
5. Skip symlink targets by default; optionally remove only the symlink itself if symlinks are later supported.
6. Filter likely image files by extension first:
   - jpg/jpeg
   - png
   - webp
   - gif
   - bmp
   - tiff/tif
   - avif
   - qoi
   - ico
7. Store `created`, `modified`, file size, and display name immediately.
8. Decode actual dimensions lazily when needed for current image, current grid page, or info overlay.

For normal scanning:

```rust
pub fn scan_directory(opts: ScanOptions) -> anyhow::Result<Vec<ImageEntry>> {
    if opts.recursive {
        scan_recursive_with_jwalk(opts)
    } else {
        scan_flat_with_read_dir(opts)
    }
}
```
In recursive mode:
```rust
use jwalk::{Parallelism, WalkDir};

let walker = WalkDir::new(&root)
    .parallelism(Parallelism::RayonDefaultPool {
        busy_timeout: std::time::Duration::from_millis(500),
    })
    .skip_hidden(!include_hidden);
```
Important performance note:

jwalk and thumbnail generation may both use Rayon. Keep scanning metadata-only, and do not decode or resize images inside the scanner. Decoding/resizing belongs in the thumbnail worker pipeline.

Rescan rules:

- Pressing `r` toggles recursive mode and rescans.
- Keep the current image selected if its canonical path still exists.
- If it disappeared, select the nearest valid index.
- Clear thumbnail jobs by increasing a generation token; workers should ignore stale results.

---

## 8. Sorting rules

`t` toggles time sorting:

```text
first t  -> newest
second t -> oldest
third t  -> newest
```

`n` toggles name sorting:

```text
first n  -> name ascending
second n -> name descending
third n  -> name ascending
```

When sorting by time, use:

```text
EXIF DateTimeOriginal if available
else filesystem created time if available
else modified time
else discovered order
```

For name sorting:

- Use ICU collation when `file_name` can be represented as Unicode.
- Use locale from `--locale`, then environment, then root/default locale.
- Fallback for invalid Unicode filenames: byte/platform order, then discovered order for stable tie-breaking.
- Do not lowercase manually; let collation handle case/diacritics where possible.

---

## 9. Keyboard map

Implement key handling centrally in `input.rs`. The event handler should return an `Action` enum, not mutate UI directly.

```rust
pub enum Action {
    Quit,
    Next,
    Previous,
    ToggleQueueCurrent,
    ShowDeleteQueueGrid,
    ToggleGrid,
    PageDown,
    PageUp,
    ConfirmDeleteQueued,
    ConfirmYes,
    ConfirmNo,
    ToggleFullscreenUi,
    ToggleRecursive,
    ToggleTimeSort,
    ToggleNameSort,
    ToggleInfoOverlay,
    ToggleHelpOverlay,
    ToggleZoom,
    Noop,
}
```

Required bindings:

| Key | Behavior |
|---|---|
| `j` | Next image |
| `k` | Previous image |
| `q` | Quit |
| `d` | Toggle current image to delete queue |
| `D` | View delete queue as grid |
| `g` | Toggle grid view |
| `z` | Toggle `ZoomMode::Fit` / `ZoomMode::OriginalPixels` |
| `ctrl+d` | Next grid page |
| `ctrl+u` | Previous grid page |
| `ctrl+r` | Open permanent delete confirmation popup |
| `y` | Confirm delete, only inside confirmation popup |
| `n`, `Esc` | Cancel delete popup |
| `f` | Toggle distraction-free full-terminal UI |
| `r` | Toggle recursive view and rescan |
| `t` | Toggle newest/oldest time sort |
| `n` | Toggle name/name-reversed sort, except in confirmation popup where `n` means no |
| `i` | Toggle image info overlay |
| `h` | Toggle shortcut help overlay |

Suggested extra bindings:

| Key | Behavior |
|---|---|
| `l` / `Enter` | Open highlighted grid image in preview |
| `h` / `left` | Previous image in grid, unless help overlay takes priority |
| `space` | Toggle queue current image |
| `/` | Filter/search filenames |
| `?` | Same as help |
| `Home` / `End` | First/last image |
| `R` | Rescan current directory |
| `m` | Toggle mark/keep separate from delete queue |

Because `n` has two meanings, the confirmation modal must capture keys before global bindings.

---

## 10. Preview rendering

Default preview behavior:

- Fill all available terminal area.
- Preserve original aspect ratio.
- Center image.
- Leave room for status line only when `fullscreen_ui == false`.
- Hide cursor.
- Use alternate screen.

`z` behavior:

```rust
ZoomMode::Fit
    => scale image down/up to fit available area while preserving aspect ratio

ZoomMode::OriginalPixels
    => display at native pixel dimensions where protocol/backend can map pixels;
       if larger than viewport, center-crop or top-left crop depending chosen option
```

Define “full screen” as app full-screen, not OS window fullscreen. The `f` key should hide status bars, borders, metadata, and help so the image gets the entire terminal canvas. Trying to force the terminal emulator itself into OS fullscreen is not portable enough for the core app.

---

## 11. Grid rendering

Grid layout algorithm:

```text
available = terminal area minus status/help
target cell min = 18 columns x 10 rows
cols = max(1, available.width / target_cell_width)
rows = max(1, available.height / target_cell_height)
page_size = cols * rows
page = current_index / page_size
```

For each visible cell:

1. Request thumbnail for `(path, file_len, modified, cell_width, cell_height, backend_id)`.
2. If ready, draw image.
3. If loading, draw placeholder with filename.
4. If failed, draw error placeholder.
5. Draw a border around each cell.
6. Use a visibly different border/title for the current image.
7. Show delete-queued marker, for example `DEL`, in the cell title.

`ctrl+d` and `ctrl+u` should move by one page. Keep current image index synchronized with page. In grid mode, `j/k` can still move linearly next/previous.

Delete queue grid uses the same component but the image list is:

```rust
entries.iter().filter(|entry| delete_queue.contains(&entry.path))
```

If delete queue is empty, show a centered message: “Delete queue is empty.”

---

## 12. Thumbnail pipeline

Rules:

- Never save thumbnails to disk.
- Never create a cache directory.
- Never create temp image files.
- Cache only decoded/resized image buffers or protocol payloads in memory.
- Thumbnail workers must never write to terminal directly.
- Only UI/render thread writes terminal output.

Pipeline:

```text
UI asks thumbnail_cache.get_or_request(key)
    -> if cached: return ready payload
    -> if missing: enqueue job and return Loading

Worker receives job
    -> decode source image
    -> apply EXIF orientation
    -> resize to target pixel dimensions
    -> encode/render protocol payload if backend needs it
    -> send result to UI channel

UI receives result
    -> insert into LRU memory cache
    -> request redraw
```

Use a bounded queue so fast scrolling does not produce infinite stale work. Add a generation counter:

```rust
struct ThumbJob {
    key: ThumbKey,
    generation: u64,
}
```

When directory, sort mode, recursive flag, or backend changes, increment generation. Workers can finish old jobs, but UI should discard old-generation results.

Prefetch strategy:

- Preview mode: current image, next 2, previous 2.
- Grid mode: current page, next page, previous page.
- Delete queue grid: current delete queue page only.

Memory budget:

```text
default thumbnail cache: 256 MB
preview decoded cache: 2–4 full images max
evict by least-recently-used
```

---

## 13. Renderer abstraction

Create a trait like:

```rust
pub trait ImageRenderer {
    fn backend_id(&self) -> RendererBackendId;
    fn preflight(&mut self) -> anyhow::Result<RendererCapabilities>;
    fn render_preview(&mut self, frame: &mut Frame, area: Rect, entry: &ImageEntry, zoom: ZoomMode);
    fn render_thumbnail(&mut self, frame: &mut Frame, area: Rect, thumb: &ThumbnailPayload);
    fn clear(&mut self) -> anyhow::Result<()>;
}
```

Implement at least:

```text
NativeRatatuiImageRenderer
ChafaRenderer, optional / feature-gated
```

Native renderer:

- Uses `ratatui-image`.
- Uses `Picker`.
- Uses `ThreadProtocol` or background thread pathway for resize/encode.
- Best for interactive grid and overlays.

Chafa renderer:

- Use `chafa --probe` during preflight.
- Use `--format kitty|sixels|iterm` when backend is known.
- Use `--size WxH` or `--view-size WxH`.
- Pipe in-memory thumbnails via stdin if rendering generated thumbnails.
- Do not let multiple Chafa processes write to terminal concurrently.
- Treat Chafa as fallback or user-forced backend, not the first implementation path for every grid cell.

---

## 14. Delete queue and permanent deletion

Delete queue behavior:

- `d` toggles current image in the delete queue.
- Adding an already queued file is idempotent.
- Optional but strongly recommended: `u` removes current image from queue.
- Status line shows queue count: `queued: 12`.

`ctrl+r` behavior:

1. Open modal popup:

   ```text
   Permanently delete 12 files?
   This cannot be undone.
   Press y to delete, n/Esc to cancel.
   ```

2. Only `y` proceeds.
3. `n` or `Esc` cancels.
4. During deletion:
   - Leave raw mode active.
   - Show progress/status.
   - Use `std::fs::remove_file`.
   - Do not delete directories.
   - Do not follow symlinks.
   - Remove successfully deleted files from `entries`.
   - Keep failed files in queue and show error summary.
5. Clamp `current_index`.

Safety checks before deleting:

```text
- Only delete paths discovered by scanner.
- Canonicalize and ensure path is still under selected directory unless user explicitly disables this later.
- Re-read symlink metadata; skip if no longer a normal file.
- Optionally compare file_len and modified time with queued metadata.
```

Even though the requested behavior is permanent deletion, add `--dry-run-delete` for testing and CI.

---

## 15. Info overlay

`i` toggles an overlay on top of the current image.

Display:

```text
filename
width x height
megapixels
file type
file size
date
path
delete queued: yes/no
index: 42 / 500
```

Megapixels:

```rust
mp = (width as f64 * height as f64) / 1_000_000.0
```

Date priority:

```text
EXIF DateTimeOriginal
created
modified
unknown
```

Use a semi-compact centered or top-left block so it does not eat the whole image.

---

## 16. Help overlay

`h` toggles shortcut help.

Suggested layout:

```text
Navigation
  j/k        next/previous
  g          grid/preview
  ctrl+d/u   next/previous page
  q          quit

Culling
  d          queue for deletion
  u          unqueue
  D          show delete queue
  ctrl+r     permanently delete queued images

View
  z          fit/original pixels
  i          image info
  f          full-terminal UI
  h          help

Sorting/scan
  r          recursive on/off
  t          newest/oldest
  n          name/name reversed
```

---

## 17. Error handling

Use a non-crashing UX:

- Bad image decode: show placeholder, keep app running.
- Permission denied: show status, skip file.
- Terminal resize: recompute layout, invalidate size-specific thumbnail keys.
- Renderer failure: show error popup and suggest supported terminal/tool.
- Delete failure: keep failed file in queue with error list.
- Empty directory: exit before entering TUI.

Use `tracing` for debug logs, but do not write logs into the image directory. If logging is needed, write to stderr before entering TUI or behind a `--log-file` option chosen by user. Default should not create files.

---

## 18. Testing plan

Unit tests:

```text
scanner:
  non-recursive excludes subfolders
  recursive includes subfolders
  hidden handling works
  extension filter works

sorter:
  newest/oldest stable fallback
  name asc/desc stable fallback
  Unicode filenames do not panic

delete queue:
  d is idempotent
  u removes
  current index clamps after deletion

grid:
  page size calculation
  ctrl+d/u page boundaries
  current image remains highlighted after page move

thumbnail cache:
  same key returns cached item
  resize key changes when cell size changes
  generation mismatch discards stale result
```

Integration tests:

```text
- Create temp input directory with sample images.
- Run scanner and thumbnail generation.
- Assert no new files appear in input directory after thumbnail generation.
- Run dry-run delete and assert files remain.
- Run real delete inside temp directory and assert queued files removed.
```

Manual acceptance checklist:

```text
- Starts in current directory if -d not provided.
- -d and --directory work.
- -h and --help work.
- q exits cleanly and restores terminal.
- j/k are instant after first decode.
- g shows grid thumbnails.
- current grid image is highlighted.
- ctrl+d/u page correctly.
- d queues image.
- D shows only delete queue.
- ctrl+r asks y/n before deleting.
- n/Esc cancels delete.
- y deletes queued images permanently.
- r rescans recursively/non-recursively.
- t toggles newest/oldest.
- n toggles name/name reversed outside modal.
- i overlay shows width/height/MP/type/date.
- h overlay shows shortcuts.
- f gives image the full terminal canvas.
- thumbnails are never written to disk.
```

---

## 19. Codex implementation sequence

### Phase 1: Project skeleton

Create the Cargo project, module layout, CLI parser, and a stub app loop. Add a `--dry-run-delete` flag immediately so deletion can be tested safely.

### Phase 2: Scanner and sorter

Implement image discovery, metadata collection, recursive toggle, and sorting. Add tests before building the UI.

### Phase 3: App state and key handling

Implement `AppState`, `Action`, and key mapping with crossterm events. Add unit tests for key-to-action mapping, especially uppercase `D`, `ctrl+d`, `ctrl+u`, and `ctrl+r`.

### Phase 4: Basic TUI shell

Enter alternate screen/raw mode, draw status bar, handle quit, and restore terminal on panic/error. No images yet.

### Phase 5: Native preview renderer

Add `ratatui-image` preview rendering for current image. Implement `j`, `k`, `z`, `i`, `h`, and resize handling.

### Phase 6: Memory-only thumbnail cache

Implement worker pool, resize jobs, LRU cache, and stale-generation discard. Confirm no disk writes.

### Phase 7: Grid view

Implement grid layout, current highlight, thumbnail rendering, page up/down, `g`, and opening highlighted image in preview.

### Phase 8: Delete queue grid and confirmation

Implement `d`, `D`, `ctrl+r`, modal y/n, dry-run delete, real delete, and error summaries.

### Phase 9: Recursive rescan and sorting UX

Implement `r`, `t`, `n`, current image preservation across resort/rescan, and locale-aware filename sorting.

### Phase 10: External renderer preflight

Add Chafa/Kitty/Sixel preflight. Warn and exit if no real image-capable renderer exists and fallback is disabled.

### Phase 11: Polish and performance

Add prefetching, memory budget, better placeholders, metadata overlay polish, and tracing.

---

## 20. Extra features worth adding later

The most valuable culling features after the MVP:

- `space` toggle queue.
- Move-to-trash mode as a safer alternative to permanent deletion.
- Sidecar session file: save delete queue/review progress only when user explicitly asks.
- Compare mode: two-up or before/after adjacent images.
- Star/rating labels.
- Filter by extension, date range, filename search.
- Duplicate detection by perceptual hash.
- EXIF orientation correction.
- Open current image in external editor.
- Mouse wheel grid scrolling.
- Config file for keybindings.
- Optional “move rejected files to folder” workflow instead of delete.
- Video thumbnail support later, but keep it separate from image MVP.

---
---

## 22. Architectural principle

Keep the UI, state, cache, sorting, deletion, and scanning native to the Rust app. Hide terminal graphics weirdness behind a renderer trait.

The big invariant is:

> Thumbnails are generated fast, kept in memory only, and never written to disk.
