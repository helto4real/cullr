# cullr

`cullr` is a fast desktop image viewer and culling tool for reviewing a folder
of images, marking rejects, and deleting the queued files only after an explicit
confirmation step.

![cullr workflow infographic](docs/cullr-infographic.png)

## What It Does

- Opens an image directory, or opens a specific file positioned inside its
  parent directory.
- Shows a large preview view and a thumbnail grid view for fast review.
- Decodes images on worker threads and uploads them as GPU textures, so the
  window can resize without re-decoding every frame.
- Uses libjpeg-turbo scaled decode for large JPEG previews, with other formats
  decoded through the Rust `image` crate.
- Keeps preview and thumbnail data in memory; it does not write image cache
  files into the input directory.
- Lets you queue images for deletion, inspect the queue, and confirm before
  files are removed.

## Quick Start

Run from source with Cargo:

```sh
cargo run -- /path/to/images
```

Scan subfolders too:

```sh
cargo run -- --recursive /path/to/images
```

Try the delete flow without removing files:

```sh
cargo run -- --dry-run-delete /path/to/images
```

Build a release binary:

```sh
cargo build --release
./target/release/cullr /path/to/images
```

## CLI Options

```text
Usage: cullr [OPTIONS] [PATH]
```

| Option | Description |
| --- | --- |
| `PATH` | Image file or directory to open. A file opens its folder positioned on that file. |
| `-d, --directory <DIR>` | Directory to open. |
| `--recursive` | Include images in subdirectories. |
| `--file_ext <EXTS>` | Comma-separated extensions to include, for example `jpg,png,webp`. |
| `--sort <SORT>` | Initial sort: `newest`, `oldest`, `name`, or `name-desc`. |
| `--locale <LOCALE>` | Locale to use for name sorting, for example `sv` or `en`. |
| `--dry-run-delete` | Exercise the delete flow without deleting files. |
| `--hidden` | Include hidden files and directories. |

If no path or directory is supplied, `cullr` opens the current working
directory.

## Keyboard Shortcuts

| Key | Action |
| --- | --- |
| `h` / `k` / left / up | Previous image in preview mode. |
| `l` / `j` / right / down | Next image in preview mode. |
| `g` | Toggle between preview and grid. |
| `enter` | Open the highlighted grid image in preview mode. |
| `h` / `l` | Move left or right in grid mode. |
| `j` / `k` | Move down or up one row in grid mode. |
| `ctrl+d` / `ctrl+u` | Move half a page down or up in grid mode. |
| `home` / `end` | Jump to the first or last image. |
| `space` / `d` | Toggle the current image in the delete queue. |
| `u` | Remove the current image from the delete queue. |
| `shift+D` | Show the delete queue grid. |
| `ctrl+R` | Confirm deletion for queued files. |
| `y` / `n` | Accept or cancel the delete confirmation. |
| `z` | Toggle fit-to-window and original-pixels zoom. |
| `f` | Toggle fullscreen window mode. |
| `t` | Cycle time sorting. |
| `n` | Cycle name sorting. |
| `r` | Toggle recursive scanning and rescan. |
| `shift+R` | Rescan the current directory. |
| `i` | Toggle the info overlay. |
| `?` | Toggle help. |
| `q` / `esc` | Quit, close overlays, or leave grid mode depending on context. |

## Supported Formats

By default, `cullr` scans for:

```text
jpg, jpeg, png, webp, gif, bmp, tiff, tif, avif, qoi, ico
```

Use `--file_ext` to choose a different comma-separated extension set.

## Delete Safety

Deletion is intentionally staged:

- Mark files with `space` or `d`.
- Review the queue with `shift+D`.
- Press `ctrl+R`, then confirm with `y`.

Before deleting, `cullr` checks that each queued path still belongs to the
selected directory, is a real file rather than a symlink, and has not changed
size or modification time since it was scanned. `--dry-run-delete` keeps the
same flow but leaves all files on disk.

## Development

Run the test suite:

```sh
cargo test
```

The tests cover scanning, sorting, decode sizing, delete safety, and the
dry-run delete path.

## License

MIT
