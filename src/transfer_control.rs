//! Cooperative control for a single file transfer.
//!
//! The copy loops in `ftp_client` and `sftp_client` own the data path, but the
//! decisions that belong to the user — stop, pause, slow down — arrive from
//! another thread. Every field here is atomic, so the UI can flip `cancel` or
//! `pause` while a transfer is mid-file without taking a lock that the copy
//! loop holds.
//!
//! Before this existed, cancellation was a single `AtomicBool` shared by every
//! transfer, so stopping one file stopped them all and pausing was impossible.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Chunk size used by every transfer loop (256 KiB high-throughput buffer).
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Returned when a transfer stopped because the user asked it to. Callers match
/// on this to tell a cancellation apart from a genuine I/O failure, which must
/// not be retried or reported as an error.
pub const CANCELLED: &str = "Transfer cancelled";

/// A single throttle sleep is capped: with a very low speed limit the exact
/// deficit can be seconds, and parking the worker that long would make pause and
/// cancel feel unresponsive. Sleeping in bounded steps converges just as well.
const MAX_THROTTLE_SLEEP: Duration = Duration::from_millis(100);

/// How often a paused transfer re-checks whether it may continue.
const PAUSE_POLL: Duration = Duration::from_millis(40);

type ProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Control state for one transfer. Shared by reference with the copy loop; the
/// UI holds a [`ControlHandle`] to steer it.
pub struct TransferControl {
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    /// Bytes per second cap. 0 means unlimited.
    limit_bps: Arc<AtomicU64>,
    /// Shared with the handle so the UI can read live progress.
    transferred: Arc<AtomicU64>,
    total: u64,
    started: Instant,
    progress: Option<ProgressFn>,
}

impl TransferControl {
    /// A control with no limit and no progress reporting.
    pub fn new(total: u64) -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
            limit_bps: Arc::new(AtomicU64::new(0)),
            transferred: Arc::new(AtomicU64::new(0)),
            total,
            started: Instant::now(),
            progress: None,
        }
    }

    /// A control for transfers nobody can steer (archive staging, tests).
    pub fn unbounded() -> Self {
        Self::new(0)
    }

    /// Reports cumulative progress after every chunk. `total` may be 0 when the
    /// size is unknown, in which case the callback still receives the byte count.
    pub fn with_progress(total: u64, progress: ProgressFn) -> Self {
        let mut control = Self::new(total);
        control.progress = Some(progress);
        control
    }

    /// The steerable half, for the UI or the queue scheduler.
    pub fn handle(&self) -> ControlHandle {
        ControlHandle {
            cancel: self.cancel.clone(),
            pause: self.pause.clone(),
            limit_bps: self.limit_bps.clone(),
            transferred: self.transferred.clone(),
        }
    }

    pub fn set_limit_bps(&self, bps: u64) {
        self.limit_bps.store(bps, Ordering::Relaxed);
    }

    /// Called by the copy loop after writing each chunk.
    ///
    /// Returns [`CANCELLED`] once the user stops the transfer, so the loop can
    /// unwind through its normal error path and still close the data channel.
    /// While paused this blocks, which is what makes a pause real: returning
    /// early instead would leave the worker spinning and still reading the
    /// socket, so a "paused" transfer would quietly keep consuming the file.
    pub fn on_chunk(&self, n: usize) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }

        while self.pause.load(Ordering::Relaxed) {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.to_string());
            }
            std::thread::sleep(PAUSE_POLL);
        }

        // Re-checked after the loop: `cancel` clears the pause flag to wake a
        // parked transfer, so falling out of the loop is *not* proof that the
        // transfer may continue — without this line the cancelled chunk would
        // go through and the file would keep copying.
        if self.cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }

        let done = self.transferred.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        self.throttle(done)?;

        if let Some(ref cb) = self.progress {
            cb(done, self.total);
        }
        Ok(())
    }

    /// Sleeps just long enough that the average rate stays under the cap.
    ///
    /// The comparison is cumulative — bytes sent against time elapsed since the
    /// transfer started — rather than per chunk. A per-chunk calculation drifts,
    /// because the time a chunk actually takes depends on the link.
    fn throttle(&self, done: u64) -> Result<(), String> {
        let bps = self.limit_bps.load(Ordering::Relaxed);
        if bps == 0 {
            return Ok(());
        }

        let allowed = Duration::from_secs_f64(done as f64 / bps as f64);
        let elapsed = self.started.elapsed();
        if allowed <= elapsed {
            return Ok(());
        }

        let mut remaining = (allowed - elapsed).min(MAX_THROTTLE_SLEEP);
        while remaining > Duration::ZERO {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.to_string());
            }
            std::thread::sleep(remaining.min(Duration::from_millis(20)));
            remaining = remaining.saturating_sub(Duration::from_millis(20));
        }
        Ok(())
    }
}

/// Steerable view of a running transfer, cheap to clone and safe to hold from
/// another thread.
#[derive(Clone)]
pub struct ControlHandle {
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    limit_bps: Arc<AtomicU64>,
    transferred: Arc<AtomicU64>,
}

impl ControlHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        // Un-pause as well: a cancelled transfer that is still parked in the
        // pause loop would never notice the cancel flag.
        self.pause.store(false, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.pause.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.pause.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn set_limit_bps(&self, bps: u64) {
        self.limit_bps.store(bps, Ordering::Relaxed);
    }

    pub fn bytes_transferred(&self) -> u64 {
        self.transferred.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn an_unbounded_control_accepts_every_chunk() {
        let control = TransferControl::unbounded();
        for _ in 0..1000 {
            control.on_chunk(CHUNK_SIZE).expect("nothing should stop it");
        }
        assert_eq!(control.handle().bytes_transferred(), 1000 * CHUNK_SIZE as u64);
    }

    #[test]
    fn cancel_stops_the_next_chunk() {
        let control = TransferControl::new(1024);
        let handle = control.handle();
        control.on_chunk(512).expect("the first chunk is fine");

        handle.cancel();

        let err = control.on_chunk(512).expect_err("a cancelled transfer must stop");
        assert_eq!(err, CANCELLED);
    }

    /// A cancel must also release a transfer that is already parked in the pause
    /// loop, otherwise "cancel" appears to do nothing on a paused row.
    #[test]
    fn cancel_wakes_a_paused_transfer() {
        let control = TransferControl::new(1024);
        let handle = control.handle();
        handle.pause();

        let waiter = std::thread::spawn(move || control.on_chunk(1));

        std::thread::sleep(Duration::from_millis(60));
        handle.cancel();

        let err = waiter
            .join()
            .expect("the paused thread must not panic")
            .expect_err("it must unwind with a cancellation");
        assert_eq!(err, CANCELLED);
    }

    #[test]
    fn pause_blocks_until_resumed() {
        let control = Arc::new(TransferControl::new(1024));
        let handle = control.handle();
        handle.pause();

        let worker_control = control.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        let worker_counter = counter.clone();

        let worker = std::thread::spawn(move || {
            worker_counter.fetch_add(1, Ordering::SeqCst);
            worker_control.on_chunk(1)
        });

        // While paused the worker must not pass the gate.
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(counter.load(Ordering::SeqCst), 1, "the worker should be parked");
        assert!(
            !worker.is_finished(),
            "a paused transfer must still be blocked"
        );

        handle.resume();
        worker
            .join()
            .expect("the worker must not panic")
            .expect("resuming lets it through");
    }

    /// The limiter must actually slow the transfer down, not just be recorded.
    /// 512 KB at 2 MB/s cannot finish sooner than ~250 ms.
    #[test]
    fn the_speed_limit_actually_throttles() {
        let control = TransferControl::new(4 * CHUNK_SIZE as u64);
        control.set_limit_bps(2 * 1024 * 1024);

        let started = Instant::now();
        for _ in 0..4 {
            control.on_chunk(CHUNK_SIZE).expect("not cancelled");
        }
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_millis(200),
            "512 KB at 2 MB/s should take at least 200 ms, took {:?}",
            elapsed
        );
    }

    /// A limit of zero means "no limit", which must not accidentally sleep.
    #[test]
    fn a_zero_limit_is_unlimited() {
        let control = TransferControl::new(0);
        control.set_limit_bps(0);

        let started = Instant::now();
        for _ in 0..20 {
            control.on_chunk(CHUNK_SIZE).expect("not cancelled");
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "an unlimited transfer must not be delayed"
        );
    }

    #[test]
    fn progress_reports_cumulative_bytes() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let control = TransferControl::with_progress(
            3 * CHUNK_SIZE as u64,
            Arc::new(move |done, total| {
                sink.lock().unwrap().push((done, total));
            }),
        );

        control.on_chunk(CHUNK_SIZE).unwrap();
        control.on_chunk(CHUNK_SIZE).unwrap();
        control.on_chunk(CHUNK_SIZE).unwrap();

        let calls = seen.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                (CHUNK_SIZE as u64, 3 * CHUNK_SIZE as u64),
                (2 * CHUNK_SIZE as u64, 3 * CHUNK_SIZE as u64),
                (3 * CHUNK_SIZE as u64, 3 * CHUNK_SIZE as u64),
            ]
        );
    }

    /// Pausing must not lose the bytes already counted, or the UI progress bar
    /// would jump backwards on resume.
    #[test]
    fn pausing_preserves_the_byte_count() {
        let control = TransferControl::new(1024);
        let handle = control.handle();
        control.on_chunk(300).unwrap();

        handle.pause();
        handle.resume();
        control.on_chunk(200).unwrap();

        assert_eq!(control.handle().bytes_transferred(), 500);
    }
}
