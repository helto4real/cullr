use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use crate::state::{MediaMode, SortMode};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "cullr",
    version,
    about = "Fast GPU-windowed media viewer and culler"
)]
pub struct Cli {
    /// Media file or directory to open. A file opens its folder positioned on it.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    #[arg(short = 'd', long = "directory", value_name = "DIR")]
    pub directory: Option<PathBuf>,

    #[arg(long)]
    pub recursive: bool,

    #[arg(long = "file_ext", value_name = "EXTS")]
    pub file_ext: Option<String>,

    #[arg(long, value_enum, default_value_t = CliMediaMode::Both)]
    pub media: CliMediaMode,

    #[arg(long, value_enum)]
    pub sort: Option<CliSortMode>,

    #[arg(long)]
    pub locale: Option<String>,

    #[arg(long)]
    pub dry_run_delete: bool,

    #[arg(long)]
    pub hidden: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum CliSortMode {
    Newest,
    Oldest,
    Name,
    NameDesc,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum CliMediaMode {
    Both,
    Image,
    Video,
}

impl From<CliMediaMode> for MediaMode {
    fn from(value: CliMediaMode) -> Self {
        match value {
            CliMediaMode::Both => Self::Both,
            CliMediaMode::Image => Self::Image,
            CliMediaMode::Video => Self::Video,
        }
    }
}

impl Cli {
    pub fn resolved_extensions(&self) -> Vec<String> {
        let media_mode = MediaMode::from(self.media);
        self.file_ext.as_deref().map_or_else(
            || default_extensions_for(media_mode),
            |raw| {
                raw.split(',')
                    .map(|part| part.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|part| !part.is_empty() && media_mode.allows_extension(part))
                    .collect()
            },
        )
    }

    pub fn initial_sort_mode(&self) -> SortMode {
        match self.sort {
            Some(CliSortMode::Newest) => SortMode::Newest,
            Some(CliSortMode::Oldest) => SortMode::Oldest,
            Some(CliSortMode::Name) => SortMode::NameAsc,
            Some(CliSortMode::NameDesc) => SortMode::NameDesc,
            None => SortMode::Discovered,
        }
    }
}

pub fn default_extensions() -> Vec<String> {
    default_extensions_for(MediaMode::Both)
}

pub fn default_extensions_for(media_mode: MediaMode) -> Vec<String> {
    media_mode.default_extensions()
}

pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "tiff", "tif", "avif", "qoi", "ico",
];

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "mpg", "mpeg", "m2v", "ts", "m2ts", "mts", "wmv",
    "flv", "3gp", "3g2", "ogv",
];

fn owned(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

impl MediaMode {
    pub fn default_extensions(self) -> Vec<String> {
        match self {
            Self::Both => IMAGE_EXTENSIONS
                .iter()
                .chain(VIDEO_EXTENSIONS)
                .map(|value| (*value).to_owned())
                .collect(),
            Self::Image => owned(IMAGE_EXTENSIONS),
            Self::Video => owned(VIDEO_EXTENSIONS),
        }
    }

    pub fn allows_extension(self, ext: &str) -> bool {
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        match self {
            Self::Both => is_image_extension(&ext) || is_video_extension(&ext),
            Self::Image => is_image_extension(&ext),
            Self::Video => is_video_extension(&ext),
        }
    }
}

pub fn is_image_extension(ext: &str) -> bool {
    IMAGE_EXTENSIONS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(ext))
}

pub fn is_video_extension(ext: &str) -> bool {
    VIDEO_EXTENSIONS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_extensions_include_images_and_videos() {
        let cli = Cli {
            path: None,
            directory: None,
            recursive: false,
            file_ext: None,
            media: CliMediaMode::Both,
            sort: None,
            locale: None,
            dry_run_delete: false,
            hidden: false,
        };

        let extensions = cli.resolved_extensions();

        assert!(extensions.contains(&"jpg".to_owned()));
        assert!(extensions.contains(&"mp4".to_owned()));
    }

    #[test]
    fn media_mode_filters_explicit_extensions() {
        let cli = Cli {
            path: None,
            directory: None,
            recursive: false,
            file_ext: Some("jpg,mp4,txt".to_owned()),
            media: CliMediaMode::Video,
            sort: None,
            locale: None,
            dry_run_delete: false,
            hidden: false,
        };

        assert_eq!(cli.resolved_extensions(), vec!["mp4".to_owned()]);
    }

    #[test]
    fn image_and_video_defaults_are_separate() {
        assert!(!default_extensions_for(MediaMode::Image).contains(&"mp4".to_owned()));
        assert!(!default_extensions_for(MediaMode::Video).contains(&"jpg".to_owned()));
    }
}
