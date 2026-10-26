//! A real transfer queue: per-task state, bounded parallelism, automatic
//! retries and one shared speed limit.
//!
//! Before this module, every pane upload and download was a single blocking IPC
//! call whose progress bar was animated by a `setInterval` guess in the UI, and
//! the only control was one global `AtomicBool` that stopped everything at once.
//! Here each task carries its own [`ControlHandle`], so a row can be paused,
//! resumed or cancelled without touching its neighbours.
//!
//! Workers are long-lived threads that claim one task at a time. That is
//! deliberately simpler than a thread pool: a transfer is dominated by network
//! wait, so the useful parallelism is a handful of sessions, and a fixed worker
//! count keeps the per-task connection lifecycle obvious.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::remote::RemoteClient;
use crate::transfer_control::{CANCELLED, ControlHandle, TransferControl};

/// Opens a fresh connection for one task. The queue never shares a session
/// between two transfers, because FTP control channels are strictly
/// request/response and interleaving two of them deadlocks.
pub type ConnectFn = Arc<dyn Fn() -> Result<RemoteClient, String> + Send + Sync>;

/// Pushes a script into the WebView. The queue runs on background threads and
/// has no other way to reach the UI.
pub type EmitFn = Arc<dyn Fn(String) + Send + Sync>;

/// How often a progress update is pushed to the UI. Emitting per chunk would
/// flood the WebView with thousands of `evaluate_script` calls per second.
const PROGRESS_EMIT_MS: u64 = 150;

/// How long a worker with nothing to do waits before looking again.
const IDLE_POLL: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Upload,
    Download,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Upload => "upload",
            Kind::Download => "download",
        }
    }

    pub fn parse(value: &str) -> Option<Kind> {
        match value.trim().to_ascii_lowercase().as_str() {
            "upload" | "put" => Some(Kind::Upload),
            "download" | "get" => Some(Kind::Download),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Running => "running",
            State::Paused => "paused",
            State::Completed => "completed",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }

    /// True once the task will never run again without being re-queued.
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Completed | State::Failed | State::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: u64,
    pub kind: Kind,
    pub local_path: String,
    pub remote_path: String,
    /// Expected size in bytes, or 0 when the caller did not know it.
    pub size: u64,
    pub attempts: u32,
    pub state: State,
    pub error: Option<String>,
    pub bytes: u64,
}

impl Task {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "kind": self.kind.as_str(),
            "state": self.state.as_str(),
            "local_path": self.local_path,
            "remote_path": self.remote_path,
            "size": self.size,
            "bytes": self.bytes,
            "attempts": self.attempts,
            "error": self.error,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// How many transfers may run at once.
    pub max_parallel: usize,
    /// Shared bytes-per-second cap. 0 means unlimited.
    pub speed_limit_bps: u64,
    /// Total attempts per task, including the first one.
    pub max_attempts: u32,
    /// Pause before re-queueing a failed task.
    pub retry_delay_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Two matches what most shared hosts allow: a control session plus
            // one bulk session, so a second transfer never trips a login cap.
            max_parallel: 2,
            speed_limit_bps: 0,
            max_attempts: 3,
            retry_delay_ms: 900,
        }
    }
}

impl Settings {
    /// Clamps incoming values so a bad payload cannot spawn 10 000 threads.
    fn sanitised(mut self) -> Self {
        self.max_parallel = self.max_parallel.clamp(1, 8);
        self.max_attempts = self.max_attempts.clamp(1, 10);
        self.retry_delay_ms = self.retry_delay_ms.min(30_000);
        self
    }
}

#[derive(Clone)]
pub struct Queue {
    tasks: Arc<Mutex<Vec<Task>>>,
    handles: Arc<Mutex<HashMap<u64, ControlHandle>>>,
    settings: Arc<Mutex<Settings>>,
    next_id: Arc<AtomicU64>,
    worker_count: Arc<AtomicU64>,
    last_emit_ms: Arc<AtomicU64>,
    /// Set to true when the queue is being dropped so workers exit their loop
    /// instead of polling forever. Without this, every test leaks threads.
    shutdown: Arc<AtomicBool>,
    connect: ConnectFn,
    emit: Option<EmitFn>,
}

impl Queue {
    pub fn new(connect: ConnectFn, emit: Option<EmitFn>) -> Self {
        Self {
            tasks: Arc::new(Mutex::new(Vec::new())),
            handles: Arc::new(Mutex::new(HashMap::new())),
            settings: Arc::new(Mutex::new(Settings::default())),
            next_id: Arc::new(AtomicU64::new(1)),
            worker_count: Arc::new(AtomicU64::new(0)),
            last_emit_ms: Arc::new(AtomicU64::new(0)),
            shutdown: Arc::new(AtomicBool::new(false)),
            connect,
            emit,
        }
    }

    /// Signals every worker to exit its loop. New tasks can still be enqueued
    /// after a shutdown, but workers will not be respawned until the next
    /// `ensure_workers` call (e.g. on a subsequent `enqueue`).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake any sleeping workers so they notice the flag promptly.
        drop(self.tasks.lock());
    }

    /// Adds a task and makes sure workers exist to run it.
    pub fn enqueue(&self, kind: Kind, local_path: &str, remote_path: &str, size: u64) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.tasks.lock().unwrap().push(Task {
            id,
            kind,
            local_path: local_path.to_string(),
            remote_path: remote_path.to_string(),
            size,
            attempts: 0,
            state: State::Pending,
            error: None,
            bytes: 0,
        });

        self.ensure_workers();
        self.emit_now();
        id
    }

    pub fn set_settings(&self, settings: Settings) {
        let clean = settings.sanitised();
        *self.settings.lock().unwrap() = clean;

        // Apply the new cap to transfers that are already running, so changing
        // the limit takes effect immediately instead of on the next file.
        for handle in self.handles.lock().unwrap().values() {
            handle.set_limit_bps(clean.speed_limit_bps);
        }

        self.ensure_workers();
        self.emit_now();
    }

    pub fn settings(&self) -> Settings {
        *self.settings.lock().unwrap()
    }

    /// Pauses a task. A running transfer blocks between chunks; a pending one is
    /// simply skipped by the workers.
    pub fn pause(&self, id: u64) -> bool {
        let mut tasks = self.tasks.lock().unwrap();
        let Some(task) = tasks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if task.state.is_terminal() {
            return false;
        }

        if let Some(handle) = self.handles.lock().unwrap().get(&id) {
            handle.pause();
        }
        task.state = State::Paused;
        drop(tasks);
        self.emit_now();
        true
    }

    pub fn resume(&self, id: u64) -> bool {
        let mut tasks = self.tasks.lock().unwrap();
        let Some(task) = tasks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if task.state.is_terminal() {
            return false;
        }

        let handle = self.handles.lock().unwrap().get(&id).cloned();
        match handle {
            // Already running: just release the gate.
            Some(h) => {
                h.resume();
                task.state = State::Running;
            }
            // Was pending: let a worker pick it up again.
            None => {
                task.state = State::Pending;
            }
        }
        drop(tasks);
        self.ensure_workers();
        self.emit_now();
        true
    }

    pub fn cancel(&self, id: u64) -> bool {
        {
            let mut tasks = self.tasks.lock().unwrap();
            let Some(task) = tasks.iter_mut().find(|t| t.id == id) else {
                return false;
            };
            if task.state.is_terminal() {
                return false;
            }
            // A pending task has no worker looking at it, and a task claimed a
            // moment ago has no handle yet. Both are finished by state alone.
            let has_handle = self.handles.lock().unwrap().contains_key(&id);
            if task.state == State::Pending || !has_handle {
                task.state = State::Cancelled;
                drop(tasks);
                self.emit_now();
                return true;
            }
        }

        if let Some(handle) = self.handles.lock().unwrap().get(&id) {
            handle.cancel();
        }
        self.emit_now();
        true
    }

    /// Cancels everything, running or pending.
    pub fn cancel_all(&self) -> usize {
        let ids: Vec<u64> = {
            let tasks = self.tasks.lock().unwrap();
            tasks
                .iter()
                .filter(|t| !t.state.is_terminal())
                .map(|t| t.id)
                .collect()
        };
        let count = ids.len();
        for id in ids {
            self.cancel(id);
        }
        count
    }

    /// Puts a failed or cancelled task back at the front of the queue.
    pub fn requeue(&self, id: u64) -> bool {
        let mut tasks = self.tasks.lock().unwrap();
        let Some(task) = tasks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if !matches!(task.state, State::Failed | State::Cancelled) {
            return false;
        }
        task.state = State::Pending;
        task.attempts = 0;
        task.error = None;
        task.bytes = 0;
        drop(tasks);

        self.ensure_workers();
        self.emit_now();
        true
    }

    /// Drops tasks that will never run again. Returns how many went away.
    pub fn clear_finished(&self) -> usize {
        let before = {
            let mut tasks = self.tasks.lock().unwrap();
            let before = tasks.len();
            tasks.retain(|t| !t.state.is_terminal());
            before - tasks.len()
        };
        if before > 0 {
            self.emit_now();
        }
        before
    }

    pub fn snapshot(&self) -> serde_json::Value {
        // Lock order is tasks -> handles everywhere in this module. Taking them
        // the other way here would deadlock against `pause`.
        let mut tasks_guard = self.tasks.lock().unwrap();
        let handles = self.handles.lock().unwrap();

        let tasks: Vec<serde_json::Value> = tasks_guard
            .iter_mut()
            .map(|task| {
                let mut value = task.to_json();
                if let Some(handle) = handles.get(&task.id) {
                    // Live values beat the task's own counters: the copy loop
                    // updates them every chunk, while `task.bytes` only moves on
                    // a throttled emit. `paused` likewise reflects the gate the
                    // worker is actually blocked on.
                    value["bytes"] = serde_json::json!(handle.bytes_transferred());
                    value["paused"] = serde_json::json!(handle.is_paused());
                } else {
                    value["paused"] = serde_json::json!(task.state == State::Paused);
                }
                value
            })
            .collect();
        drop(handles);
        drop(tasks_guard);

        let settings = self.settings();
        serde_json::json!({
            "tasks": tasks,
            "settings": {
                "max_parallel": settings.max_parallel,
                "speed_limit_bps": settings.speed_limit_bps,
                "max_attempts": settings.max_attempts,
                "retry_delay_ms": settings.retry_delay_ms,
            },
        })
    }

    /// True when nothing is pending or running. Used to decide whether the UI's
    /// refresh-after-transfer is worth doing.
    pub fn is_idle(&self) -> bool {
        let tasks = self.tasks.lock().unwrap();
        !tasks
            .iter()
            .any(|t| matches!(t.state, State::Pending | State::Running | State::Paused))
    }

    fn ensure_workers(&self) {
        if self.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let want = self.settings.lock().unwrap().max_parallel.max(1);
        let have = self.worker_count.load(Ordering::SeqCst) as usize;
        for _ in have..want {
            self.worker_count.fetch_add(1, Ordering::SeqCst);
            let queue = self.clone();
            std::thread::spawn(move || queue.worker_loop());
        }
    }

    fn worker_loop(&self) {
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                self.worker_count.fetch_sub(1, Ordering::SeqCst);
                return;
            }
            match self.claim() {
                Some(task) => self.run_task(task),
                None => std::thread::sleep(IDLE_POLL),
            }
        }
    }

    /// Takes the oldest pending task. Paused tasks are skipped rather than
    /// blocking the queue behind them, which is what makes "pause" mean
    /// "everything else keeps going".
    fn claim(&self) -> Option<Task> {
        let mut tasks = self.tasks.lock().unwrap();
        let index = tasks.iter().position(|t| t.state == State::Pending)?;
        tasks[index].state = State::Running;
        let task = tasks[index].clone();
        drop(tasks);

        self.emit_now();
        Some(task)
    }

    fn run_task(&self, mut task: Task) {
        // Register the control *before* connecting. Opening a session can take a
        // second, and a task cancelled during that window must still be findable
        // by `cancel`; otherwise the transfer would start anyway after the user
        // had already stopped it.
        let control = {
            let queue = self.clone();
            let id = task.id;
            TransferControl::with_progress(
                task.size,
                Arc::new(move |done, _total| queue.update_bytes(id, done)),
            )
        };
        control.set_limit_bps(self.settings.lock().unwrap().speed_limit_bps);

        let handle = control.handle();
        self.handles.lock().unwrap().insert(task.id, handle.clone());

        if handle.is_cancelled() || !self.is_still_running(task.id) {
            self.handles.lock().unwrap().remove(&task.id);
            self.settle(task.id, State::Cancelled, None);
            return;
        }

        let mut client = match (self.connect)() {
            Ok(client) => client,
            Err(e) => {
                self.handles.lock().unwrap().remove(&task.id);
                self.fail_or_retry(
                    &mut task,
                    format!("Could not open a transfer session: {}", e),
                );
                return;
            }
        };

        let result = match task.kind {
            Kind::Upload => client
                .upload_from_local_file_ctl(&task.local_path, &task.remote_path, &control)
                .map(|_| ()),
            Kind::Download => client
                .download_to_local_file_ctl(&task.remote_path, &task.local_path, &control)
                .map(|_| ()),
        };

        self.handles.lock().unwrap().remove(&task.id);

        match result {
            Ok(()) => self.settle(task.id, State::Completed, None),
            // A cancellation is not a failure: it must never be retried, or the
            // file would restart by itself after the user stopped it.
            Err(e) if e == CANCELLED => self.settle(task.id, State::Cancelled, None),
            Err(e) => self.fail_or_retry(&mut task, e),
        }
    }

    /// True while the task is still meant to run. A cancel that arrived between
    /// `claim` and the handle being registered shows up here as a state change,
    /// so the worker can bail out before opening a connection.
    fn is_still_running(&self, id: u64) -> bool {
        let tasks = self.tasks.lock().unwrap();
        tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.state == State::Running)
            .unwrap_or(false)
    }

    fn fail_or_retry(&self, task: &mut Task, error: String) {
        let settings = self.settings();
        let attempts = {
            let mut tasks = self.tasks.lock().unwrap();
            let Some(entry) = tasks.iter_mut().find(|t| t.id == task.id) else {
                return;
            };
            entry.attempts += 1;
            let attempts = entry.attempts;

            if attempts >= settings.max_attempts {
                entry.state = State::Failed;
                entry.error = Some(error.clone());
            } else {
                // Back to pending with the byte counter cleared: the retry starts
                // the file again, so a stale count would show a progress bar that
                // jumps past 100%.
                entry.state = State::Pending;
                entry.bytes = 0;
                entry.error = Some(format!(
                    "Attempt {}/{} failed: {}",
                    attempts, settings.max_attempts, error
                ));
            }
            attempts
        };

        self.emit_now();

        if attempts < settings.max_attempts {
            std::thread::sleep(Duration::from_millis(settings.retry_delay_ms));
        }
    }

    fn settle(&self, id: u64, state: State, error: Option<String>) {
        {
            let mut tasks = self.tasks.lock().unwrap();
            if let Some(entry) = tasks.iter_mut().find(|t| t.id == id) {
                entry.state = state;
                entry.error = error;
                if state == State::Completed {
                    // Report the full size even when the server never sent a
                    // total, so the row does not finish showing a partial bar.
                    if entry.size > 0 {
                        entry.bytes = entry.size;
                    }
                }
            }
        }
        self.emit_now();
    }

    fn update_bytes(&self, id: u64, done: u64) {
        {
            let mut tasks = self.tasks.lock().unwrap();
            let Some(entry) = tasks.iter_mut().find(|t| t.id == id) else {
                return;
            };
            // A paused task keeps its count; a cancelled one is left alone so the
            // final snapshot shows where it actually stopped.
            if entry.state == State::Running {
                entry.bytes = done;
            }
        }
        self.emit_throttled();
    }

    /// Sends a full snapshot. The task list is small (tens of rows), so one
    /// message type keeps the UI simple and cannot drift out of sync.
    fn emit_now(&self) {
        let Some(ref emit) = self.emit else { return };
        let payload = self.snapshot();
        let script = format!(
            "if(window.onTransferEvent)window.onTransferEvent({});",
            payload
        );
        emit(script);
    }

    fn emit_throttled(&self) {
        let now = crate::watcher::now_ms();
        let last = self.last_emit_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < PROGRESS_EMIT_MS {
            return;
        }
        self.last_emit_ms.store(now, Ordering::Relaxed);
        self.emit_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps a Queue and shuts it down on drop so tests do not leak worker
    /// threads. Every test helper returns this instead of a bare Queue.
    struct TestQueue(Queue);
    impl std::ops::Deref for TestQueue {
        type Target = Queue;
        fn deref(&self) -> &Queue { &self.0 }
    }
    impl Drop for TestQueue {
        fn drop(&mut self) {
            self.0.shutdown();
            // Give workers time to notice the flag and exit before the next
            // test starts spawning its own.
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    /// A queue whose worker can never connect, so `enqueue` exercises the
    /// bookkeeping without touching the network.
    fn offline_queue() -> TestQueue {
        TestQueue(Queue::new(Arc::new(|| Err("offline in tests".to_string())), None))
    }

    fn state_of(queue: &Queue, id: u64) -> State {
        queue
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .expect("task must exist")
            .state
    }

    #[test]
    fn enqueue_assigns_increasing_ids_and_starts_pending() {
        let queue = offline_queue();
        let a = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 10);
        let b = queue.enqueue(Kind::Download, "/b.txt", "C:/b.txt", 20);

        assert!(b > a, "ids must increase");
        assert_eq!(state_of(&queue, a), State::Pending);
        assert_eq!(state_of(&queue, b), State::Pending);
    }

    /// A task that never connects must fail rather than spin forever, and must
    /// exhaust its attempts.
    #[test]
    fn an_unconnectable_task_fails_after_all_attempts() {
        let queue = offline_queue();
        queue.set_settings(Settings {
            max_attempts: 2,
            retry_delay_ms: 1,
            max_parallel: 1,
            ..Default::default()
        });

        let id = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 10);

        // Two attempts with a 1 ms delay: give it room without being flaky.
        for _ in 0..100 {
            if state_of(&queue, id) == State::Failed {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(state_of(&queue, id), State::Failed);
        let attempts = queue
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .unwrap()
            .attempts;
        assert_eq!(attempts, 2, "it must use exactly max_attempts");
    }

    #[test]
    fn pausing_a_pending_task_stops_a_worker_claiming_it() {
        let queue = offline_queue();
        queue.set_settings(Settings {
            max_parallel: 1,
            ..Default::default()
        });
        let id = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 10);

        assert!(queue.pause(id), "a pending task can be paused");
        assert_eq!(state_of(&queue, id), State::Paused);
    }

    #[test]
    fn resuming_a_paused_pending_task_returns_it_to_the_queue() {
        let queue = offline_queue();
        let id = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 10);

        queue.pause(id);
        assert!(queue.resume(id));
        assert_eq!(state_of(&queue, id), State::Pending);
    }

    #[test]
    fn cancelling_a_pending_task_is_immediate() {
        let queue = offline_queue();
        let id = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 10);

        assert!(queue.cancel(id));
        assert_eq!(state_of(&queue, id), State::Cancelled);
    }

    #[test]
    fn a_terminal_task_cannot_be_paused_or_cancelled_again() {
        let queue = offline_queue();
        let id = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 10);
        queue.cancel(id);

        assert!(!queue.pause(id), "a cancelled task must reject a pause");
        assert!(!queue.cancel(id), "and reject a second cancel");
    }

    #[test]
    fn requeue_resets_attempts_and_progress() {
        let queue = offline_queue();
        let id = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 10);
        queue.cancel(id);

        assert!(queue.requeue(id));
        let task = queue
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == id)
            .unwrap()
            .clone();
        assert_eq!(task.state, State::Pending);
        assert_eq!(task.attempts, 0);
        assert_eq!(task.bytes, 0);
        assert!(task.error.is_none());
    }

    #[test]
    fn clear_finished_removes_only_terminal_tasks() {
        let queue = offline_queue();
        let keep = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 10);
        let drop_it = queue.enqueue(Kind::Upload, "C:/b.txt", "/b.txt", 10);
        queue.cancel(drop_it);

        assert_eq!(queue.clear_finished(), 1);
        assert_eq!(state_of(&queue, keep), State::Pending);
    }

    #[test]
    fn cancel_all_stops_pending_and_running_tasks_alike() {
        let queue = offline_queue();
        let a = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 1);
        let b = queue.enqueue(Kind::Download, "/b.txt", "C:/b.txt", 1);

        assert_eq!(queue.cancel_all(), 2);
        assert_eq!(state_of(&queue, a), State::Cancelled);
        assert_eq!(state_of(&queue, b), State::Cancelled);
    }

    #[test]
    fn settings_are_clamped_to_a_sane_range() {
        let queue = offline_queue();
        queue.set_settings(Settings {
            max_parallel: 9999,
            max_attempts: 0,
            speed_limit_bps: 0,
            retry_delay_ms: 10_000_000,
        });

        let applied = queue.settings();
        assert_eq!(applied.max_parallel, 8, "must not spawn unbounded workers");
        assert_eq!(applied.max_attempts, 1, "at least one attempt");
        assert_eq!(applied.retry_delay_ms, 30_000);
    }

    #[test]
    fn the_snapshot_exposes_every_field_the_ui_needs() {
        let queue = offline_queue();
        let id = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 1234);

        let snap = queue.snapshot();
        let tasks = snap["tasks"].as_array().expect("tasks array");
        let task = tasks
            .iter()
            .find(|t| t["id"] == id)
            .expect("the queued task must appear");

        assert_eq!(task["kind"], "upload");
        assert_eq!(task["state"], "pending");
        assert_eq!(task["local_path"], "C:/a.txt");
        assert_eq!(task["remote_path"], "/a.txt");
        assert_eq!(task["size"], 1234);
        assert_eq!(task["bytes"], 0);
        assert!(task["error"].is_null());
        assert!(snap["settings"]["max_parallel"].is_number());
    }

    #[test]
    fn an_idle_queue_reports_itself_idle() {
        let queue = offline_queue();
        assert!(queue.is_idle(), "a fresh queue has nothing to do");

        let id = queue.enqueue(Kind::Upload, "C:/a.txt", "/a.txt", 1);
        assert!(!queue.is_idle(), "a pending task is work");

        queue.cancel(id);
        assert!(queue.is_idle(), "a cancelled task is no longer work");
    }

    #[test]
    fn kind_parsing_accepts_the_aliases_the_ui_may_send() {
        assert_eq!(Kind::parse("upload"), Some(Kind::Upload));
        assert_eq!(Kind::parse("put"), Some(Kind::Upload));
        assert_eq!(Kind::parse("Download"), Some(Kind::Download));
        assert_eq!(Kind::parse("get"), Some(Kind::Download));
        assert_eq!(Kind::parse("sideways"), None);
    }
}
