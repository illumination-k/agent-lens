//! Test-only helper for asserting on `tracing` output.
//!
//! Several extraction paths in this crate degrade their result when the
//! source can't be read (a node whose bytes aren't UTF-8, a `go.mod`
//! that exists but can't be opened). The whole point of those paths is
//! that the degradation is *observable* on stderr rather than silent,
//! so the tests assert the diagnostic, not just the fallback value.
//!
//! # Why one global subscriber rather than `with_default`
//!
//! The obvious shape — build a subscriber per call and install it with
//! [`tracing::subscriber::with_default`] — is flaky under `cargo test`'s
//! thread pool, because `tracing` caches each callsite's `Interest`
//! **globally** while `with_default` installs a subscriber **per thread**.
//!
//! When only one dispatcher is alive, `tracing-core` rebuilds that cache
//! through its `JustOne` path, which asks *the thread calling the rebuild*
//! for its default subscriber. A thread that has none answers
//! `Interest::never()`, and the callsite is then skipped for every thread
//! — including one sitting inside `with_default` waiting to capture. The
//! capture comes back empty and the test fails claiming the code never
//! logged, when in truth the event was filtered before it was created.
//!
//! So the subscriber here is installed once for the whole test binary and
//! never torn down: with a global default, every thread answers a rebuild
//! the same way and the callsite stays enabled. The per-test part is only
//! *where the bytes go* — [`capture_logs`] parks a buffer in a
//! thread-local for the duration of the body, so tests on different
//! threads capture independently and events from threads that asked for
//! nothing are discarded.

use std::cell::RefCell;
use std::io;
use std::sync::{Arc, Mutex, Once};

use tracing_subscriber::fmt::MakeWriter;

thread_local! {
    /// Buffer the current thread is capturing into, if any.
    static SINK: RefCell<Option<CapturedLogs>> = const { RefCell::new(None) };
}

/// A shared buffer accumulating one thread's captured log output.
#[derive(Clone, Default)]
pub(crate) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    pub(crate) fn text(&self) -> String {
        let buf = self.0.lock().expect("log buffer poisoned");
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// Writer that routes each event to whichever buffer the emitting thread
/// installed, dropping it when that thread is not capturing.
struct SinkWriter;

impl io::Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        SINK.with_borrow(|slot| {
            if let Some(logs) = slot.as_ref() {
                logs.0
                    .lock()
                    .expect("log buffer poisoned")
                    .extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct MakeSinkWriter;

impl<'a> MakeWriter<'a> for MakeSinkWriter {
    type Writer = SinkWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SinkWriter
    }
}

/// Restores the previous sink when a capture ends, including on panic —
/// otherwise one failing test would leave its buffer installed and start
/// swallowing another test's output on the same pool thread.
struct SinkGuard(Option<CapturedLogs>);

impl Drop for SinkGuard {
    fn drop(&mut self) {
        SINK.with_borrow_mut(|slot| *slot = self.0.take());
    }
}

fn install_subscriber() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(MakeSinkWriter)
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        // A global default can only be set once per process. Nothing else
        // in this test binary sets one; if that ever changes, captures come
        // back empty and the assertions in the calling test say so.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Run `body` with this thread's `tracing` output captured, and return
/// everything it logged alongside the body's value.
pub(crate) fn capture_logs<T>(body: impl FnOnce() -> T) -> (T, String) {
    install_subscriber();

    let logs = CapturedLogs::default();
    let _guard = SinkGuard(SINK.with_borrow_mut(|slot| slot.replace(logs.clone())));

    let value = body();
    let text = logs.text();
    (value, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the property the module docstring explains: a capture must
    /// survive another thread rebuilding the global interest cache.
    ///
    /// The rebuild is provoked directly rather than waited for. Against
    /// the `with_default` implementation this replaces, the loop below
    /// loses ~199 of its 200 events; the whole point of the global
    /// subscriber is that the count is zero. Left as a test because the
    /// failure it guards is otherwise a rare cross-test flake that no
    /// amount of re-running reliably surfaces.
    #[test]
    fn captures_survive_a_foreign_interest_cache_rebuild() {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rebuilder = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                // This thread deliberately installs no subscriber of its own.
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    tracing::callsite::rebuild_interest_cache();
                }
            })
        };

        let lost = (0..200)
            .filter(|_| {
                let (_, logs) = capture_logs(|| tracing::warn!("interest cache probe"));
                !logs.contains("interest cache probe")
            })
            .count();

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        rebuilder.join().expect("rebuilder thread");

        assert_eq!(lost, 0, "{lost}/200 captures lost their event");
    }

    #[test]
    fn captures_are_isolated_from_events_on_other_threads() {
        let (_, logs) = capture_logs(|| {
            std::thread::spawn(|| tracing::warn!("from an uncaptured thread"))
                .join()
                .expect("worker thread");
            tracing::warn!("from the capturing thread");
        });

        assert!(
            logs.contains("from the capturing thread"),
            "expected this thread's event, got: {logs}"
        );
        assert!(
            !logs.contains("from an uncaptured thread"),
            "another thread's event leaked in, got: {logs}"
        );
    }
}
