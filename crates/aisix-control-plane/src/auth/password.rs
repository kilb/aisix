//! Passwords: argon2 hashing behind a concurrency gate, email
//! normalisation, and the per-email login cooldown.
//!
//! - Passwords are hashed with argon2, never a fast digest: a password has
//!   little entropy, and a stolen table of unsalted fast hashes is cracked
//!   offline. SHA-256 is only for high-entropy random strings such as
//!   session tokens.
//! - One argon2 run takes 19 MiB with the default parameters, and login is
//!   reachable without authentication. Ungated, that is a memory-exhaustion
//!   denial of service, so every hash and every verification a request
//!   causes shares one semaphore ([`PasswordGate`]).
//! - A login for an account that does not exist still runs one
//!   verification, against a dummy hash. Otherwise "no such account"
//!   answers measurably faster than "wrong password", and the login route
//!   enumerates accounts however identical the answers are.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, PasswordHash};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::ApiError;

/// The shortest password accepted. Shorter ones are refused where they are
/// set, rather than left to each user.
pub const MIN_PASSWORD_LEN: usize = 12;

/// How many argon2 runs may be in flight at once.
pub const ARGON2_GATE_PERMITS: usize = 4;

/// How long a request queues for the gate before it is answered as busy.
/// Refusing at once would let a few connections looping on passwords turn
/// every login into a 429.
const GATE_WAIT: Duration = Duration::from_secs(2);

/// The longest email accepted (the SMTP path limit). Each failed login keeps
/// an entry per email in memory for a minute; without a limit an
/// unauthenticated caller could fill memory with request-sized "emails".
const MAX_EMAIL_BYTES: usize = 254;

/// Consecutive failures for one email before it cools down.
const LOGIN_FAILURES_BEFORE_COOLDOWN: u32 = 5;

/// How long the cooldown lasts.
///
/// A choice between two harms: too long, and anyone can lock a person out
/// by failing five times with their email; too short, and it stops nothing.
/// A minute holds guessing to five tries a minute, and someone locked out by
/// mistake waits a minute.
const LOGIN_COOLDOWN: Duration = Duration::from_secs(60);

/// The gate was still full after waiting [`GATE_WAIT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the password gate is busy")]
pub struct GateBusy;

/// argon2 could not hash a password. It never carries the password.
#[derive(Debug, thiserror::Error)]
#[error("password hashing failed: {0}")]
pub struct PasswordError(String);

/// Why [`PasswordGate::hash`] produced no hash.
#[derive(Debug, thiserror::Error)]
pub enum HashError {
    #[error(transparent)]
    Busy(#[from] GateBusy),
    #[error(transparent)]
    Failed(#[from] PasswordError),
}

impl From<GateBusy> for ApiError {
    fn from(_: GateBusy) -> ApiError {
        ApiError::busy()
    }
}

impl From<HashError> for ApiError {
    fn from(e: HashError) -> ApiError {
        match e {
            HashError::Busy(_) => ApiError::busy(),
            HashError::Failed(e) => ApiError::internal(e),
        }
    }
}

/// An argon2 PHC string for `password`, with a fresh salt. Slow: run it off
/// the async threads, behind the gate ([`PasswordGate::hash`]) where the
/// caller is a request.
pub fn hash(password: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError(e.to_string()))
}

fn matches(stored: &str, password: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Whether `password` is long enough to be set.
pub fn long_enough(password: &str) -> bool {
    password.chars().count() >= MIN_PASSWORD_LEN
}

/// The email trimmed and lowercased, or `None` when it cannot be an email
/// address: longer than [`MAX_EMAIL_BYTES`], or not one `@` between a
/// non-empty local part and a domain with an inner dot.
///
/// Only a shape check: whether the address is reachable is not checked
/// here.
pub fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_ascii_lowercase();
    if email.len() > MAX_EMAIL_BYTES {
        return None;
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let plausible = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.');
    plausible.then_some(email)
}

/// The concurrency gate in front of argon2, shared by every hash and
/// verification a request causes. Cheap to clone.
#[derive(Clone)]
pub struct PasswordGate {
    permits: Arc<Semaphore>,
    /// Verified against when the account does not exist, so that case costs
    /// what a real verification costs.
    dummy_hash: Arc<str>,
    /// Verifications run, so a test can show that an absent account is
    /// verified too without measuring time (which would be flaky).
    verifications: Arc<AtomicU64>,
}

impl PasswordGate {
    /// A gate with `permits` slots. Hashes the dummy once, which is why it
    /// can fail.
    pub fn new(permits: usize) -> Result<PasswordGate, PasswordError> {
        // Any valid PHC string with the default parameters does: verifying
        // it costs the same as verifying a real hash.
        let dummy = hash("aisix-control-plane-absent-account-placeholder")?;
        Ok(PasswordGate {
            permits: Arc::new(Semaphore::new(permits)),
            dummy_hash: dummy.into(),
            verifications: Arc::new(AtomicU64::new(0)),
        })
    }

    /// A slot, or [`GateBusy`] after [`GATE_WAIT`]. The permit is owned, so
    /// it moves into the blocking task and stays taken while argon2 runs,
    /// even if the request that asked is dropped meanwhile.
    async fn permit(&self) -> Result<OwnedSemaphorePermit, GateBusy> {
        tokio::time::timeout(GATE_WAIT, self.permits.clone().acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(GateBusy)
    }

    /// Whether `password` matches `stored`. With no stored hash (no such
    /// account) the dummy is verified instead and the answer is `false`, so
    /// both cases take as long.
    pub async fn verify(&self, stored: Option<&str>, password: &str) -> Result<bool, GateBusy> {
        let permit = self.permit().await?;
        let known = stored.is_some();
        let stored = stored.map_or_else(|| self.dummy_hash.to_string(), str::to_string);
        let password = password.to_string();
        self.verifications.fetch_add(1, Ordering::SeqCst);
        let matched = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            matches(&stored, &password)
        })
        .await
        // A verification that did not finish matched nothing.
        .unwrap_or(false);
        Ok(known && matched)
    }

    /// [`hash`], behind the gate and off the async threads.
    pub async fn hash(&self, password: &str) -> Result<String, HashError> {
        let permit = self.permit().await?;
        let password = password.to_string();
        let hashed = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            hash(&password)
        })
        .await
        .map_err(|e| PasswordError(e.to_string()))??;
        Ok(hashed)
    }

    /// How many verifications this gate has run.
    pub fn verification_count(&self) -> u64 {
        self.verifications.load(Ordering::SeqCst)
    }
}

/// Consecutive login failures per normalised email, kept in this process.
///
/// Keyed by email, not by client address: behind a reverse proxy the
/// address comes from `X-Forwarded-For`, which a caller can forge, while
/// someone guessing one account's password must keep using its email. With
/// several instances each counts on its own. Cheap to clone.
#[derive(Clone, Default)]
pub struct LoginThrottle {
    /// Failures and the instant the entry lapses.
    failures: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
}

impl LoginThrottle {
    /// Whether `email` is cooling down now.
    pub fn in_cooldown(&self, email: &str) -> bool {
        self.in_cooldown_at(email, Instant::now())
    }

    /// Counts one failure for `email`.
    pub fn note_failure(&self, email: &str) {
        self.note_failure_at(email, Instant::now());
    }

    /// Forgets `email`'s failures, after a success: otherwise occasional
    /// typos add up to a lockout.
    pub fn clear(&self, email: &str) {
        self.lock().remove(email);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (u32, Instant)>> {
        // The map stays consistent whatever panicked while holding it.
        self.failures.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn in_cooldown_at(&self, email: &str, now: Instant) -> bool {
        matches!(
            self.lock().get(email),
            Some((n, until)) if *n >= LOGIN_FAILURES_BEFORE_COOLDOWN && *until > now
        )
    }

    /// Every failure pushes the lapse instant out by [`LOGIN_COOLDOWN`]: it
    /// is both the cooldown's end and the entry's lifetime. So failures
    /// spread further apart than that never add up, and slow guessing is
    /// held to five tries per period anyway.
    fn note_failure_at(&self, email: &str, now: Instant) {
        let mut failures = self.lock();
        failures.retain(|_, (_, until)| *until > now);
        let entry = failures.entry(email.to_string()).or_insert((0, now));
        entry.0 += 1;
        entry.1 = now + LOGIN_COOLDOWN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_trimmed_lowercased_and_shape_checked() {
        assert_eq!(
            normalize_email("  Ops@Example.COM \n").as_deref(),
            Some("ops@example.com")
        );
        for bad in [
            "",
            "no-at-sign",
            "@example.com",
            "ops@",
            "ops@localhost",
            "ops@.example.com",
            "ops@example.com.",
            "ops@ex@ample.com",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad:?}");
        }
        let longest = format!("{}@example.com", "a".repeat(MAX_EMAIL_BYTES - 12));
        assert_eq!(longest.len(), MAX_EMAIL_BYTES);
        assert!(normalize_email(&longest).is_some());
        assert_eq!(normalize_email(&format!("a{longest}")), None);
    }

    #[test]
    fn a_password_needs_twelve_characters_not_bytes() {
        assert!(!long_enough("elevenchars"));
        assert!(long_enough("twelve chars"));
        // Twelve characters, more than twelve bytes.
        assert!(long_enough("密码密码密码密码密码密码"));
        assert!(!long_enough("密码密码密码密码密码密"));
    }

    #[tokio::test]
    async fn the_gate_verifies_real_hashes_and_never_the_dummy() {
        let gate = PasswordGate::new(2).expect("a gate");
        let stored = gate.hash("right password").await.expect("a hash");
        assert!(stored.starts_with("$argon2id$"), "{stored}");
        assert_eq!(gate.verify(Some(&stored), "right password").await, Ok(true));
        assert_eq!(
            gate.verify(Some(&stored), "wrong password").await,
            Ok(false)
        );
        assert_eq!(gate.verify(Some("not a hash"), "x").await, Ok(false));
        // The dummy's own password does not make an absent account match.
        assert_eq!(
            gate.verify(None, "aisix-control-plane-absent-account-placeholder")
                .await,
            Ok(false)
        );
        assert_eq!(gate.verification_count(), 4);
    }

    #[tokio::test]
    async fn a_full_gate_answers_busy_after_waiting() {
        let gate = PasswordGate::new(1).expect("a gate");
        let held = gate
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("a permit");
        let started = Instant::now();
        let (verified, hashed) = tokio::join!(gate.verify(None, "x"), gate.hash("y"));
        assert_eq!(verified, Err(GateBusy));
        assert!(matches!(hashed, Err(HashError::Busy(_))), "{hashed:?}");
        assert!(started.elapsed() >= GATE_WAIT, "{:?}", started.elapsed());
        // Nothing was verified while waiting.
        assert_eq!(gate.verification_count(), 0);
        drop(held);
        assert_eq!(gate.verify(None, "x").await, Ok(false));
    }

    #[test]
    fn five_failures_cool_an_email_down_until_it_lapses() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        for i in 0..4 {
            throttle.note_failure_at("a@x.io", t0 + Duration::from_secs(i));
            assert!(!throttle.in_cooldown_at("a@x.io", t0 + Duration::from_secs(i)));
        }
        let fifth = t0 + Duration::from_secs(4);
        throttle.note_failure_at("a@x.io", fifth);
        assert!(throttle.in_cooldown_at("a@x.io", fifth));
        assert!(!throttle.in_cooldown_at("b@x.io", fifth));
        assert!(throttle.in_cooldown_at("a@x.io", fifth + LOGIN_COOLDOWN - Duration::from_secs(1)));
        assert!(!throttle.in_cooldown_at("a@x.io", fifth + LOGIN_COOLDOWN));

        // Failures further apart than the cooldown never add up.
        let slow = LoginThrottle::default();
        for i in 0..10u32 {
            let at = t0 + (LOGIN_COOLDOWN + Duration::from_secs(1)) * i;
            slow.note_failure_at("a@x.io", at);
            assert!(!slow.in_cooldown_at("a@x.io", at), "failure {i}");
        }

        // A success clears the count.
        for i in 0..4 {
            throttle.note_failure_at("c@x.io", t0 + Duration::from_secs(i));
        }
        throttle.clear("c@x.io");
        throttle.note_failure_at("c@x.io", fifth);
        assert!(!throttle.in_cooldown_at("c@x.io", fifth));
    }
}
