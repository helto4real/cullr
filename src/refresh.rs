use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use flume::{Receiver, Sender};

use crate::{
    browser::read_browser_entries_with_sort_cancellable,
    scanner::{ScanOptions, scan_directory_cancellable, scan_files_cancellable},
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
    epoch: u64,
    scope: MediaRefreshScope,
    previous_entries: Vec<MediaEntry>,
    sort_mode: SortMode,
    locale: Option<String>,
}

#[derive(Debug)]
struct BrowserRequest {
    generation: u64,
    epoch: u64,
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
    media_epoch: Arc<AtomicU64>,
    browser_epoch: Arc<AtomicU64>,
}

impl RefreshService {
    pub fn new(suspended: Arc<AtomicBool>, repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (wake_tx, wake_rx) = flume::bounded(1);
        let (result_wake_tx, result_wake_rx) = flume::bounded(1);
        let pending = Arc::new(Mutex::new(PendingRefreshes::default()));
        let results = Arc::new(Mutex::new(PendingResults::default()));
        let worker_pending = pending.clone();
        let worker_results = results.clone();
        let media_epoch = Arc::new(AtomicU64::new(0));
        let browser_epoch = Arc::new(AtomicU64::new(0));
        let worker_suspended = Arc::clone(&suspended);
        let worker_media_epoch = Arc::clone(&media_epoch);
        let worker_browser_epoch = Arc::clone(&browser_epoch);
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        thread::Builder::new()
            .name("cullr-live-refresh".to_owned())
            .spawn(move || {
                RefreshWorker {
                    wake_rx,
                    pending: worker_pending,
                    result_wake_tx,
                    results: worker_results,
                    suspended: worker_suspended,
                    media_epoch: worker_media_epoch,
                    browser_epoch: worker_browser_epoch,
                    repaint,
                }
                .run();
            })
            .expect("failed to start live refresh worker");

        Self {
            wake_tx,
            pending,
            result_wake_rx,
            results,
            media_epoch,
            browser_epoch,
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
        let epoch = self.media_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.media = Some(MediaRequest {
            generation,
            epoch,
            scope,
            previous_entries,
            sort_mode,
            locale,
        });
        drop(pending);
        let _ = self.wake_tx.try_send(());
    }

    pub fn request_browser(&self, generation: u64, scope: BrowserRefreshScope) {
        let epoch = self.browser_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.browser = Some(BrowserRequest {
            generation,
            epoch,
            scope,
        });
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

    pub fn cancel_all(&self) {
        self.media_epoch.fetch_add(1, Ordering::AcqRel);
        self.browser_epoch.fetch_add(1, Ordering::AcqRel);
    }
}

struct RefreshWorker {
    wake_rx: Receiver<()>,
    pending: Arc<Mutex<PendingRefreshes>>,
    result_wake_tx: Sender<()>,
    results: Arc<Mutex<PendingResults>>,
    suspended: Arc<AtomicBool>,
    media_epoch: Arc<AtomicU64>,
    browser_epoch: Arc<AtomicU64>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl RefreshWorker {
    fn run(self) {
        while self.wake_rx.recv().is_ok() {
            let pending = {
                let mut guard = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                std::mem::take(&mut *guard)
            };
            if let Some(request) = pending.media {
                let MediaRequest {
                    generation,
                    epoch,
                    scope,
                    previous_entries,
                    sort_mode,
                    locale,
                } = request;
                let cancelled = || {
                    self.suspended.load(Ordering::Acquire)
                        || self.media_epoch.load(Ordering::Acquire) != epoch
                };
                let result = scan_media(&scope, &cancelled).map(|fresh| {
                    fresh.and_then(|fresh| {
                        prepare_media_refresh_cancellable(
                            previous_entries,
                            fresh,
                            sort_mode,
                            locale.as_deref(),
                            &cancelled,
                        )
                    })
                });
                if !cancelled()
                    && let Some(result) = match result {
                        Ok(Some(output)) => Some(Ok(output)),
                        Ok(None) => None,
                        Err(error) => Some(Err(format!("{error:#}"))),
                    }
                {
                    publish_result(
                        RefreshResult::Media {
                            generation,
                            scope,
                            sort_mode,
                            result,
                        },
                        &self.result_wake_tx,
                        &self.results,
                    );
                    (self.repaint)();
                }
            }
            if let Some(request) = pending.browser {
                let BrowserRequest {
                    generation,
                    epoch,
                    scope,
                } = request;
                let cancelled = || {
                    self.suspended.load(Ordering::Acquire)
                        || self.browser_epoch.load(Ordering::Acquire) != epoch
                };
                let result = read_browser_entries_with_sort_cancellable(
                    &scope.directory,
                    scope.include_hidden,
                    &scope.extensions,
                    scope.sort_mode,
                    &cancelled,
                );
                if !cancelled()
                    && let Some(result) = match result {
                        Ok(Some(entries)) => Some(Ok(entries)),
                        Ok(None) => None,
                        Err(error) => Some(Err(format!("{error:#}"))),
                    }
                {
                    publish_result(
                        RefreshResult::Browser {
                            generation,
                            scope,
                            result,
                        },
                        &self.result_wake_tx,
                        &self.results,
                    );
                    (self.repaint)();
                }
            }
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

fn scan_media(
    scope: &MediaRefreshScope,
    cancelled: &impl Fn() -> bool,
) -> anyhow::Result<Option<Vec<MediaEntry>>> {
    match scope {
        MediaRefreshScope::Directory(options) => {
            scan_directory_cancellable(options.clone(), cancelled)
        }
        MediaRefreshScope::SelectedFiles { files, extensions } => {
            scan_files_cancellable(files, extensions, cancelled)
        }
    }
}

#[cfg(test)]
pub(crate) fn prepare_media_refresh(
    old: Vec<MediaEntry>,
    fresh: Vec<MediaEntry>,
    sort_mode: SortMode,
    locale: Option<&str>,
) -> MediaRefreshOutput {
    prepare_media_refresh_cancellable(old, fresh, sort_mode, locale, &|| false)
        .expect("refresh preparation with cancellation disabled cannot be cancelled")
}

fn prepare_media_refresh_cancellable(
    old: Vec<MediaEntry>,
    fresh: Vec<MediaEntry>,
    sort_mode: SortMode,
    locale: Option<&str>,
    cancelled: &impl Fn() -> bool,
) -> Option<MediaRefreshOutput> {
    if cancelled() {
        return None;
    }
    let mut old_by_path = HashMap::with_capacity(old.len());
    for entry in &old {
        if cancelled() {
            return None;
        }
        old_by_path.insert(entry.path.as_path(), entry);
    }
    let mut fresh_paths = HashSet::with_capacity(fresh.len());
    for entry in &fresh {
        if cancelled() {
            return None;
        }
        fresh_paths.insert(entry.path.clone());
    }
    let mut unchanged_paths = HashSet::new();
    let mut invalidated_paths = HashSet::new();
    for entry in &old {
        if cancelled() {
            return None;
        }
        if !fresh_paths.contains(&entry.path) {
            invalidated_paths.insert(entry.path.clone());
        }
    }
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
        if cancelled() {
            return None;
        }
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
    if output.changed()
        && !sorter::sort_entries_cancellable(&mut output.entries, sort_mode, locale, cancelled)
    {
        return None;
    }
    (!cancelled()).then_some(output)
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
        cell::Cell,
        ffi::OsString,
        fs,
        time::{Duration, Instant},
    };
    use tempfile::tempdir;

    #[test]
    fn worker_scans_media_without_blocking_the_caller() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("image.jpg"), b"image").unwrap();
        let service = RefreshService::new(Arc::new(AtomicBool::new(false)), || {});
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
    fn suspended_worker_drops_refresh_without_publishing_a_result() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("image.jpg"), b"image").unwrap();
        let suspended = Arc::new(AtomicBool::new(true));
        let service = RefreshService::new(Arc::clone(&suspended), || {});
        let scope = MediaRefreshScope::Directory(ScanOptions {
            root: temp.path().to_path_buf(),
            recursive: true,
            include_hidden: false,
            extensions: vec!["jpg".to_owned()],
        });

        service.request_media(1, scope, Vec::new(), SortMode::Discovered, None);
        thread::sleep(Duration::from_millis(50));

        assert!(service.drain().is_empty());
    }

    #[test]
    fn refresh_reconciliation_stops_cooperatively_when_cancelled() {
        let old = vec![media_entry("old.jpg", 0)];
        let fresh = vec![media_entry("new.jpg", 0)];
        let checks = Cell::new(0);
        let cancelled = || {
            let next = checks.get() + 1;
            checks.set(next);
            next >= 3
        };

        let result =
            prepare_media_refresh_cancellable(old, fresh, SortMode::Discovered, None, &cancelled);

        assert!(result.is_none());
        assert!(checks.get() >= 3);
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
