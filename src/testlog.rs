//! Shared `tracing` capture for tests, and the crate's single global subscriber slot.
//!
//! This module exists because there is exactly ONE global subscriber slot per process and
//! several modules need to assert on log output. `series` owned this harness first; it moved
//! here when `config` needed it too, rather than have `config`'s tests reach into
//! `series`'s test module. Tasks adding `warn!`/`error!` paths for dropped batches, a full
//! channel, or skipped records all need the same thing — the alternative was three more
//! modules inverting the same layering.
//!
//! **Do not install a second subscriber anywhere.** `set_global_default` succeeds once per
//! process and panics afterwards, so a second one turns every test that touches logging into
//! a coin flip depending on which ran first. Call [`log_sink`] instead; it is idempotent.

use std::sync::{Arc, Mutex, OnceLock};

/// Capture `tracing` output so tests can assert on log lines.
///
/// A dropped label that is never logged is invisible in production, so "does it warn" is
/// a real behaviour and needs a real assertion — without this, deleting the `warn!` kills
/// no test and the guard rots.
#[derive(Clone, Default)]
pub struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install a process-wide log sink exactly once, and hand back its buffer.
///
/// This is deliberately *global* rather than `tracing::subscriber::with_default`, and the
/// distinction is load-bearing. `tracing` caches per callsite whether anyone is listening.
/// `with_default` only redirects the calling thread, so a test running concurrently on
/// another thread sees no subscriber, hits the `warn!` in `labels_for`, and re-caches that
/// callsite as "nobody is listening" — silently emptying the buffer of whichever test was
/// asserting on it. Measured, not theorised: with `with_default` this suite failed 2 runs
/// in 30, and adding `rebuild_interest_cache` only narrowed the window rather than closing
/// it, because the race is against other threads re-caching afterwards.
///
/// A global default is installed for every thread and never removed, so once
/// `set_global_default` rebuilds the interest cache the callsite stays enabled for good.
/// `main()` installs its own subscriber but is never called under test, so there is no
/// conflict over the single global slot.
pub fn log_sink() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOG_SINK: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG_SINK.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CaptureWriter(buffer.clone()))
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("test binary installs exactly one global subscriber");
        buffer
    })
}

/// Everything logged so far, by any test. Assertions must therefore key off values unique
/// to the calling test rather than assuming the buffer holds only their own output.
pub fn captured_logs() -> String {
    let bytes = log_sink().lock().unwrap().clone();
    String::from_utf8(bytes).expect("log output should be utf-8")
}

/// Serializes tests that assert on log output, so only one `LogTail` window is open at a
/// time. See [`LogTail`] for why a buffer offset alone is not enough.
static LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Scoped capture: everything logged between construction and [`LogTail::tail`].
///
/// The buffer is shared and never cleared, so the older [`captured_logs`] forces every
/// assertion to key off a globally-unique marker string (`"per-record-"`, `"dup-aa"`) to
/// avoid reading a sibling test's output. That works, but only as long as everyone remembers
/// to invent one — and a test that forgets does not fail, it silently passes on somebody
/// else's log line. That is the wrong failure direction for a helper whose whole job is
/// catching silent behaviour.
///
/// Recording the buffer length up front removes the need for a marker, but **an offset alone
/// is not isolation** and assuming otherwise is a live bug, not a theoretical one: the first
/// test written against this helper asserted on the first line mentioning `CHANNEL_CAPACITY`
/// and matched a concurrently-running sibling's warning about a different `CHANNEL_CAPACITY`
/// failure. It failed immediately, which is the only reason it was not shipped as a flake.
///
/// So `start` also takes a process-wide lock, held until the `LogTail` drops. That makes
/// **absence** assertions sound — "nothing was logged" is meaningless if another test may be
/// writing into your window — which is what tests like `nothing_is_logged_when_variables_are
/// _simply_unset` depend on.
///
/// # Every test in a module that logs must hold the lock
///
/// Not just the ones asserting on output. The lock only excludes other lock *holders*, so a
/// test that merely triggers a warning while not holding it writes straight into somebody
/// else's window. This was also measured, not predicted: with the lock taken by log-asserting
/// tests alone, the suite still failed 1 run in 8, because siblings like
/// `unparseable_values_fall_back_to_defaults` emit the very warnings the absence assertions
/// were checking for. `config::tests` therefore opens a `LogTail` in *every* test, including
/// those that never look at the buffer. Bind it to a named `_log`, not `_` — `let _ = ...`
/// drops the guard immediately and silently restores the flake.
///
/// Remaining caveat, deliberately not closed: tests using [`captured_logs`] directly do not
/// take this lock, so their output can still land inside a `LogTail` window. That is fine
/// today because those tests are in `series` and log about labels and dimensions, sharing no
/// vocabulary with anything asserted here. A test asserting the *absence* of a string that
/// `series` might log must not rely on this helper alone.
pub struct LogTail {
    start: usize,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl LogTail {
    /// Start capturing, blocking until any other `LogTail` window has closed. Also forces the
    /// subscriber to exist, so callsites hit afterwards are enabled — the same reason
    /// existing tests call `log_sink()` before the action they are watching.
    pub fn start() -> Self {
        // Recover rather than propagate: a panicking test poisons this lock, and cascading
        // that into every other log test turns one failure into a dozen misleading ones.
        let guard = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let start = log_sink().lock().unwrap().len();
        Self {
            start,
            _guard: guard,
        }
    }

    /// Everything logged since `start`.
    pub fn tail(&self) -> String {
        let bytes = log_sink().lock().unwrap()[self.start..].to_vec();
        String::from_utf8(bytes).expect("log output should be utf-8")
    }
}
