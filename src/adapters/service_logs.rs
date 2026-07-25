use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::application::{CapturedLogStream, ServiceLogSink};

const MAX_TAIL_LINES: usize = 1_000;
const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;
const LIVE_RING_LINES: usize = 1_000;
const LIVE_RING_BYTES: usize = 1024 * 1024;
const LIVE_SUBSCRIBER_QUEUE: usize = 256;
const MAX_LIVE_SUBSCRIBERS_PER_SERVICE: usize = 8;
const MAX_PARTIAL_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stdout" => Some(Self::Stdout),
            "stderr" => Some(Self::Stderr),
            _ => None,
        }
    }
}

impl From<CapturedLogStream> for LogStream {
    fn from(value: CapturedLogStream) -> Self {
        match value {
            CapturedLogStream::Stdout => Self::Stdout,
            CapturedLogStream::Stderr => Self::Stderr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveLogSelection {
    Both,
    Stdout,
    Stderr,
}

impl LiveLogSelection {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "both" => Some(Self::Both),
            "stdout" => Some(Self::Stdout),
            "stderr" => Some(Self::Stderr),
            _ => None,
        }
    }

    const fn includes(self, stream: LogStream) -> bool {
        matches!(
            (self, stream),
            (Self::Both, _) | (Self::Stdout, LogStream::Stdout) | (Self::Stderr, LogStream::Stderr)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceLogTail {
    pub service_id: String,
    pub stream: LogStream,
    pub lines: Vec<String>,
    pub truncated_to: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveLogEventKind {
    Line,
    Gap,
    Heartbeat,
}

/// One versioned, newline-delimited event emitted by the live-log protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveLogEvent {
    pub schema_version: u32,
    pub kind: LiveLogEventKind,
    pub hub_instance_id: String,
    pub service_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<LogStream>,
    pub observed_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
}

impl LiveLogEvent {
    #[must_use]
    pub fn heartbeat(hub_instance_id: &str, service_id: &str) -> Self {
        Self {
            schema_version: 1,
            kind: LiveLogEventKind::Heartbeat,
            hub_instance_id: hub_instance_id.to_owned(),
            service_id: service_id.to_owned(),
            sequence: None,
            stream: None,
            observed_at_unix_ms: unix_time_ms(),
            text: None,
            dropped: None,
        }
    }
}

/// Platform-neutral in-memory fan-out for durable service output.
///
/// The Windows process adapter and future Unix adapters publish through the
/// same `ServiceLogSink` port. Slow consumers can only fill their own bounded
/// queues; they never block a process pipe or the rotating log writer.
#[derive(Debug)]
pub struct LiveLogHub {
    instance_id: String,
    state: Mutex<LiveLogState>,
}

impl LiveLogHub {
    #[must_use]
    pub fn new() -> Self {
        Self {
            instance_id: format!("{}-{}", std::process::id(), unix_time_ms()),
            state: Mutex::new(LiveLogState::default()),
        }
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    fn reconcile(&self, services: &BTreeMap<String, ServiceLogPaths>) {
        let existing = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .services
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let preloaded = services
            .iter()
            .filter(|(service_id, _)| !existing.contains(service_id))
            .map(|(service_id, paths)| {
                let lines = [
                    (LogStream::Stdout, &paths.stdout),
                    (LogStream::Stderr, &paths.stderr),
                ]
                .into_iter()
                .flat_map(|(stream, path)| {
                    read_tail_lines(path, MAX_TAIL_LINES / 2)
                        .unwrap_or_default()
                        .into_iter()
                        .map(move |line| (stream, line))
                })
                .collect::<Vec<_>>();
                (service_id.clone(), lines)
            })
            .collect::<Vec<_>>();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .services
            .retain(|service_id, _| services.contains_key(service_id));
        for (service_id, lines) in preloaded {
            if state.services.contains_key(&service_id) {
                continue;
            }
            let mut service = LiveServiceState::default();
            for (stream, line) in lines {
                let event = Self::line_event(
                    &self.instance_id,
                    &service_id,
                    stream,
                    state.next_sequence(),
                    line,
                );
                service.push_ring(event);
            }
            state.services.insert(service_id, service);
        }
    }

    fn publish(&self, service_id: &str, stream: LogStream, bytes: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(mut service) = state.services.remove(service_id) else {
            return;
        };
        service.partial_mut(stream).extend_from_slice(bytes);
        let lines = service.take_complete_lines(stream, false);
        for line in lines {
            let event = Self::line_event(
                &self.instance_id,
                service_id,
                stream,
                state.next_sequence(),
                line,
            );
            service.publish_event(&event);
        }
        state.services.insert(service_id.to_owned(), service);
    }

    fn close_stream(&self, service_id: &str, stream: LogStream) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(mut service) = state.services.remove(service_id) else {
            return;
        };
        let lines = service.take_complete_lines(stream, true);
        for line in lines {
            let event = Self::line_event(
                &self.instance_id,
                service_id,
                stream,
                state.next_sequence(),
                line,
            );
            service.publish_event(&event);
        }
        state.services.insert(service_id.to_owned(), service);
    }

    fn line_event(
        instance_id: &str,
        service_id: &str,
        stream: LogStream,
        sequence: u64,
        text: String,
    ) -> LiveLogEvent {
        LiveLogEvent {
            schema_version: 1,
            kind: LiveLogEventKind::Line,
            hub_instance_id: instance_id.to_owned(),
            service_id: service_id.to_owned(),
            sequence: Some(sequence),
            stream: Some(stream),
            observed_at_unix_ms: unix_time_ms(),
            text: Some(text),
            dropped: None,
        }
    }

    fn subscribe(
        self: &Arc<Self>,
        service_id: &str,
        selection: LiveLogSelection,
        tail: usize,
        after: Option<u64>,
    ) -> Result<LiveLogSubscription, ServiceLogError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ServiceLogError::LockPoisoned)?;
        let subscriber_id = state.next_subscriber_id();
        let service = state
            .services
            .get_mut(service_id)
            .ok_or_else(|| ServiceLogError::ServiceNotFound(service_id.to_owned()))?;
        if service.subscribers.len() >= MAX_LIVE_SUBSCRIBERS_PER_SERVICE {
            return Err(ServiceLogError::TooManySubscribers(service_id.to_owned()));
        }
        let mut initial = service
            .ring
            .iter()
            .filter(|event| {
                event
                    .stream
                    .is_some_and(|stream| selection.includes(stream))
                    && after
                        .is_none_or(|sequence| event.sequence.is_some_and(|value| value > sequence))
            })
            .cloned()
            .collect::<Vec<_>>();
        if after.is_none() && initial.len() > tail {
            initial.drain(..initial.len() - tail);
        }
        let (sender, receiver) = mpsc::sync_channel(LIVE_SUBSCRIBER_QUEUE);
        service.subscribers.insert(
            subscriber_id,
            LiveSubscriber {
                selection,
                sender,
                dropped: 0,
            },
        );
        Ok(LiveLogSubscription {
            service_id: service_id.to_owned(),
            subscriber_id,
            initial,
            receiver,
            hub: Arc::clone(self),
        })
    }

    fn unsubscribe(&self, service_id: &str, subscriber_id: u64) {
        if let Ok(mut state) = self.state.lock()
            && let Some(service) = state.services.get_mut(service_id)
        {
            service.subscribers.remove(&subscriber_id);
        }
    }
}

impl Default for LiveLogHub {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct LiveLogState {
    next_sequence: u64,
    next_subscriber_id: u64,
    services: BTreeMap<String, LiveServiceState>,
}

impl LiveLogState {
    fn next_sequence(&mut self) -> u64 {
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.next_sequence
    }

    fn next_subscriber_id(&mut self) -> u64 {
        self.next_subscriber_id = self.next_subscriber_id.saturating_add(1);
        self.next_subscriber_id
    }
}

#[derive(Debug, Default)]
struct LiveServiceState {
    ring: VecDeque<LiveLogEvent>,
    ring_bytes: usize,
    stdout_partial: Vec<u8>,
    stderr_partial: Vec<u8>,
    subscribers: BTreeMap<u64, LiveSubscriber>,
}

impl LiveServiceState {
    fn partial_mut(&mut self, stream: LogStream) -> &mut Vec<u8> {
        match stream {
            LogStream::Stdout => &mut self.stdout_partial,
            LogStream::Stderr => &mut self.stderr_partial,
        }
    }

    fn take_complete_lines(&mut self, stream: LogStream, close: bool) -> Vec<String> {
        let partial = self.partial_mut(stream);
        let mut lines = Vec::new();
        loop {
            if let Some(position) = partial.iter().position(|byte| *byte == b'\n') {
                let mut bytes = partial.drain(..=position).collect::<Vec<_>>();
                bytes.pop();
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                lines.push(String::from_utf8_lossy(&bytes).into_owned());
            } else if partial.len() >= MAX_PARTIAL_LINE_BYTES {
                let bytes = partial.drain(..MAX_PARTIAL_LINE_BYTES).collect::<Vec<_>>();
                lines.push(format!(
                    "{} [line truncated]",
                    String::from_utf8_lossy(&bytes)
                ));
            } else {
                break;
            }
        }
        if close && !partial.is_empty() {
            lines.push(String::from_utf8_lossy(partial).into_owned());
            partial.clear();
        }
        lines
    }

    fn publish_event(&mut self, event: &LiveLogEvent) {
        self.push_ring(event.clone());
        let mut disconnected = Vec::new();
        for (subscriber_id, subscriber) in &mut self.subscribers {
            let Some(stream) = event.stream else {
                continue;
            };
            if !subscriber.selection.includes(stream) {
                continue;
            }
            if subscriber.dropped > 0 {
                let gap = LiveLogEvent {
                    schema_version: 1,
                    kind: LiveLogEventKind::Gap,
                    hub_instance_id: event.hub_instance_id.clone(),
                    service_id: event.service_id.clone(),
                    sequence: event.sequence,
                    stream: None,
                    observed_at_unix_ms: unix_time_ms(),
                    text: None,
                    dropped: Some(subscriber.dropped),
                };
                match subscriber.sender.try_send(gap) {
                    Ok(()) => subscriber.dropped = 0,
                    Err(TrySendError::Full(_)) => {
                        subscriber.dropped = subscriber.dropped.saturating_add(1);
                        continue;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        disconnected.push(*subscriber_id);
                        continue;
                    }
                }
            }
            match subscriber.sender.try_send(event.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    subscriber.dropped = subscriber.dropped.saturating_add(1);
                }
                Err(TrySendError::Disconnected(_)) => disconnected.push(*subscriber_id),
            }
        }
        for subscriber_id in disconnected {
            self.subscribers.remove(&subscriber_id);
        }
    }

    fn push_ring(&mut self, event: LiveLogEvent) {
        let event_bytes = event.text.as_ref().map_or(0, String::len);
        self.ring_bytes = self.ring_bytes.saturating_add(event_bytes);
        self.ring.push_back(event);
        while self.ring.len() > LIVE_RING_LINES || self.ring_bytes > LIVE_RING_BYTES {
            let Some(removed) = self.ring.pop_front() else {
                break;
            };
            self.ring_bytes = self
                .ring_bytes
                .saturating_sub(removed.text.as_ref().map_or(0, String::len));
        }
    }
}

#[derive(Debug)]
struct LiveSubscriber {
    selection: LiveLogSelection,
    sender: SyncSender<LiveLogEvent>,
    dropped: u64,
}

#[derive(Debug)]
pub struct LiveLogSubscription {
    service_id: String,
    subscriber_id: u64,
    pub initial: Vec<LiveLogEvent>,
    pub receiver: Receiver<LiveLogEvent>,
    hub: Arc<LiveLogHub>,
}

impl Drop for LiveLogSubscription {
    fn drop(&mut self) {
        self.hub.unsubscribe(&self.service_id, self.subscriber_id);
    }
}

#[derive(Debug)]
pub struct ServiceLogStore {
    runtime_services_directory: PathBuf,
    paths: RwLock<BTreeMap<String, ServiceLogPaths>>,
    live: Arc<LiveLogHub>,
}

impl ServiceLogStore {
    #[must_use]
    pub fn new(
        runtime_services_directory: &Path,
        service_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        let store = Self {
            runtime_services_directory: runtime_services_directory.to_owned(),
            paths: RwLock::new(BTreeMap::new()),
            live: Arc::new(LiveLogHub::new()),
        };
        store.reconcile_service_ids(service_ids);
        store
    }

    /// Replaces the bounded service-ID allowlist after live reconciliation.
    /// Existing files remain on disk and unchanged services keep subscribers.
    pub fn reconcile_service_ids(&self, service_ids: impl IntoIterator<Item = String>) {
        let paths = service_ids
            .into_iter()
            .map(|service_id| {
                let paths = ServiceLogPaths {
                    stdout: self
                        .runtime_services_directory
                        .join(format!("{service_id}.stdout.log")),
                    stderr: self
                        .runtime_services_directory
                        .join(format!("{service_id}.stderr.log")),
                };
                (service_id, paths)
            })
            .collect::<BTreeMap<_, _>>();
        self.live.reconcile(&paths);
        *self
            .paths
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = paths;
    }

    /// Reads at most the requested number of lines from the active log file.
    ///
    /// # Errors
    ///
    /// Returns an unknown-service, metadata, or bounded-read error.
    pub fn tail(
        &self,
        service_id: &str,
        stream: LogStream,
        lines: usize,
    ) -> Result<ServiceLogTail, ServiceLogError> {
        let allowed = self
            .paths
            .read()
            .map_err(|_| ServiceLogError::LockPoisoned)?;
        let paths = allowed
            .get(service_id)
            .ok_or_else(|| ServiceLogError::ServiceNotFound(service_id.to_owned()))?;
        let path = match stream {
            LogStream::Stdout => &paths.stdout,
            LogStream::Stderr => &paths.stderr,
        };
        let lines = lines.clamp(1, MAX_TAIL_LINES);
        Ok(ServiceLogTail {
            service_id: service_id.to_owned(),
            stream,
            lines: read_tail_lines(path, lines)?,
            truncated_to: lines,
        })
    }

    /// Opens one bounded live subscription for a registered service.
    ///
    /// # Errors
    ///
    /// Returns an unknown-service, subscriber-limit, or lock error.
    pub fn subscribe(
        &self,
        service_id: &str,
        selection: LiveLogSelection,
        tail: usize,
        after: Option<u64>,
    ) -> Result<LiveLogSubscription, ServiceLogError> {
        self.live
            .subscribe(service_id, selection, tail.clamp(0, MAX_TAIL_LINES), after)
    }

    #[must_use]
    pub fn hub_instance_id(&self) -> &str {
        self.live.instance_id()
    }
}

impl ServiceLogSink for ServiceLogStore {
    fn publish(&self, service_id: &str, stream: CapturedLogStream, bytes: &[u8]) {
        self.live.publish(service_id, stream.into(), bytes);
    }

    fn close_stream(&self, service_id: &str, stream: CapturedLogStream) {
        self.live.close_stream(service_id, stream.into());
    }
}

fn read_tail_lines(path: &Path, lines: usize) -> Result<Vec<String>, ServiceLogError> {
    let bytes = match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_READ_BYTES => {
            return Err(ServiceLogError::Oversized {
                path: path.to_owned(),
                bytes: metadata.len(),
            });
        }
        Ok(_) => fs::read(path).map_err(|source| ServiceLogError::Read {
            path: path.to_owned(),
            source,
        })?,
        Err(source) if source.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(ServiceLogError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut tail = text
        .lines()
        .rev()
        .take(lines)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    tail.reverse();
    Ok(tail)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[derive(Debug, Clone)]
struct ServiceLogPaths {
    stdout: PathBuf,
    stderr: PathBuf,
}

#[derive(Debug)]
pub enum ServiceLogError {
    ServiceNotFound(String),
    TooManySubscribers(String),
    LockPoisoned,
    Read { path: PathBuf, source: io::Error },
    Oversized { path: PathBuf, bytes: u64 },
}

impl fmt::Display for ServiceLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServiceNotFound(service_id) => write!(formatter, "unknown service: {service_id}"),
            Self::TooManySubscribers(service_id) => {
                write!(formatter, "too many live-log subscribers for {service_id}")
            }
            Self::LockPoisoned => formatter.write_str("service log registry lock is poisoned"),
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to read service log {}: {source}",
                    path.display()
                )
            }
            Self::Oversized { path, bytes } => write!(
                formatter,
                "service log {} exceeds bounded read size: {bytes} bytes",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ServiceLogError {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc::TryRecvError;

    use crate::application::{CapturedLogStream, ServiceLogSink};

    use super::{LiveLogEventKind, LiveLogSelection, LogStream, ServiceLogStore};

    fn directory(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("aku-supervisor-{name}-{}", std::process::id()))
    }

    #[test]
    fn tail_is_ordered_and_bounded() {
        let directory = directory("log-tail");
        fs::create_dir_all(&directory).expect("create log directory");
        fs::write(directory.join("api.stdout.log"), "one\ntwo\nthree\n")
            .expect("write log fixture");
        let store = ServiceLogStore::new(&directory, ["api".to_owned()]);

        let tail = store
            .tail("api", LogStream::Stdout, 2)
            .expect("read log tail");

        assert_eq!(tail.lines, ["two", "three"]);
        assert!(store.tail("unknown", LogStream::Stdout, 2).is_err());
        store.reconcile_service_ids(["new".to_owned()]);
        assert!(store.tail("api", LogStream::Stdout, 2).is_err());
        assert!(store.tail("new", LogStream::Stdout, 2).is_ok());
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn live_hub_merges_streams_and_finishes_partial_lines() {
        let directory = directory("live-log");
        fs::create_dir_all(&directory).expect("create log directory");
        let store = ServiceLogStore::new(&directory, ["api".to_owned()]);
        let subscription = store
            .subscribe("api", LiveLogSelection::Both, 50, None)
            .expect("subscribe");

        store.publish("api", CapturedLogStream::Stdout, b"out");
        store.publish("api", CapturedLogStream::Stderr, b"err\n");
        store.publish("api", CapturedLogStream::Stdout, b"put\n");
        store.close_stream("api", CapturedLogStream::Stdout);

        let first = subscription.receiver.recv().expect("stderr event");
        let second = subscription.receiver.recv().expect("stdout event");
        assert_eq!(first.stream, Some(LogStream::Stderr));
        assert_eq!(first.text.as_deref(), Some("err"));
        assert_eq!(second.stream, Some(LogStream::Stdout));
        assert_eq!(second.text.as_deref(), Some("output"));
        assert!(first.sequence < second.sequence);
        assert!(matches!(
            subscription.receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn stream_filter_and_initial_tail_are_applied() {
        let directory = directory("live-filter");
        fs::create_dir_all(&directory).expect("create log directory");
        let store = ServiceLogStore::new(&directory, ["api".to_owned()]);
        store.publish("api", CapturedLogStream::Stdout, b"one\ntwo\n");
        store.publish("api", CapturedLogStream::Stderr, b"ignored\n");
        let subscription = store
            .subscribe("api", LiveLogSelection::Stdout, 1, None)
            .expect("subscribe");
        assert_eq!(subscription.initial.len(), 1);
        assert_eq!(subscription.initial[0].text.as_deref(), Some("two"));
        assert_eq!(subscription.initial[0].kind, LiveLogEventKind::Line);
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn slow_subscriber_gets_gap_without_blocking_publisher() {
        let directory = directory("live-gap");
        fs::create_dir_all(&directory).expect("create log directory");
        let store = ServiceLogStore::new(&directory, ["api".to_owned()]);
        let subscription = store
            .subscribe("api", LiveLogSelection::Both, 0, None)
            .expect("subscribe");

        for index in 0..300 {
            store.publish(
                "api",
                CapturedLogStream::Stdout,
                format!("{index}\n").as_bytes(),
            );
        }
        for _ in 0..256 {
            subscription.receiver.recv().expect("queued event");
        }
        store.publish("api", CapturedLogStream::Stdout, b"after-gap\n");

        let gap = subscription.receiver.recv().expect("gap event");
        let line = subscription.receiver.recv().expect("post-gap event");
        assert_eq!(gap.kind, LiveLogEventKind::Gap);
        assert_eq!(gap.dropped, Some(44));
        assert_eq!(line.text.as_deref(), Some("after-gap"));
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn unchanged_registration_keeps_live_subscription() {
        let directory = directory("live-reconcile");
        fs::create_dir_all(&directory).expect("create log directory");
        let store = ServiceLogStore::new(&directory, ["api".to_owned()]);
        let subscription = store
            .subscribe("api", LiveLogSelection::Both, 0, None)
            .expect("subscribe");

        store.reconcile_service_ids(["api".to_owned(), "worker".to_owned()]);
        store.publish("api", CapturedLogStream::Stderr, b"still-live\n");

        assert_eq!(
            subscription
                .receiver
                .recv()
                .expect("event after reconciliation")
                .text
                .as_deref(),
            Some("still-live")
        );
        fs::remove_dir_all(directory).ok();
    }
}
