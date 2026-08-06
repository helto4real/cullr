use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use flume::{Receiver, Sender};
use notify::{
    Config as NotifyConfig, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode,
    Watcher,
    event::{AccessKind, AccessMode, CreateKind, ModifyKind},
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
    Rescan,
    Error(String),
}

pub struct MediaWatcher {
    backend: WatchBackend,
    receiver: Receiver<()>,
    pending: Arc<Mutex<PendingMessages>>,
    raw_pending: Arc<Mutex<RawMessages>>,
    debounce_sender: Sender<()>,
    watched: HashSet<WatchPath>,
}

#[derive(Default)]
struct PendingMessages {
    paths: HashSet<PathBuf>,
    force_rescan: bool,
    errors: Vec<String>,
}

#[derive(Default)]
struct RawMessages {
    dirty: HashMap<PathBuf, SettleRequirement>,
    ready: HashSet<PathBuf>,
    force_rescan: bool,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettleRequirement {
    StableMetadata,
    CloseWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventDisposition {
    Ignore,
    Dirty(SettleRequirement),
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

struct SettlingPath {
    requirement: SettleRequirement,
    stamp: Option<FileStamp>,
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
        let raw_pending = Arc::new(Mutex::new(RawMessages::default()));
        let (debounce_sender, debounce_receiver) = flume::bounded(1);
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        spawn_debouncer(
            debounce_receiver,
            raw_pending.clone(),
            sender.clone(),
            pending.clone(),
            repaint,
        );
        let native_handler = event_handler(
            debounce_sender.clone(),
            raw_pending.clone(),
            native_close_write_reliable(),
        );
        let backend = match RecommendedWatcher::new(native_handler, NotifyConfig::default()) {
            Ok(watcher) => WatchBackend::Native(watcher),
            Err(native_error) => {
                tracing::warn!(%native_error, "native filesystem watcher unavailable; using polling");
                let notify_config =
                    NotifyConfig::default().with_poll_interval(Duration::from_secs(2));
                let polling_handler =
                    event_handler(debounce_sender.clone(), raw_pending.clone(), false);
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
        if std::mem::take(&mut pending.force_rescan) {
            messages.push(WatchMessage::Rescan);
        }
        messages.extend(pending.errors.drain(..).map(WatchMessage::Error));
        messages
    }

    pub fn is_polling(&self) -> bool {
        self.backend.is_polling()
    }

    fn polling_backend(&self) -> Result<WatchBackend> {
        let notify_config = NotifyConfig::default().with_poll_interval(Duration::from_secs(2));
        let handler = event_handler(
            self.debounce_sender.clone(),
            self.raw_pending.clone(),
            false,
        );
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
    pending: &Mutex<RawMessages>,
    result: notify::Result<Event>,
    close_write_reliable: bool,
) {
    let mut pending = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut changed = false;
    match result {
        Ok(event) if event.need_rescan() => {
            pending.force_rescan = true;
            changed = true;
        }
        Ok(event) => match event_disposition(&event.kind, close_write_reliable) {
            EventDisposition::Ignore => {}
            EventDisposition::Dirty(requirement) => {
                changed = !event.paths.is_empty();
                for path in event.paths {
                    pending.ready.remove(&path);
                    pending
                        .dirty
                        .entry(path)
                        .and_modify(|current| {
                            if requirement == SettleRequirement::CloseWrite {
                                *current = requirement;
                            }
                        })
                        .or_insert(requirement);
                }
            }
            EventDisposition::Ready => {
                changed = !event.paths.is_empty();
                for path in event.paths {
                    pending.dirty.remove(&path);
                    pending.ready.insert(path);
                }
            }
        },
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
    pending: Arc<Mutex<RawMessages>>,
    close_write_reliable: bool,
) -> impl FnMut(notify::Result<Event>) + Send + 'static {
    move |result| {
        send_raw_result(&sender, &pending, result, close_write_reliable);
    }
}

fn event_disposition(kind: &EventKind, close_write_reliable: bool) -> EventDisposition {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => EventDisposition::Ready,
        EventKind::Access(_) => EventDisposition::Ignore,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => EventDisposition::Ready,
        EventKind::Create(CreateKind::Folder) => EventDisposition::Ready,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_)) => {
            EventDisposition::Dirty(if close_write_reliable {
                SettleRequirement::CloseWrite
            } else {
                SettleRequirement::StableMetadata
            })
        }
        EventKind::Modify(ModifyKind::Metadata(_))
        | EventKind::Modify(ModifyKind::Any | ModifyKind::Other)
        | EventKind::Any
        | EventKind::Other => EventDisposition::Dirty(SettleRequirement::StableMetadata),
    }
}

fn native_close_write_reliable() -> bool {
    cfg!(any(target_os = "linux", target_os = "android"))
}

fn spawn_debouncer(
    receiver: Receiver<()>,
    raw_pending: Arc<Mutex<RawMessages>>,
    sender: Sender<()>,
    pending: Arc<Mutex<PendingMessages>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    thread::Builder::new()
        .name("cullr-watch-debounce".to_owned())
        .spawn(move || {
            let mut settling = HashMap::<PathBuf, SettlingPath>::new();
            loop {
                let received_event = if settling.is_empty() {
                    receiver.recv().map(|()| true).map_err(|_| ())
                } else {
                    match receiver.recv_timeout(WATCH_DEBOUNCE) {
                        Ok(()) => Ok(true),
                        Err(flume::RecvTimeoutError::Timeout) => Ok(false),
                        Err(flume::RecvTimeoutError::Disconnected) => Err(()),
                    }
                };
                let Ok(received_event) = received_event else {
                    break;
                };
                if received_event {
                    while receiver.recv_timeout(WATCH_DEBOUNCE).is_ok() {}
                }

                let raw = {
                    let mut raw = raw_pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    std::mem::take(&mut *raw)
                };
                for path in &raw.ready {
                    settling.remove(path);
                }
                for (path, requirement) in raw.dirty {
                    if raw.ready.contains(&path) {
                        continue;
                    }
                    settling.insert(
                        path,
                        SettlingPath {
                            requirement,
                            stamp: None,
                        },
                    );
                }

                let mut ready_paths = raw.ready;
                settling.retain(|path, state| {
                    if state.requirement == SettleRequirement::CloseWrite {
                        return true;
                    }
                    match file_stamp(path) {
                        None => {
                            ready_paths.insert(path.clone());
                            false
                        }
                        Some(stamp) if state.stamp == Some(stamp) => {
                            ready_paths.insert(path.clone());
                            false
                        }
                        Some(stamp) => {
                            state.stamp = Some(stamp);
                            true
                        }
                    }
                });

                let mut output = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                output.paths.extend(ready_paths);
                output.force_rescan |= raw.force_rescan;
                output.errors.extend(raw.errors);
                let changed =
                    !output.paths.is_empty() || output.force_rescan || !output.errors.is_empty();
                drop(output);

                if changed {
                    let _ = sender.try_send(());
                    repaint();
                }
            }
        })
        .expect("failed to start filesystem watch debouncer");
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.is_dir() {
        return None;
    }
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

pub fn path_is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        time::Instant,
    };
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
    fn event_disposition_waits_for_close_write_when_available() {
        use notify::event::{AccessKind, AccessMode, CreateKind, RemoveKind};

        assert_eq!(
            event_disposition(&EventKind::Access(AccessKind::Open(AccessMode::Read)), true,),
            EventDisposition::Ignore
        );
        assert_eq!(
            event_disposition(&EventKind::Create(CreateKind::File), true),
            EventDisposition::Dirty(SettleRequirement::CloseWrite)
        );
        assert_eq!(
            event_disposition(&EventKind::Create(CreateKind::File), false),
            EventDisposition::Dirty(SettleRequirement::StableMetadata)
        );
        assert_eq!(
            event_disposition(
                &EventKind::Access(AccessKind::Close(AccessMode::Write)),
                true,
            ),
            EventDisposition::Ready
        );
        assert_eq!(
            event_disposition(&EventKind::Remove(RemoveKind::File), true),
            EventDisposition::Ready
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn native_watcher_does_not_publish_a_file_while_its_writer_is_open() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("slow.jpg");
        let mut watcher = MediaWatcher::new(|| {}).unwrap();
        if watcher.is_polling() {
            return;
        }
        watcher
            .replace_paths([WatchPath::new(temp.path().to_path_buf(), false)])
            .unwrap();

        let mut writer = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&file)
            .unwrap();
        writer.write_all(b"still being written").unwrap();
        writer.flush().unwrap();

        assert!(
            watcher.receiver.recv_timeout(WATCH_DEBOUNCE * 3).is_err(),
            "a live refresh was published before close-write"
        );

        drop(writer);
        wait_for_path(&watcher, &file);
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
