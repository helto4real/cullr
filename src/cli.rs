use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use crate::state::SortMode;

#[derive(Debug, Clone, Parser)]
#[command(name = "cullr", version, about = "Fast GPU-windowed image viewer and culler")]
pub struct Cli {
    /// Image file or directory to open. A file opens its folder positioned on it.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    #[arg(short = 'd', long = "directory", value_name = "DIR")]
    pub directory: Option<PathBuf>,

    #[arg(long)]
    pub recursive: bool,

    #[arg(long = "file_ext", value_name = "EXTS")]
    pub file_ext: Option<String>,

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

impl Cli {
    pub fn resolved_extensions(&self) -> Vec<String> {
        self.file_ext
            .as_deref()
            .map(|raw| {
                raw.split(',')
                    .map(|part| part.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|part| !part.is_empty())
                    .collect()
            })
            .unwrap_or_else(default_extensions)
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
    [
        "jpg", "jpeg", "png", "webp", "gif", "bmp", "tiff", "tif", "avif", "qoi", "ico",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
