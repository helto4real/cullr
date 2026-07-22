use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};

use flume::{Receiver, Sender};

use crate::{
    browser::read_browser_entries_with_sort,
    scanner::{ScanOptions, scan_directory, scan_files},
    state::{BrowserEntry, MediaEntry, SortMode},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaRefreshScope {
    Directory(ScanOptions),
    SelectedFiles {
        files: Vec<PathBuf>,
        extensions: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRefreshScope {
    pub directory: PathBuf,
    pub include_hidden: bool,
    pub extensions: Vec<String>,
    pub sort_mode: SortMode,
}

#[derive(Debug)]
pub enum RefreshResult {
    Media {
        generation: u64,
        scope: MediaRefreshScope,
        result: Result<Vec<MediaEntry>, String>,
    },
    Browser {
        generation: u64,
        scope: BrowserRefreshScope,
        result: Result<Vec<BrowserEntry>, String>,
    },
}

#[derive(Debug)]
struct MediaRequest {
    generation: u64,
    scope: MediaRefreshScope,
}

#[derive(Debug)]
struct BrowserRequest {
    generation: u64,
    scope: BrowserRefreshScope,
}

#[derive(Debug, Default)]
struct PendingRefreshes {
    media: Option<MediaRequest>,
    browser: Option<BrowserRequest>,
}

pub struct RefreshService {
    wake_tx: Sender<()>,
    pending: Arc<Mutex<PendingRefreshes>>,
    result_rx: Receiver<RefreshResult>,
}

impl RefreshService {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (wake_tx, wake_rx) = flume::bounded(1);
        let (result_tx, result_rx) = flume::unbounded();
        let pending = Arc::new(Mutex::new(PendingRefreshes::default()));
        let worker_pending = pending.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        thread::Builder::new()
            .name("cullr-live-refresh".to_owned())
            .spawn(move || refresh_worker(wake_rx, worker_pending, result_tx, repaint))
            .expect("failed to start live refresh worker");

        Self {
            wake_tx,
            pending,
            result_rx,
        }
    }

    pub fn request_media(&self, generation: u64, scope: MediaRefreshScope) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.media = Some(MediaRequest { generation, scope });
        drop(pending);
        let _ = self.wake_tx.try_send(());
    }

    pub fn request_browser(&self, generation: u64, scope: BrowserRefreshScope) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.browser = Some(BrowserRequest { generation, scope });
        drop(pending);
        let _ = self.wake_tx.try_send(());
    }

    pub fn drain(&self) -> Vec<RefreshResult> {
        self.result_rx.try_iter().collect()
    }
}

fn refresh_worker(
    wake_rx: Receiver<()>,
    pending: Arc<Mutex<PendingRefreshes>>,
    result_tx: Sender<RefreshResult>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    while wake_rx.recv().is_ok() {
        let pending = {
            let mut guard = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *guard)
        };
        if let Some(request) = pending.media {
            let result = scan_media(&request.scope).map_err(|error| format!("{error:#}"));
            if result_tx
                .send(RefreshResult::Media {
                    generation: request.generation,
                    scope: request.scope,
                    result,
                })
                .is_err()
            {
                break;
            }
            repaint();
        }
        if let Some(request) = pending.browser {
            let result = read_browser_entries_with_sort(
                &request.scope.directory,
                request.scope.include_hidden,
                &request.scope.extensions,
                request.scope.sort_mode,
            )
            .map_err(|error| format!("{error:#}"));
            if result_tx
                .send(RefreshResult::Browser {
                    generation: request.generation,
                    scope: request.scope,
                    result,
                })
                .is_err()
            {
                break;
            }
            repaint();
        }
    }
}

fn scan_media(scope: &MediaRefreshScope) -> anyhow::Result<Vec<MediaEntry>> {
    match scope {
        MediaRefreshScope::Directory(options) => scan_directory(options.clone()),
        MediaRefreshScope::SelectedFiles { files, extensions } => scan_files(files, extensions),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};
    use tempfile::tempdir;

    #[test]
    fn worker_scans_media_without_blocking_the_caller() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("image.jpg"), b"image").unwrap();
        let service = RefreshService::new(|| {});
        let scope = MediaRefreshScope::Directory(ScanOptions {
            root: temp.path().to_path_buf(),
            recursive: false,
            include_hidden: false,
            extensions: vec!["jpg".to_owned()],
        });

        service.request_media(7, scope.clone());
        let result = service
            .result_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap();

        match result {
            RefreshResult::Media {
                generation,
                scope: returned_scope,
                result,
            } => {
                assert_eq!(generation, 7);
                assert_eq!(returned_scope, scope);
                assert_eq!(result.unwrap().len(), 1);
            }
            RefreshResult::Browser { .. } => panic!("expected media refresh"),
        }
    }
}
