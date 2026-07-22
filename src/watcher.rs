use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use flume::{Receiver, Sender};
use notify::{
    Config as NotifyConfig, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode,
    Watcher,
};

const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchPath {
    pub path: PathBuf,
    pub recursive: bool,
}

impl WatchPath {
    pub fn new(path: PathBuf, recursive: bool) -> Self {
        Self { path, recursive }
    }

    fn recursive_mode(&self) -> RecursiveMode {
        if self.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        }
    }
}

#[derive(Debug)]
pub enum WatchMessage {
    Changed(Vec<PathBuf>),
    Error(String),
}

pub struct MediaWatcher {
    backend: WatchBackend,
    receiver: Receiver<()>,
    pending: Arc<Mutex<PendingMessages>>,
    raw_pending: Arc<Mutex<PendingMessages>>,
    debounce_sender: Sender<()>,
    watched: HashSet<WatchPath>,
}

#[derive(Default)]
struct PendingMessages {
    paths: HashSet<PathBuf>,
    errors: Vec<String>,
}

enum WatchBackend {
    Native(RecommendedWatcher),
    Polling(PollWatcher),
}

impl WatchBackend {
    fn watcher(&mut self) -> &mut dyn Watcher {
        match self {
            Self::Native(watcher) => watcher,
            Self::Polling(watcher) => watcher,
        }
    }

    fn is_polling(&self) -> bool {
        matches!(self, Self::Polling(_))
    }
}

impl MediaWatcher {
    pub fn new(repaint: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let (sender, receiver) = flume::bounded(1);
        let pending = Arc::new(Mutex::new(PendingMessages::default()));
        let raw_pending = Arc::new(Mutex::new(PendingMessages::default()));
        let (debounce_sender, debounce_receiver) = flume::bounded(1);
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        spawn_debouncer(
            debounce_receiver,
            raw_pending.clone(),
            sender.clone(),
            pending.clone(),
            repaint,
        );
        let native_handler = event_handler(debounce_sender.clone(), raw_pending.clone());
        let backend = match RecommendedWatcher::new(native_handler, NotifyConfig::default()) {
            Ok(watcher) => WatchBackend::Native(watcher),
            Err(native_error) => {
                tracing::warn!(%native_error, "native filesystem watcher unavailable; using polling");
                let notify_config =
                    NotifyConfig::default().with_poll_interval(Duration::from_secs(2));
                let polling_handler = event_handler(debounce_sender.clone(), raw_pending.clone());
                let watcher = PollWatcher::new(polling_handler, notify_config)
                    .context("failed to initialize native or polling filesystem watcher")?;
                WatchBackend::Polling(watcher)
            }
        };

        Ok(Self {
            backend,
            receiver,
            pending,
            raw_pending,
            debounce_sender,
            watched: HashSet::new(),
        })
    }

    pub fn replace_paths(&mut self, paths: impl IntoIterator<Item = WatchPath>) -> Result<()> {
        let next = normalized_paths(paths);
        if next == self.watched {
            return Ok(());
        }
        if let Err(native_error) = replace_backend_paths(&mut self.backend, &self.watched, &next) {
            if self.backend.is_polling() {
                return Err(native_error);
            }
            tracing::warn!(%native_error, "native path watch failed; switching to polling");
            let mut polling = self.polling_backend()?;
            replace_backend_paths(&mut polling, &HashSet::new(), &next)
                .context("polling fallback could not watch active media paths")?;
            self.backend = polling;
        }

        self.watched = next;
        Ok(())
    }

    pub fn drain(&self) -> Vec<WatchMessage> {
        if self.receiver.try_recv().is_err() {
            return Vec::new();
        }
        self.take_pending()
    }

    fn take_pending(&self) -> Vec<WatchMessage> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut messages = Vec::new();
        if !pending.paths.is_empty() {
            messages.push(WatchMessage::Changed(pending.paths.drain().collect()));
        }
        messages.extend(pending.errors.drain(..).map(WatchMessage::Error));
        messages
    }

    pub fn is_polling(&self) -> bool {
        self.backend.is_polling()
    }

    fn polling_backend(&self) -> Result<WatchBackend> {
        let notify_config = NotifyConfig::default().with_poll_interval(Duration::from_secs(2));
        let handler = event_handler(self.debounce_sender.clone(), self.raw_pending.clone());
        let watcher = PollWatcher::new(handler, notify_config)
            .context("failed to initialize polling filesystem watcher")?;
        Ok(WatchBackend::Polling(watcher))
    }
}

fn replace_backend_paths(
    backend: &mut WatchBackend,
    current: &HashSet<WatchPath>,
    next: &HashSet<WatchPath>,
) -> Result<()> {
    for watch in current.difference(next) {
        backend
            .watcher()
            .unwatch(&watch.path)
            .with_context(|| format!("failed to stop watching {}", watch.path.display()))?;
    }
    for watch in next.difference(current) {
        backend
            .watcher()
            .watch(&watch.path, watch.recursive_mode())
            .with_context(|| format!("failed to watch {}", watch.path.display()))?;
    }
    Ok(())
}

fn normalized_paths(paths: impl IntoIterator<Item = WatchPath>) -> HashSet<WatchPath> {
    let mut normalized = HashSet::new();
    for watch in paths {
        let path = watch.path;
        let recursive = watch.recursive
            || normalized
                .iter()
                .any(|existing: &WatchPath| existing.path == path && existing.recursive);
        normalized.retain(|existing| existing.path != path);
        normalized.insert(WatchPath::new(path, recursive));
    }
    normalized
}

fn send_raw_result(
    sender: &Sender<()>,
    pending: &Mutex<PendingMessages>,
    result: notify::Result<Event>,
) {
    let mut pending = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut changed = false;
    match result {
        Ok(event) if event_affects_media(&event.kind) => {
            changed = !event.paths.is_empty();
            pending.paths.extend(event.paths);
        }
        Ok(_) => {}
        Err(error) => {
            pending.errors.push(error.to_string());
            changed = true;
        }
    }
    drop(pending);
    if changed {
        let _ = sender.try_send(());
    }
}

fn event_handler(
    sender: Sender<()>,
    pending: Arc<Mutex<PendingMessages>>,
) -> impl FnMut(notify::Result<Event>) + Send + 'static {
    move |result| {
        send_raw_result(&sender, &pending, result);
    }
}

fn event_affects_media(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_))
}

fn spawn_debouncer(
    receiver: Receiver<()>,
    raw_pending: Arc<Mutex<PendingMessages>>,
    sender: Sender<()>,
    pending: Arc<Mutex<PendingMessages>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    thread::Builder::new()
        .name("cullr-watch-debounce".to_owned())
        .spawn(move || {
            while receiver.recv().is_ok() {
                while receiver.recv_timeout(WATCH_DEBOUNCE).is_ok() {}

                let mut raw = raw_pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut ready = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                ready.paths.extend(raw.paths.drain());
                ready.errors.append(&mut raw.errors);
                drop(ready);
                drop(raw);

                let _ = sender.try_send(());
                repaint();
            }
        })
        .expect("failed to start filesystem watch debouncer");
}

pub fn path_is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Instant};
    use tempfile::tempdir;

    #[test]
    fn duplicate_paths_prefer_recursive_watch() {
        let root = PathBuf::from("/tmp/media");
        let paths = normalized_paths([
            WatchPath::new(root.clone(), false),
            WatchPath::new(root.clone(), true),
        ]);

        assert_eq!(paths, HashSet::from([WatchPath::new(root, true)]));
    }

    #[test]
    fn path_membership_accepts_root_and_descendants() {
        let root = Path::new("/tmp/media");
        assert!(path_is_within(root, root));
        assert!(path_is_within(
            Path::new("/tmp/media/nested/file.jpg"),
            root
        ));
        assert!(!path_is_within(Path::new("/tmp/elsewhere/file.jpg"), root));
    }

    #[test]
    fn access_events_do_not_trigger_media_refreshes() {
        use notify::event::{AccessKind, AccessMode, CreateKind, RemoveKind};

        assert!(!event_affects_media(&EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(event_affects_media(&EventKind::Create(CreateKind::File)));
        assert!(event_affects_media(&EventKind::Remove(RemoveKind::File)));
    }

    #[test]
    fn watcher_reports_created_modified_and_removed_files() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("live.jpg");
        let mut watcher = MediaWatcher::new(|| {}).unwrap();
        watcher
            .replace_paths([WatchPath::new(temp.path().to_path_buf(), false)])
            .unwrap();

        fs::write(&file, b"first").unwrap();
        wait_for_path(&watcher, &file);
        fs::write(&file, b"changed content").unwrap();
        wait_for_path(&watcher, &file);
        fs::remove_file(&file).unwrap();
        wait_for_path(&watcher, &file);
    }

    fn wait_for_path(watcher: &MediaWatcher, expected: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let timeout = deadline.saturating_duration_since(Instant::now());
            match watcher.receiver.recv_timeout(timeout) {
                Ok(()) => {
                    if watcher.take_pending().into_iter().any(|message| {
                        matches!(message, WatchMessage::Changed(paths) if paths.iter().any(|path| path == expected))
                    }) {
                        return;
                    }
                }
                Err(error) => panic!("timed out waiting for {}: {error}", expected.display()),
            }
        }
        panic!("timed out waiting for {}", expected.display());
    }
}
