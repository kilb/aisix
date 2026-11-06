//! `scripts/cp-test-services.sh` acts only on the cluster it started, and
//! runs only the pinned kine.
//!
//! Hermetic: `pg_config`, `initdb`, `pg_ctl` and `psql` are stand-ins put
//! first on `PATH`, so no real Postgres is started or contacted. The
//! stand-in cluster "runs" on one port (line 4 of `postmaster.pid`, as a real
//! postmaster writes it), and every stand-in logs its invocation.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use uuid::Uuid;

/// The port the stand-in cluster listens on.
const CLUSTER_PORT: &str = "56740";
/// A port it does not listen on.
const OTHER_PORT: &str = "56742";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// A state directory whose cluster runs (or not) on [`CLUSTER_PORT`],
    /// with a cached `kine` that is not the pinned binary.
    fn new(running: bool) -> Fixture {
        let root =
            std::env::temp_dir().join(format!("aisix-cp-services-{}", Uuid::new_v4().simple()));
        let fixture = Fixture { root };
        let bin = fixture.bin();
        let data = fixture.state().join("data");
        std::fs::create_dir_all(&bin).expect("create bin/");
        std::fs::create_dir_all(&data).expect("create data/");
        std::fs::write(data.join("PG_VERSION"), "18\n").expect("write PG_VERSION");
        if running {
            let pid = format!(
                "4242\n{}\n1700000000\n{CLUSTER_PORT}\n\n127.0.0.1\n  1 2\nready   \n",
                data.display()
            );
            std::fs::write(data.join("postmaster.pid"), pid).expect("write postmaster.pid");
        }
        let log = fixture.log();
        let status = if running { 0 } else { 3 };
        fixture.stand_in("pg_config", &format!("echo '{}'", bin.display()));
        fixture.stand_in(
            "initdb",
            &format!("echo \"initdb $*\" >>'{}'; exit 1", log.display()),
        );
        fixture.stand_in(
            "pg_ctl",
            &format!(
                "echo \"pg_ctl $*\" >>'{}'; case \"$*\" in *status*) exit {status};; esac; exit 1",
                log.display()
            ),
        );
        // Answers the "does kine_test exist" query with 1, so `start` goes on
        // to kine when it gets this far.
        fixture.stand_in(
            "psql",
            &format!("echo \"psql $*\" >>'{}'; echo 1", log.display()),
        );
        fixture.write_script(
            &fixture.state().join("kine"),
            &format!("echo \"kine $*\" >>'{}'", log.display()),
        );
        fixture
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn log(&self) -> PathBuf {
        self.root.join("calls.log")
    }

    fn stand_in(&self, name: &str, body: &str) {
        self.write_script(&self.bin().join(name), body);
    }

    fn write_script(&self, path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write a stand-in");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    fn run(&self, subcommand: &str, pg_port: &str) -> Output {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/cp-test-services.sh");
        let path = format!(
            "{}:{}",
            self.bin().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("bash")
            .arg(script)
            .arg(subcommand)
            .env("PATH", path)
            .env("CP_TEST_DIR", self.state())
            .env("CP_TEST_PG_PORT", pg_port)
            .env("CP_TEST_KINE_PORT", "56743")
            .env_remove("CP_TEST_KINE_BIN")
            .output()
            .expect("run the script")
    }

    /// Every logged invocation of `program`.
    fn calls(&self, program: &str) -> Vec<String> {
        let log = std::fs::read_to_string(self.log()).unwrap_or_default();
        log.lines()
            .filter(|l| l.starts_with(&format!("{program} ")))
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn start_and_env_refuse_a_port_their_cluster_is_not_listening_on() {
    let fixture = Fixture::new(true);
    for subcommand in ["start", "env"] {
        let out = fixture.run(subcommand, OTHER_PORT);
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{subcommand} succeeded: {stderr}");
        assert!(stderr.contains(OTHER_PORT), "{subcommand}: {stderr}");
        assert!(
            !text(&out.stdout).contains("export"),
            "{subcommand} printed exports"
        );
    }
    assert_eq!(fixture.calls("psql"), Vec::<String>::new());
    assert_eq!(fixture.calls("kine"), Vec::<String>::new());
}

#[test]
fn env_refuses_when_its_cluster_is_not_running() {
    let fixture = Fixture::new(false);
    let out = fixture.run("env", CLUSTER_PORT);
    assert!(!out.status.success(), "{}", text(&out.stderr));
    assert!(!text(&out.stdout).contains("export"));
}

#[test]
fn env_prints_the_exports_of_its_own_cluster() {
    let fixture = Fixture::new(true);
    let out = fixture.run("env", CLUSTER_PORT);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains(&format!(
            "export CP_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:{CLUSTER_PORT}/postgres"
        )),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn start_uses_its_own_port_and_runs_only_the_pinned_kine() {
    let fixture = Fixture::new(true);
    let out = fixture.run("start", CLUSTER_PORT);
    // The stand-ins are what the script ran, and psql went to the cluster's
    // own port: this is what makes the refusals above mean something.
    let psql = fixture.calls("psql");
    assert!(!psql.is_empty(), "psql never ran: {}", text(&out.stderr));
    assert!(
        psql.iter()
            .all(|c| c.contains(&format!("127.0.0.1:{CLUSTER_PORT}/"))),
        "{psql:?}"
    );
    // The cached kine is not the pinned binary: refused, never executed.
    assert!(!out.status.success(), "{}", text(&out.stderr));
    assert_eq!(fixture.calls("kine"), Vec::<String>::new());
}
