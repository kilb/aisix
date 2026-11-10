//! How many statements a public model's view spends on who can call it.
//!
//! The view counts every tenant whose scope reaches the model and every live
//! key whose grant does. Those are read once per answer, however many
//! tenants there are: reading each tenant's scope with statements of its own
//! costs two round trips per tenant on every view and every list.
//!
//! The statements are counted from the database driver's own events (the
//! `sqlx::query` target), through a subscriber this binary installs for the
//! whole process, so this file holds one test.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};

use aisix_control_plane::auth::OperatorRole;
use serde_json::json;
use support::TestCp;
use tracing::field::{Field, Visit};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use uuid::Uuid;

/// The statements so far that read one tenant's row by id.
static TENANT_READS: AtomicUsize = AtomicUsize::new(0);

/// Counts every statement the driver reports that reads one tenant's row by
/// its id, the shape a per-tenant scope read has.
struct TenantReads;

/// A statement event's text: its summary and, when longer, its statement.
#[derive(Default)]
struct Text(String);

impl Visit for Text {
    fn record_str(&mut self, field: &Field, value: &str) {
        if matches!(field.name(), "summary" | "db.statement") {
            self.0.push_str(value);
            self.0.push(' ');
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if matches!(field.name(), "summary" | "db.statement") {
            self.0.push_str(&format!("{value:?} "));
        }
    }
}

impl<S: Subscriber> Layer<S> for TenantReads {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if metadata.target() == "sqlx::query" {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        metadata.target() == "sqlx::query"
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut text = Text::default();
        event.record(&mut text);
        if text.0.contains("FROM tenants WHERE id = $1") {
            TENANT_READS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_public_models_view_reads_the_tenants_once_however_many_there_are() {
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(TenantReads))
        .expect("the only subscriber of this binary");
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let r = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(op.cookie()),
            Some(json!({"doc": {
                "display_name": "acct-reach-count-9d4",
                "provider": "openai",
                "api_key": "sk-reach-count-9d4",
                "groups": ["shared"],
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let account = r.body["id"].as_str().expect("an account id").to_string();
    let r = cp
        .send(
            "POST",
            "/admin/public-models",
            Some(op.cookie()),
            Some(json!({"name": "pub-reach-count-9d4"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let public = Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid");
    let r = cp
        .send(
            "PUT",
            &format!("/admin/provider-keys/{account}/channel-models/{public}"),
            Some(op.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    // Customers on every public model, each of which the view counts.
    const TENANTS: usize = 30;
    for n in 0..TENANTS {
        let r = cp
            .send(
                "POST",
                "/admin/tenants",
                Some(op.cookie()),
                Some(json!({"name": format!("reach-count-{n}-9d4")})),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
    }

    // The fewest tenant reads one answer took, over a few answers of each
    // route: anything else the control plane runs meanwhile only adds.
    for path in [
        format!("/admin/public-models/{public}"),
        "/admin/public-models".to_string(),
    ] {
        let mut fewest = usize::MAX;
        for _ in 0..3 {
            let before = TENANT_READS.load(Ordering::Relaxed);
            let r = cp.send("GET", &path, Some(op.cookie()), None).await;
            assert_eq!(r.status, 200, "{r:?}");
            let view = if r.body["items"].is_array() {
                r.body["items"][0].clone()
            } else {
                r.body.clone()
            };
            assert_eq!(view["reachable_tenants"], json!(TENANTS), "{view}");
            fewest = fewest.min(TENANT_READS.load(Ordering::Relaxed) - before);
        }
        assert!(
            fewest < TENANTS / 2,
            "GET {path} read a tenant's row by id {fewest} times for {TENANTS} tenants"
        );
    }
}
