//! `observability.access_log = false` must actually stop the access log.
//!
//! The field was accepted, defaulted to `true`, documented, and read by
//! nothing — so an operator who turned it off to cut log volume kept getting
//! every line, with no error and nothing in the config to say it had not
//! applied.
//!
//! Its own test binary on purpose. The switch is process-wide (it has to be:
//! the emit is called from every handler's tail, and threading a flag through
//! all of them is the shape that lets one forget), so a test that flips it
//! races every sibling running in parallel — which is exactly what happened
//! when this lived in the module's test suite: two unrelated cases started
//! seeing an empty buffer. One test, one process, no ordering assumptions.

use aisix_obs::AccessLog;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::subscriber::with_default;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Clone, Default)]
struct VecWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}
impl VecWriter {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().expect("writer buffer")).into_owned()
    }
}
impl Write for VecWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf
            .lock()
            .expect("writer buffer")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for VecWriter {
    type Writer = VecWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture(enabled: bool) -> String {
    aisix_obs::access_log::init_enabled(enabled);
    let writer = VecWriter::default();
    let subscriber = fmt()
        .with_writer(writer.clone())
        .with_ansi(false)
        .with_target(false)
        .with_env_filter(EnvFilter::new("info"))
        .finish();
    with_default(subscriber, || {
        AccessLog {
            method: "POST",
            path: "/v1/chat/completions",
            status: 200,
            latency: Duration::from_millis(1),
            provider: None,
            model: None,
            api_key_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            request_id: "req-switch",
            provider_request_id: None,
            served_by_model: None,
            routing_attempt_count: None,
            routing_fallback_count: None,
            error_kind: None,
            error: None,
        }
        .emit()
    });
    writer.contents()
}

#[test]
fn the_access_log_switch_actually_switches() {
    let off = capture(false);
    // The control case runs second and leaves the switch back on, so the
    // assertions below cannot strand it off for anything that follows.
    let on = capture(true);

    assert!(
        off.is_empty(),
        "access_log = false still wrote a line:\n{off}"
    );
    assert!(
        on.contains("/v1/chat/completions"),
        "the control case must still log, or the assertion above proves \
         nothing:\n{on}"
    );
}
