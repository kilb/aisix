//! A Redis that accepts the connection and then never answers must not hang
//! the caller.
//!
//! redis-rs leaves BOTH timeouts unset by default in every mode — 0.27's
//! `ConnectionManagerConfig::DEFAULT_RESPONSE_TIMEOUT` and
//! `DEFAULT_CONNECTION_TIMEOUT` are `None`, and the cluster builder documents
//! its own as "if enabled". A blackholed server — mid-failover, a
//! security-group change, any silently dropped route — therefore produces an
//! await that never returns.
//!
//! That is worse than it sounds here, because all three consumers of this
//! connection are written to DEGRADE when Redis is unavailable: the
//! rate-limit store falls open to per-replica counting, the response cache
//! and the semantic cache fall back to a miss. None of those paths is
//! reachable while the await has not returned, so the unbounded wait does not
//! delay the fallback — it replaces "fail open" with "hang".
//!
//! No Redis needed: a bare TCP listener that accepts and then says nothing is
//! exactly the shape being guarded.

use aisix_core::{RedisConnConfig, RedisMode};
use std::time::{Duration, Instant};

/// Accept connections and never write a byte. The listener is held for the
/// life of the returned guard.
fn blackhole() -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().expect("read back the port").port();
    let handle = std::thread::spawn(move || {
        // Hold every accepted socket open, unanswered, until the process ends.
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept() {
            held.push(sock);
        }
    });
    (port, handle)
}

#[tokio::test]
async fn connecting_to_a_silent_server_gives_up_instead_of_hanging() {
    let (port, _listener) = blackhole();
    let cfg = RedisConnConfig {
        mode: RedisMode::Single,
        url: Some(format!("redis://127.0.0.1:{port}")),
        ..RedisConnConfig::default()
    };

    let started = Instant::now();
    // Generous ceiling: the point is bounded vs. unbounded, not the exact
    // number. Before the connect/response timeouts were set this future never
    // resolved, so the test hung until the harness killed the run.
    let outcome = tokio::time::timeout(Duration::from_secs(30), aisix_redis::connect(&cfg)).await;
    let elapsed = started.elapsed();

    let result = outcome.unwrap_or_else(|_| {
        panic!(
            "connecting to a silent server never returned — an unbounded wait \
             is a hang, not a fallback, and every consumer's degrade path sits \
             behind this await"
        )
    });
    assert!(
        result.is_err(),
        "a server that never answers must not report a healthy connection"
    );
    // Measured at ~6.4s with the current bounds, and ~16s with the retry
    // ladder left at its default of six. The ceiling sits between the two on
    // purpose: loose enough not to flake on a busy runner, tight enough that
    // dropping any one of the three bounds shows up here rather than as a
    // slow outage in production.
    assert!(
        elapsed < Duration::from_secs(12),
        "gave up only after {elapsed:?} — the reconnect ladder grew back, and \
         the caller waits through all of it before the fallback runs"
    );
}
