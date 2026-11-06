//! Boot, configuration, the master key and the Postgres data layer.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::config::CpConfig;
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::{uncast_sums, Db, DbError};
use aisix_control_plane::error::BootError;
use aisix_control_plane::settings::{self, ensure_env, EnvSettings};
use serde_json::json;
use support::{TempFile, TestCp};
use uuid::Uuid;

#[tokio::test]
async fn the_first_boot_mints_an_env_id_and_later_boots_keep_it() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let first = cp.env().env_id;
    assert!(!first.is_nil());
    assert_eq!(cp.restart().await.env().env_id, first);
    // Every fresh database mints its own: the template carries none, so no two
    // tests share a kine prefix.
    let other = TestCp::start().await.expect("the same server");
    assert_ne!(other.env().env_id, first);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_first_boots_agree_on_one_env_id() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    for round in 0..20 {
        // Back to a database no boot has seen.
        sqlx::query("DELETE FROM cp_settings")
            .execute(cp.db().pool())
            .await
            .expect("reset the settings");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let boots: Vec<_> = (0..2)
            .map(|_| {
                let db = cp.db().clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    ensure_env(&db, None, "/aisix").await
                })
            })
            .collect();
        let mut ids = Vec::new();
        for boot in boots {
            ids.push(boot.await.expect("join").expect("boot").env_id);
        }
        assert_eq!(ids[0], ids[1], "round {round}");
        let later = ensure_env(cp.db(), None, "/aisix")
            .await
            .expect("a later boot");
        assert_eq!(later.env_id, ids[0], "round {round}");
    }
}

#[tokio::test]
async fn a_seed_that_disagrees_with_the_stored_env_id_refuses_to_boot() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let err = ensure_env(cp.db(), Some(Uuid::new_v4()), "/aisix")
        .await
        .unwrap_err();
    assert!(matches!(err, BootError::EnvIdMismatch { .. }), "{err}");
    assert!(err.to_string().contains("CP_ENV_ID"), "{err}");
    // The refusal changed nothing, and the agreeing seed boots.
    let agreeing = ensure_env(cp.db(), Some(cp.env().env_id), "/aisix")
        .await
        .expect("the stored env_id as seed");
    assert_eq!(agreeing.env_id, cp.env().env_id);
}

#[tokio::test]
async fn a_seed_names_the_env_id_of_the_first_boot() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    sqlx::query("DELETE FROM cp_settings")
        .execute(cp.db().pool())
        .await
        .expect("reset the settings");
    let seed = Uuid::new_v4();
    let first = ensure_env(cp.db(), Some(seed), "/aisix")
        .await
        .expect("seeded boot");
    assert_eq!(first.env_id, seed);
    let later = ensure_env(cp.db(), None, "/aisix")
        .await
        .expect("unseeded boot");
    assert_eq!(later.env_id, seed);
}

#[tokio::test]
async fn a_changed_etcd_prefix_refuses_to_boot() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let err = ensure_env(cp.db(), None, "/other").await.unwrap_err();
    assert!(matches!(err, BootError::PrefixChanged { .. }), "{err}");
    assert!(err.to_string().contains("CP_ETCD_PREFIX"), "{err}");
    let kept = ensure_env(cp.db(), None, "/aisix")
        .await
        .expect("the original prefix");
    assert_eq!(kept.etcd_prefix, "/aisix");
    assert_eq!(kept.env_id, cp.env().env_id);
}

#[test]
fn the_env_and_global_prefixes_end_in_a_slash() {
    let env_id = Uuid::new_v4();
    for prefix in ["/aisix", "/aisix/"] {
        let env = EnvSettings {
            env_id,
            etcd_prefix: prefix.to_string(),
        };
        assert_eq!(env.env_prefix(), format!("/aisix/{env_id}/"));
        assert_eq!(env.global_prefix(), "/aisix/global/");
    }
}

/// The variables every valid configuration needs, with the key file given.
fn required(key_file: &TempFile) -> Vec<(&str, &str)> {
    vec![
        ("CP_DATABASE_URL", "postgres://u:hunter2@h/db"),
        ("CP_MASTER_KEY_FILE", key_file.path_str()),
        ("CP_DP_HOSTS", "cp.example.com"),
    ]
}

fn key_file() -> TempFile {
    TempFile::with_contents("a7".repeat(32).as_bytes())
}

#[test]
fn config_errors_and_debug_never_show_secrets() {
    let err = CpConfig::from_vars(&[
        ("CP_DATABASE_URL", "postgres://u:hunter2@h/db"),
        ("CP_DP_HOSTS", ""),
    ])
    .unwrap_err();
    assert!(err.to_string().contains("CP_DP_HOSTS") && !err.to_string().contains("hunter2"));
    assert!(!format!("{err:?}").contains("hunter2"));

    let key = key_file();
    let mut vars = required(&key);
    vars.push(("CP_KINE_ENDPOINT", "http://k:kinepw@127.0.0.1:2379"));
    vars.push(("CP_PORTAL_URL", "https://p:portalpw@portal.example.com/x"));
    let cfg = CpConfig::from_vars(&vars).expect("a valid configuration");
    let dbg = format!("{cfg:?}");
    for secret in ["hunter2", "kinepw", "portalpw", "a7a7", "167, 167"] {
        assert!(!dbg.contains(secret), "{secret} in {dbg}");
    }
    // The URLs are displayed, reduced to their origin.
    assert!(dbg.contains("postgres://h/"), "{dbg}");
    assert!(dbg.contains("http://127.0.0.1:2379/"), "{dbg}");
}

#[test]
fn config_defaults_and_the_derived_gateway_url() {
    let key = key_file();
    let cfg = CpConfig::from_vars(&required(&key)).expect("a valid configuration");
    assert_eq!(cfg.database_url, "postgres://u:hunter2@h/db");
    assert_eq!(
        cfg.http_addr,
        "127.0.0.1:8090".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(cfg.dp_addr, "0.0.0.0:7943".parse::<SocketAddr>().unwrap());
    assert_eq!(cfg.dp_hosts, vec!["cp.example.com"]);
    assert_eq!(cfg.dp_base_url, "https://cp.example.com:7943");
    assert_eq!(
        cfg.dp_etcd_endpoint().as_deref(),
        Some("cp.example.com:7943")
    );
    assert_eq!(cfg.kine_endpoint, "http://127.0.0.1:2379");
    assert_eq!(cfg.etcd_prefix, "/aisix");
    assert_eq!(cfg.env_id_seed, None);
    assert_eq!(cfg.instance_stale_secs, 60);
    assert_eq!(cfg.portal_url, None);
    assert_eq!(cfg.db_lock_timeout_ms, 5_000);
    assert_eq!(cfg.db_statement_timeout_ms, 30_000);

    let seed = Uuid::new_v4().to_string();
    let mut vars = required(&key);
    vars.retain(|(k, _)| *k != "CP_DP_HOSTS");
    vars.extend([
        ("CP_DP_HOSTS", " ::1 , cp.example.com "),
        ("CP_DP_ADDR", "[::]:9443"),
        ("CP_HTTP_ADDR", "0.0.0.0:8080"),
        ("CP_ENV_ID", seed.as_str()),
        ("CP_ETCD_PREFIX", "/tenant-a"),
        ("CP_INSTANCE_STALE_SECS", "90"),
        ("CP_PORTAL_URL", "https://portal.example.com"),
        ("CP_DB_LOCK_TIMEOUT_MS", "200"),
        ("CP_DB_STATEMENT_TIMEOUT_MS", "1000"),
    ]);
    let cfg = CpConfig::from_vars(&vars).expect("a valid configuration");
    assert_eq!(cfg.dp_hosts, vec!["::1", "cp.example.com"]);
    assert_eq!(cfg.dp_base_url, "https://[::1]:9443");
    assert_eq!(cfg.dp_etcd_endpoint().as_deref(), Some("[::1]:9443"));
    assert_eq!(cfg.http_addr, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
    assert_eq!(cfg.env_id_seed.map(|u| u.to_string()), Some(seed.clone()));
    assert_eq!(cfg.etcd_prefix, "/tenant-a");
    assert_eq!(cfg.instance_stale_secs, 90);
    assert_eq!(
        cfg.portal_url.as_deref(),
        Some("https://portal.example.com")
    );
    assert_eq!(cfg.db_lock_timeout_ms, 200);
    assert_eq!(cfg.db_statement_timeout_ms, 1_000);

    // An explicit gateway URL wins; its etcd endpoint takes 443 when it names
    // no port.
    let mut vars = required(&key);
    vars.push(("CP_DP_BASE_URL", "https://dp.example.com"));
    let cfg = CpConfig::from_vars(&vars).expect("a valid configuration");
    assert_eq!(cfg.dp_base_url, "https://dp.example.com");
    assert_eq!(
        cfg.dp_etcd_endpoint().as_deref(),
        Some("dp.example.com:443")
    );

    // kine on any loopback address.
    for kine in [
        "http://localhost:2379",
        "http://[::1]:2379",
        "http://127.0.0.2:2379",
        "127.0.0.1:2379",
    ] {
        let mut vars = required(&key);
        vars.push(("CP_KINE_ENDPOINT", kine));
        let cfg = CpConfig::from_vars(&vars).unwrap_or_else(|e| panic!("{kine}: {e}"));
        assert_eq!(cfg.kine_endpoint, kine);
    }
}

#[test]
fn each_config_error_names_its_variable_and_not_its_value() {
    let key = key_file();
    let empty_key_file = TempFile::with_contents(b"");
    let cases: &[(&str, &str)] = &[
        ("CP_DATABASE_URL", ""),
        ("CP_MASTER_KEY_FILE", ""),
        ("CP_DP_HOSTS", " , "),
        ("CP_DP_HOSTS", "cp.example.com:7943"),
        ("CP_DP_HOSTS", "https://cp.example.com"),
        ("CP_HTTP_ADDR", "not-an-address"),
        ("CP_DP_ADDR", "not-an-address"),
        ("CP_DP_BASE_URL", "http://plaintext.example.com"),
        ("CP_DP_BASE_URL", "not a url"),
        ("CP_ETCD_PREFIX", "no-leading-slash"),
        ("CP_ETCD_PREFIX", "/"),
        ("CP_ENV_ID", "not-a-uuid"),
        ("CP_INSTANCE_STALE_SECS", "0"),
        ("CP_INSTANCE_STALE_SECS", "soon"),
        ("CP_DB_LOCK_TIMEOUT_MS", "0"),
        ("CP_DB_STATEMENT_TIMEOUT_MS", "-5"),
        ("CP_KINE_ENDPOINT", "http://10.0.0.5:2379"),
        ("CP_KINE_ENDPOINT", "http://kine.example.com:2379"),
        ("CP_KINE_ENDPOINT", "not a url"),
        ("CP_MASTER_KEY_FILE", empty_key_file.path_str()),
    ];
    for (var, value) in cases {
        let mut vars = required(&key);
        vars.retain(|(k, _)| k != var);
        vars.push((var, value));
        let err = match CpConfig::from_vars(&vars) {
            Ok(_) => panic!("{var}={value:?} was accepted"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains(var), "{var}={value:?}: {err}");
        if value.trim().len() > 1 {
            assert!(!err.contains(value.trim()), "{var}={value:?}: {err}");
        }
        assert!(!err.contains("hunter2"), "{err}");
    }
}

#[test]
fn a_master_key_file_must_hold_64_hex_characters() {
    let hex64 = "0123456789abcdef".repeat(4);
    let expected = MasterKey(
        hex::decode(&hex64)
            .expect("hex")
            .try_into()
            .expect("32 bytes"),
    );
    for good in [hex64.clone(), format!("{hex64}\n"), hex64.to_uppercase()] {
        let file = TempFile::with_contents(good.as_bytes());
        let key = MasterKey::from_file(file.path()).expect("a valid key file");
        // The file's bytes are the key: what one seals the other opens.
        let sealed = expected.seal(b"probe", b"x");
        assert_eq!(key.open(b"probe", &sealed).expect("same key"), b"x");
    }
    let with_secret = format!("{}hunter2x", &hex64[..56]);
    let bad = [
        String::new(),
        hex64[..63].to_string(),
        format!("{hex64}0"),
        format!("{hex64}\n\n"),
        format!("{hex64}\r\n"),
        format!(" {hex64}"),
        with_secret.clone(),
    ];
    for contents in bad {
        let file = TempFile::with_contents(contents.as_bytes());
        let err = MasterKey::from_file(file.path()).unwrap_err();
        let msg = format!("{err} {err:?}");
        assert!(
            !msg.contains("hunter2") && !msg.contains("0123456789"),
            "{msg}"
        );
    }
    assert!(MasterKey::from_file(Path::new("/nonexistent/aisix-cp/master.key")).is_err());
}

#[test]
fn a_sealed_value_opens_only_under_its_own_key_and_aad() {
    let mk = MasterKey([1; 32]);
    let s = mk.seal(b"aisix-cp:ca_key", b"x");
    assert!(MasterKey([2; 32]).open(b"aisix-cp:ca_key", &s).is_err());
    assert!(mk.open(b"aisix-cp:other", &s).is_err());
    // nonce(12) || ciphertext(1) || tag(16)
    assert_eq!(s.len(), 12 + 1 + 16);
    assert_eq!(
        mk.open(b"aisix-cp:ca_key", &s).expect("own key and aad"),
        b"x"
    );
    // A fresh nonce each time.
    assert_ne!(mk.seal(b"aisix-cp:ca_key", b"x"), s);
    let mut tampered = s.clone();
    tampered[12] ^= 1;
    assert!(mk.open(b"aisix-cp:ca_key", &tampered).is_err());
    for len in [0, 11, 12, 27] {
        assert!(
            mk.open(b"aisix-cp:ca_key", &s[..len]).is_err(),
            "{len} bytes"
        );
    }
}

#[tokio::test]
async fn healthz_answers_without_a_session() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let res = client
        .get(cp.url("/healthz"))
        .send()
        .await
        .expect("GET /healthz");
    assert_eq!(res.status(), 200);
    let res = client.get(cp.url("/nope")).send().await.expect("GET /nope");
    assert_eq!(res.status(), 404);
}

/// Inserts one `cp_settings` row in a transaction of its own.
async fn insert_setting(db: &Db, key: &'static str) -> Result<(), DbError> {
    db.tx(move |conn| {
        Box::pin(async move {
            sqlx::query("INSERT INTO cp_settings (key, value) VALUES ($1, '1')")
                .bind(key)
                .execute(&mut *conn)
                .await?;
            Ok::<_, DbError>(())
        })
    })
    .await
}

#[tokio::test]
async fn a_unique_violation_is_typed_by_its_constraint() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    insert_setting(cp.db(), "dup")
        .await
        .expect("the first insert");
    let err = insert_setting(cp.db(), "dup").await.unwrap_err();
    assert!(
        matches!(&err, DbError::UniqueViolation { constraint } if constraint == "cp_settings_pkey"),
        "{err:?}"
    );
}

#[tokio::test]
async fn upserting_a_setting_twice_keeps_the_second_value() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let mut conn = cp.db().pool().acquire().await.expect("a connection");
    assert_eq!(settings::get(&mut conn, "probe").await.expect("get"), None);
    settings::set(&mut conn, "probe", &json!({"n": 1}))
        .await
        .expect("first set");
    settings::set(&mut conn, "probe", &json!({"n": 2}))
        .await
        .expect("second set");
    assert_eq!(
        settings::get(&mut conn, "probe").await.expect("get"),
        Some(json!({"n": 2}))
    );
}

#[tokio::test]
async fn a_lock_held_past_the_lock_timeout_fails_instead_of_hanging() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let impatient = Db::connect(cp.database_url(), 200, 30_000)
        .await
        .expect("a second pool");
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE cp_settings IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .expect("hold the lock");
    let started = Instant::now();
    let waited = tokio::time::timeout(
        Duration::from_secs(10),
        impatient.tx(|conn| {
            Box::pin(async move {
                sqlx::query("SELECT count(*) FROM cp_settings")
                    .execute(&mut *conn)
                    .await?;
                Ok::<_, DbError>(())
            })
        }),
    )
    .await
    .expect("lock_timeout bounds the wait, not this test's timeout");
    assert!(matches!(waited, Err(DbError::LockTimeout)), "{waited:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    holder.rollback().await.expect("release the lock");
}

#[tokio::test]
async fn a_dropped_caller_does_not_leave_a_transaction_open() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let caller = cp.db().tx(|conn| {
        Box::pin(async move {
            sqlx::query("INSERT INTO cp_settings (key, value) VALUES ('dropped-caller', '1')")
                .execute(&mut *conn)
                .await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok::<_, DbError>(())
        })
    });
    // The caller goes away between the write and the commit, as a handler
    // does when its client disconnects.
    assert!(tokio::time::timeout(Duration::from_millis(50), caller)
        .await
        .is_err());
    let pool = cp.db().pool();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let committed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM cp_settings WHERE key = 'dropped-caller')",
        )
        .fetch_one(pool)
        .await
        .expect("read");
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND state LIKE 'idle in transaction%'",
        )
        .fetch_one(pool)
        .await
        .expect("read");
        if committed && open == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "committed={committed}, sessions idle in a transaction={open}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn migrations_wait_for_a_lock_instead_of_timing_out() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    // Another session (a replica's long migration, an operator's maintenance)
    // holds the migrations table for longer than the configured lock timeout.
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE _sqlx_migrations IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .expect("hold the migrations table");
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        holder.rollback().await
    });
    let started = Instant::now();
    let db = Db::connect(cp.database_url(), 100, 30_000)
        .await
        .expect("the migration run waits for the lock instead of failing the boot");
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "the migration run never waited: {:?}",
        started.elapsed()
    );
    release.await.expect("join").expect("release the lock");
    // The pool itself keeps the configured timeouts.
    let mut conn = db.pool().acquire().await.expect("a connection");
    for (setting, expected) in [("lock_timeout", "100ms"), ("statement_timeout", "30s")] {
        let value: String = sqlx::query_scalar(&format!("SHOW {setting}"))
            .fetch_one(&mut *conn)
            .await
            .expect("show");
        assert_eq!(value, expected, "{setting}");
    }
}

#[tokio::test]
async fn a_database_migrated_by_a_newer_build_still_opens() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (99990101000000, 'from a newer build', true, decode('00', 'hex'), 0)",
    )
    .execute(cp.db().pool())
    .await
    .expect("record a migration this build does not have");
    Db::connect(cp.database_url(), 5_000, 30_000)
        .await
        .expect("an older build opens the database");
    assert_eq!(cp.restart().await.env().env_id, cp.env().env_id);
}

#[test]
fn an_uncast_sum_is_reported() {
    assert_eq!(
        uncast_sums("SELECT COALESCE(SUM(delta), 0) FROM ledger").len(),
        1
    );
    assert!(uncast_sums("SELECT COALESCE(SUM(delta), 0)::bigint FROM ledger").is_empty());
    assert!(uncast_sums("SELECT SUM(delta)::bigint FROM ledger").is_empty());
    assert!(uncast_sums("SELECT COALESCE(SUM(delta)::bigint, 0) FROM ledger").is_empty());
    assert!(uncast_sums("SELECT coalesce( sum(delta) , 0 ) :: BIGINT FROM ledger").is_empty());
    assert_eq!(
        uncast_sums("select sum(delta) from ledger"),
        vec!["sum(delta)"]
    );
    assert_eq!(
        uncast_sums("SELECT SUM(a)::bigint, SUM(b) FROM t"),
        vec!["SUM(b)"]
    );
    assert_eq!(
        uncast_sums("SELECT SUM(CASE WHEN a THEN (b) ELSE 0 END) FROM t"),
        vec!["SUM(CASE WHEN a THEN (b) ELSE 0 END)"]
    );
    assert!(uncast_sums("SELECT SUM(CASE WHEN a THEN (b) ELSE 0 END)::bigint FROM t").is_empty());
    // A cast on some other call does not count.
    assert_eq!(
        uncast_sums("SELECT GREATEST(SUM(a), 0)::bigint FROM t").len(),
        1
    );
    // An unterminated call is reported, not skipped.
    assert_eq!(uncast_sums("SELECT SUM(a FROM t").len(), 1);
    // Not the aggregate: a longer identifier, or Rust's iterator method.
    assert!(uncast_sums("SELECT checksum(x) FROM t").is_empty());
    assert!(uncast_sums("let n: i64 = xs.iter().sum();").is_empty());
}

#[test]
fn the_crate_has_no_uncast_sum() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut pending = vec![crate_dir.join("src"), crate_dir.join("migrations")];
    let mut scanned = Vec::new();
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read a directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a source file");
            for sum in uncast_sums(&text) {
                found.push(format!("{}: {sum}", path.display()));
            }
            scanned.push(path);
        }
    }
    for expected in ["db.rs", "0001_settings.sql"] {
        assert!(
            scanned.iter().any(|p| p.ends_with(expected)),
            "{expected} not among {scanned:?}"
        );
    }
    assert!(found.is_empty(), "cast every SUM to ::bigint: {found:#?}");
}
