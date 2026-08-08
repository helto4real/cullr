# Vendored FFmpeg bindings

Cullr vendors the published `ffmpeg-next` and `ffmpeg-sys-next` 8.1.0 crates so
it can carry a small FFmpeg 9 compatibility patch until upstream provides the
same support.

The local changes:

- detect FFmpeg 9 from libavcodec 63;
- use `avcodec_get_supported_config()` after the legacy `AVCodec` fields were
  removed;
- account for codec IDs removed or added in FFmpeg 9; and
- account for frame and packet side-data variants added in FFmpeg 9.

`Cargo.toml` overrides only these two crate versions through
`[patch.crates-io]`. Remove the override and this directory once an upstream
release with equivalent FFmpeg 9 support is adopted.
