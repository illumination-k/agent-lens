//! Test-only helper for asserting on `tracing` output.
//!
//! Several extraction paths in this crate degrade their result when the
//! source can't be read (a node whose bytes aren't UTF-8, a `go.mod`
//! that exists but can't be opened). The whole point of those paths is
//! that the degradation is *observable* on stderr rather than silent,
//! so the tests assert the diagnostic, not just the fallback value.

use std::io;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// A `tracing` writer that accumulates everything into a shared buffer.
#[derive(Clone, Default)]
pub(crate) struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    pub(crate) fn text(&self) -> String {
        let buf = self.0.lock().expect("log buffer poisoned");
        String::from_utf8_lossy(&buf).into_owned()
    }
}

impl io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLogs {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `body` with a `tracing` subscriber installed on this thread and
/// return everything it logged alongside the body's value.
pub(crate) fn capture_logs<T>(body: impl FnOnce() -> T) -> (T, String) {
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let value = tracing::subscriber::with_default(subscriber, body);
    let text = logs.text();
    (value, text)
}
