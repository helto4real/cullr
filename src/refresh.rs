use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};

use flume::{Receiver, Sender};

use crate::{
    browser::read_browser_entries_with_sort,
    scanner::{ScanOptions, scan_directory, scan_files},
    sorter,
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
        sort_mode: SortMode,
        result: Result<MediaRefreshOutput, String>,
    },
    Browser {
        generation: u64,
        scope: BrowserRefreshScope,
        result: Result<Vec<BrowserEntry>, String>,
    },
}

#[derive(Debug)]
pub struct MediaRefreshOutput {
    pub entries: Vec<MediaEntry>,
    pub unchanged_paths: HashSet<PathBuf>,
    pub invalidated_paths: HashSet<PathBuf>,
    pub added: usize,
    pub removed: usize,
    pub updated: usize,
}

impl MediaRefreshOutput {
    pub fn changed(&self) -> bool {
        self.added > 0 || self.removed > 0 || self.updated > 0
    }
}

#[derive(Debug)]
struct MediaRequest {
    generation: u64,
    scope: MediaRefreshScope,
    previous_entries: Vec<MediaEntry>,
    sort_mode: SortMode,
    locale: Option<String>,
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

#[derive(Debug, Default)]
struct PendingResults {
    media: Option<RefreshResult>,
    browser: Option<RefreshResult>,
}

pub struct RefreshService {
    wake_tx: Sender<()>,
    pending: Arc<Mutex<PendingRefreshes>>,
    result_wake_rx: Receiver<()>,
    results: Arc<Mutex<PendingResults>>,
}

impl RefreshService {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (wake_tx, wake_rx) = flume::bounded(1);
        let (result_wake_tx, result_wake_rx) = flume::bounded(1);
        let pending = Arc::new(Mutex::new(PendingRefreshes::default()));
        let results = Arc::new(Mutex::new(PendingResults::default()));
        let worker_pending = pending.clone();
        let worker_results = results.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        thread::Builder::new()
            .name("cullr-live-refresh".to_owned())
            .spawn(move || {
                refresh_worker(
                    wake_rx,
                    worker_pending,
                    result_wake_tx,
                    worker_results,
                    repaint,
                );
            })
            .expect("failed to start live refresh worker");

        Self {
            wake_tx,
            pending,
            result_wake_rx,
            results,
        }
    }

    pub fn request_media(
        &self,
        generation: u64,
        scope: MediaRefreshScope,
        previous_entries: Vec<MediaEntry>,
        sort_mode: SortMode,
        locale: Option<String>,
    ) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.media = Some(MediaRequest {
            generation,
            scope,
            previous_entries,
            sort_mode,
            locale,
        });
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
        if self.result_wake_rx.try_recv().is_err() {
            return Vec::new();
        }
        let mut results = self
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut drained = Vec::with_capacity(2);
        if let Some(result) = results.media.take() {
            drained.push(result);
        }
        if let Some(result) = results.browser.take() {
            drained.push(result);
        }
        drained
    }
}

fn refresh_worker(
    wake_rx: Receiver<()>,
    pending: Arc<Mutex<PendingRefreshes>>,
    result_wake_tx: Sender<()>,
    results: Arc<Mutex<PendingResults>>,
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
            let result = scan_media(&request.scope)
                .map(|fresh| {
                    prepare_media_refresh(
                        request.previous_entries,
                        fresh,
                        request.sort_mode,
                        request.locale.as_deref(),
                    )
                })
                .map_err(|error| format!("{error:#}"));
            publish_result(
                RefreshResult::Media {
                    generation: request.generation,
                    scope: request.scope,
                    sort_mode: request.sort_mode,
                    result,
                },
                &result_wake_tx,
                &results,
            );
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
            publish_result(
                RefreshResult::Browser {
                    generation: request.generation,
                    scope: request.scope,
                    result,
                },
                &result_wake_tx,
                &results,
            );
            repaint();
        }
    }
}

fn publish_result(result: RefreshResult, wake_tx: &Sender<()>, pending: &Mutex<PendingResults>) {
    let mut pending = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match &result {
        RefreshResult::Media { .. } => pending.media = Some(result),
        RefreshResult::Browser { .. } => pending.browser = Some(result),
    }
    drop(pending);
    let _ = wake_tx.try_send(());
}

fn scan_media(scope: &MediaRefreshScope) -> anyhow::Result<Vec<MediaEntry>> {
    match scope {
        MediaRefreshScope::Directory(options) => scan_directory(options.clone()),
        MediaRefreshScope::SelectedFiles { files, extensions } => scan_files(files, extensions),
    }
}

pub(crate) fn prepare_media_refresh(
    old: Vec<MediaEntry>,
    fresh: Vec<MediaEntry>,
    sort_mode: SortMode,
    locale: Option<&str>,
) -> MediaRefreshOutput {
    let old_by_path = old
        .iter()
        .map(|entry| (entry.path.as_path(), entry))
        .collect::<HashMap<_, _>>();
    let fresh_paths = fresh
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    let mut unchanged_paths = HashSet::new();
    let mut invalidated_paths = old
        .iter()
        .filter(|entry| !fresh_paths.contains(&entry.path))
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    let removed = invalidated_paths.len();
    let mut added = 0;
    let mut updated = 0;
    let mut next_order = old
        .iter()
        .map(|entry| entry.discovered_order)
        .max()
        .map_or(0, |order| order.saturating_add(1));
    let mut entries = Vec::with_capacity(fresh.len());

    for mut entry in fresh {
        match old_by_path.get(entry.path.as_path()) {
            Some(previous) if same_scanned_file(previous, &entry) => {
                unchanged_paths.insert(entry.path.clone());
                entries.push((*previous).clone());
            }
            Some(previous) => {
                entry.discovered_order = previous.discovered_order;
                invalidated_paths.insert(entry.path.clone());
                updated += 1;
                entries.push(entry);
            }
            None => {
                entry.discovered_order = next_order;
                next_order = next_order.saturating_add(1);
                added += 1;
                entries.push(entry);
            }
        }
    }

    let mut output = MediaRefreshOutput {
        entries,
        unchanged_paths,
        invalidated_paths,
        added,
        removed,
        updated,
    };
    if output.changed() {
        sorter::sort_entries(&mut output.entries, sort_mode, locale);
    }
    output
}

fn same_scanned_file(left: &MediaEntry, right: &MediaEntry) -> bool {
    left.file_len == right.file_len
        && left.modified == right.modified
        && left.media_kind == right.media_kind
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ImageKind, MediaKind};
    use std::{
        ffi::OsString,
        fs,
        time::{Duration, Instant},
    };
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

        service.request_media(7, scope.clone(), Vec::new(), SortMode::Discovered, None);
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Some(result) = service.drain().pop() {
                break result;
            }
            assert!(Instant::now() < deadline, "timed out waiting for refresh");
            thread::sleep(Duration::from_millis(10));
        };

        match result {
            RefreshResult::Media {
                generation,
                scope: returned_scope,
                result,
                ..
            } => {
                assert_eq!(generation, 7);
                assert_eq!(returned_scope, scope);
                assert_eq!(result.unwrap().entries.len(), 1);
            }
            RefreshResult::Browser { .. } => panic!("expected media refresh"),
        }
    }

    #[test]
    fn latest_media_result_replaces_older_undrained_result() {
        let (wake_tx, wake_rx) = flume::bounded(1);
        let pending = Mutex::new(PendingResults::default());
        let scope = MediaRefreshScope::SelectedFiles {
            files: Vec::new(),
            extensions: Vec::new(),
        };
        for generation in [1, 2] {
            publish_result(
                RefreshResult::Media {
                    generation,
                    scope: scope.clone(),
                    sort_mode: SortMode::Discovered,
                    result: Ok(MediaRefreshOutput {
                        entries: Vec::new(),
                        unchanged_paths: HashSet::new(),
                        invalidated_paths: HashSet::new(),
                        added: 0,
                        removed: 0,
                        updated: 0,
                    }),
                },
                &wake_tx,
                &pending,
            );
        }

        assert!(wake_rx.try_recv().is_ok());
        assert!(wake_rx.try_recv().is_err());
        let result = pending.lock().unwrap().media.take().unwrap();
        assert!(matches!(result, RefreshResult::Media { generation: 2, .. }));
    }

    #[test]
    fn reconciliation_preserves_enrichment_and_order_for_unchanged_entries() {
        let mut old = media_entry("existing.jpg", 7);
        old.file_len = 12;
        old.dimensions = Some((1920, 1080));
        old.dimensions_attempted = true;
        let mut fresh = old.clone();
        fresh.discovered_order = 0;
        fresh.dimensions = None;
        fresh.dimensions_attempted = false;
        let added = media_entry("new.jpg", 0);

        let result =
            prepare_media_refresh(vec![old], vec![fresh, added], SortMode::Discovered, None);

        assert_eq!(result.added, 1);
        assert_eq!(result.updated, 0);
        assert_eq!(result.entries[0].discovered_order, 7);
        assert_eq!(result.entries[0].dimensions, Some((1920, 1080)));
        assert_eq!(result.entries[1].discovered_order, 8);
    }

    fn media_entry(name: &str, discovered_order: usize) -> MediaEntry {
        MediaEntry {
            path: PathBuf::from(name),
            file_name: OsString::from(name),
            display_name: name.to_owned(),
            extension: Some("jpg".to_owned()),
            file_len: 0,
            created: None,
            modified: None,
            discovered_order,
            dimensions: None,
            media_kind: MediaKind::Image(ImageKind::Jpeg),
            exif_date: None,
            exif_orientation: None,
            dimensions_attempted: false,
            exif_attempted: false,
        }
    }
}
