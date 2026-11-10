# Handover — Routio platform on the AISIX gateway

Written 2026-11-10 at `main` = `dbdcd1f0`, for whoever (person or agent) picks this up next. It assumes no context from earlier sessions. Read it once end to end before touching anything; then read the `CLAUDE.md` and `AGENTS.md` files it points at, which are the binding rules of this repository.

---

## 1. What this project is

A multi-tenant AI reseller platform — tenants buy API access to language models under our own brand, **Routio** — built on a fork of the AISIX gateway. A tenant signs up, gets approved by an operator, is granted models and a provider group, mints an API key, and calls `/v1/chat/completions`, `/v1/messages`, `/v1/responses` and the other model endpoints. Every call is charged to a CNY ledger; an exhausted tenant is refused.

Three moving parts:

| Part | What it is | Where |
|---|---|---|
| **AISIX gateway** (data plane) | Forwards model traffic upstream. In this deployment it boots in *managed mode* against etcd (kine) and writes no configuration of its own. | `crates/aisix-*` except the control plane |
| **Control plane (CP)** | Postgres is its source of truth. It validates every resource, projects the gateway's documents into kine, prices usage in CNY, keeps the ledger and the budget marks, serves the operator Dashboard and the tenant portal APIs. | `crates/aisix-control-plane` |
| **Two React apps** | Operator Dashboard (`web-cp`, admin.routio.ai) and tenant portal (`web-tenant`, routio.ai). | repo root |

Production: **routio.ai** (API: `api.routio.ai`; Dashboard: `admin.routio.ai`; portal: `routio.ai`), one host, nginx + HTTP/3 in front, Postgres 18, kine 0.17.5, one gateway instance, one control plane, systemd units per component.

---

## 2. Repository map

### Rust workspace (`crates/`)

Gateway side — these come from upstream AISIX and we prefer to stay close to it:

- `aisix-server` — the gateway binary's entry, config loading, listeners.
- `aisix-proxy` — the HTTP handlers for every client-facing endpoint, try/failover dispatch, rate limits, budgets, guardrails, telemetry. Its `AGENTS.md` is long and binding; read it before touching any request path.
- `aisix-gateway` — provider dispatch, error scrubbing, SSE helpers. `upstream_http.rs` is where outbound calls go.
- `aisix-core` — resource models (`crates/aisix-core/src/models/`) and the generated JSON schemas' source. `config.rs` defines every startup key.
- `aisix-etcd` — the kine/etcd client and snapshot loader.
- `aisix-obs`, `aisix-ratelimit`, `aisix-cache`, `aisix-redis`, `aisix-mcp`, `aisix-a2a`, `aisix-admin`, `aisix-guardrails`, `aisix-provider-*` — supporting crates.

Platform side — ours, and where most recent work lives:

- **`aisix-control-plane`** — the whole platform brain. The files that matter most, by size and traffic:
  - `tenants.rs` — tenants, grants, the model scope (`all` | `selected`), projection of the tenant's team document.
  - `keys.rs` — API key minting (`sk-rt-` + 32 base62), the key document and its `KEY_DOC_VERSION` catch-up, plaintext reveal, group binding.
  - `channels.rs` — *public models* and *channel models* (see §3), the generated direct/routing documents, retire and in-use counts.
  - `groups.rs` — provider groups (routing names), group rate revisions.
  - `members.rs` — `/api/*` signup, membership, roles.
  - `billing/` — `accounts.rs` (balance, credit limit, exhausted), `ledger.rs` (append-only, `_cny` tables), `marks.rs` (the budget documents the gateway reads; **the only writer of a blocking mark**), `budgets.rs` (quotas per level), `settler.rs`, `calls.rs`, `lookup_prices.rs`, `lookup_quota.rs`.
  - `pricing/` — `engine`-driven charge computation, `margin.rs`, `import.rs` (third-party dataset import), `library.rs` (the price library + fetch), `cost_rates.rs` (per-account cost rate), `store.rs`, `api.rs`.
  - `resources/` — `write.rs` (`write_resource`, the only writer of a gateway document), `secrets.rs` (sealing, redaction, the "destination changed" rule), `refs.rs` (the resources lock).
  - `projector.rs` — the single kine writer, per environment prefix, fenced by an advisory lock.
  - `upstream_models.rs` — the only other outbound call: fetches an upstream account's model list (SSRF-guarded).
  - `http.rs` — the router and `ROUTE_ROLES`; every route needs a row there or the census fails.
  - `migrations/` — `0001`…`0034`, additive and never edited once shipped.
- **`aisix-pricing`** — the pure pricing engine shared with the control plane: rate cards, the charge formula, `catalogue/list-prices.json`. Its `tests/equivalence.rs` and `golden.rs` compare the CNY engine against the gateway's own `Model.cost` computation; they must stay green.

### Web apps (`web-cp`, `web-tenant`)

Vite + React 19, no component library. Shared UI files (`Button`, `Field`, `TwoStep`, `CopyButton`, `KeySecret`, `Money`, …) live in `web-cp/src/ui/` and are copied byte-for-byte into `web-tenant/src/ui/`; `web-tenant/src/lib/tokenParity.test.ts` fails if the copies drift. UI copy is Chinese; code comments are English.

### Deploy kit — **outside the repo**

`/home/ubuntu/code/aisix/.claude/worktrees/merge-upstream/.superpowers/platform/`

- `deploy/routio/` — `build-release.sh` (tarball), `gp-upgrade.sh` (`preflight` / `run` / `checks` / `rollback`), `host/` (install, switch, checks, backups), `nginx/`, `secrets/` (0600; see §8), `deploy-log.txt` (every deploy step, append-only).
- `design/` — the platform design (`2026-11-06-platform-control-plane-design.md`), the P0–P3 plans, the group-pricing design and plan, the P4 quota evaluation, `follow-up-issues.md`, UI direction and recon notes.
- `node-dl/`, `kine-dl/` — pinned Node 22.23.4 and kine 0.17.5 for tests.

### Session artefacts — also outside the repo

`.superpowers/sdd/2026-11-08-group-pricing-plan/` holds `progress.md`, the ledger of every ruling and test count to date. It is the fastest way to learn why something is the way it is.

---

## 3. The domain model in one page

- **Provider group** (`groups.rs`): a *routing name* (lowercase, immutable) plus a display name tenants see. An upstream account belongs to one or more groups; a tenant key belongs to exactly one group; a request from a key is only served by accounts in that group. Group rates are CNY per USD of official price, with a group default and per-model overrides, effective-dated.
- **Upstream account** = a provider key record: provider family (`openai`, `anthropic`, …), `api_base`, the credential, its groups. Its credential is sealed at rest and never displayed; editing a field that changes where the credential is sent (`provider`, `adapter`, `api_base`, …) requires re-entering it — deliberate, so a stored secret can never be redirected.
- **Public model** (`channels.rs`): the tenant-facing model name. A public model owns a generated routing document. Each account that serves it gets a generated *direct* document `public#account`, `managed_by = public:<id>`. These generated directs are internal: they never appear in baselines, group rates, grants or tenant views. **Channel model** means one (public model, account) row.
- **Hand-made models** (in 模型与路由) are ordinary gateway model documents; they are visible to pricing and grants but are not reachable by an "all models" tenant.
- **Tenant**: status (`pending`/`active`/`suspended`), `billing_mode` (prepaid/postpaid, changeable in the Dashboard), `credit_limit_micro_cny`, `allowed_provider_groups`, and `model_scope` (`all` = every public model, including future ones; `selected` = the `allowed_model_ids` list). Keys are closed while their group is absent from the tenant's list.
- **Money**: everything is micro-CNY in `_cny` tables. A charge = the official USD rate card × the key's group rate (CNY per USD), rounded up once. Cost = the same card × the serving account's cost rate; margin = charge − cost. `available = balance + credit_limit`; the tenant is marked exhausted when `available <= 0` (an i64 overflow counts as exhausted). The internal tenant (平台内部) is priced and posted but never marked.
- **Tenant identity on the wire**: `/api/*` answers never carry a provider, an upstream account, a routing name or an upstream model id — only display names. Platform mode (`proxy.platform.enabled`) hides the channel on the gateway side too, best effort; the strict part is isolation (sealed Responses ids, team-scoped cache, stored-object refusals).

---

## 4. How to work in this repo

### Build and test

```bash
# Rust, whole workspace, the way CI runs it
PROTOC=$HOME/.local/protoc-23.4/bin/protoc taskset -c 0-95 CI=1 cargo test --workspace --all-features --no-fail-fast

# Control plane and pricing alone, with its services
export CP_TEST_DIR=<scratch>/cp-test-x CP_TEST_PG_PORT=57xxx CP_TEST_KINE_PORT=57xxx
export CP_TEST_KINE_BIN=<platform>/kine-dl/bin-v0.17.5/kine-amd64
scripts/cp-test-services.sh start && eval "$(scripts/cp-test-services.sh env)"
cargo test -p aisix-control-plane -p aisix-pricing --all-features
scripts/cp-test-services.sh stop

# Gateway e2e (520+ spec files), two modes
# read .github/workflows/ci.yml and tests/e2e/package.json for the entry point
SEATEOF_THREAD_PER_CORE=true|false ...

# Web apps
cd web-cp && npm ci && npm run typecheck && npm run test:unit && npm run build && npx playwright test
```

`crates/aisix-control-plane/CLAUDE.md` explains why the workspace run matters (feature unification turns on `aws-lc-rs` beside `ring`; a per-crate build hides that) and why a `CP_TEST_*`-less run silently runs none of its integration tests.

### Format and lint

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-features --all-targets -- -D warnings`, `cargo audit` (config in `.cargo/audit.toml`; the two ignores there are justified inline).

### Schema

`cargo run -p aisix-core --bin dump-schema` regenerates `schemas/resources/` and `schemas/resources-lenient/`; commit the result with the model change, and run `cargo test -p aisix-core --test resource_schema_characterization`. The strict schema is the write path; the lenient one is what the gateway loads — prove any new required field is required only in the strict set.

### Deploy to routio.ai

`gp-upgrade.sh run` after `main` carries the release (the script refuses to start otherwise). It takes a dump, installs the release, switches the control plane and apps, then the gateway, and runs every check. Rollback is per component (`gp-upgrade.sh rollback <cp|gateway|all>`). Keep at least the previous release on the host as the rollback target.

### Rules that have bitten us

- **One writer per thing.** Gateway documents: only `write_resource`. Marks: only `marks::project`. kine: only the projector leader. Bypassing any of these still answers 200 and the gap shows days later.
- **Lock order**: tenant row → billing account → group / public-model row → `RESOURCES_LOCK_KEY`. A path that moves money or decides a mark takes `lock_billing` first. `tests/billing_marks.rs` and `tests/tenants.rs` census the paths; a new one adds a case.
- **A migration number is taken once.** Three branches in one batch got `0032` — merge serially and renumber later ones. Migrations are additive; a projected field's shape or meaning never changes in place.
- **Census tests are load-bearing**: `ROUTE_ROLES` (`http.rs`), `guardrail_coverage.rs`, `metric-endpoint-parity-e2e.test.ts`, the endpoint family census in `endpoint_acl.rs`, the parity test for the two UI copies. A route or endpoint added without them fails a test you have not run.
- **Test discipline**: an e2e that passes on the broken code is not a test. When you add one for a money or permission rule, break the code and watch it go red.
- **The SSRF guard is real in the CP's outbound calls.** Tests reach a loopback stand-in only through a library-level seam (`TestCp::start_reaching_loopback`, `start_with_sources`), never an env var, and there is deliberately no build-time bypass (#80 records that decision).
- **Flaky-looking failures are usually the environment**: a locked table under `lock_timeout`, a `datetime-local` input that drops `:00`, a Postgres/kine port another worktree holds. Reproduce before "fixing" a product.

---

## 5. Current state (2026-11-10)

**Shipped and live on routio.ai**: signup open (rate-limited), tenant approval, groups and rates, public models and channel mapping including bulk fetch from an upstream account, baselines plus an imported price library with one-click synced baselines, cost rate per account, the CNY ledger with top-ups, credits, quotas, re-pricing, the tenant portal (keys, usage, balance), and API key plaintext reveal.

**Done in the platform plan**: P0 (gateway), P1 (control plane + Dashboard), P2 (pricing and billing), P3 (deploy), the group-pricing phase, the P4 dependency and quota-reduction batch, and the P5 batch (cost rate, price library, key reveal, Routio branding, credit limit in both modes). PR #64 and #79 merged; `main` = `dbdcd1f0`.

**Open**:

- `follow-up-issues.md` and GitHub issues **#65–#81**. The ones worth knowing: **#77** (dependency advisories — fixed; the remaining ignore is justified in `.cargo/audit.toml`), **#78** (one flaky dev-stack port; the TwoStep half is fixed), **#80** (the upstream-model list cannot be driven in e2e past the SSRF guard — deliberate), **#81** (a body-slot test that raced `lock_timeout` — fixed).
- **P4 remainder**: adapt and bill the second-phase endpoints (images, audio, video, MCP, A2A) for platform mode, each its own plan.
- **Known functional gaps**: no online payment (manual top-ups only); a public model cannot be bulk-imported as a group of upstream models in one click beyond the 100-name cap; the price library fetch is a single fixed URL.

---

## 6. Things a new agent will trip on

1. **The repo has no `docs/` prose.** User documentation lives in `api7/docs`; do not add pages here.
2. **The fork tracks upstream** (`git remote upstream` = api7/aisix). Gateway crates are upstream's; prefer small, upstream-shaped changes and expect to merge periodically. `aisix-control-plane`, `aisix-pricing` and both web apps are ours.
3. **Worktrees.** Development happens in `git worktree`s under `.claude/worktrees/`; each has its own `target/` (minutes of compile). Reuse one worktree for a series of small changes instead of creating one per task, and stop the services you start.
4. **Ports.** Never touch the Postgres on 55432/55532. Control-plane test services pick their own ports; the Playwright stacks pick random ones; serialize e2e across worktrees with `flock` on a shared lock file.
5. **Secrets.** Never print an environment dump. Credentials live in 0600 files: on the host `/root/routio/operator.{email,pass}` and `/etc/aisix/*`; locally the kit's `secrets/`. Never paste a secret into a command line, a log, an audit row or a commit.
6. **The operator account** is `ops-…@routio.ai` (see the deploy log for the current one); its password is in the kit's `secrets/operator.pass`. There is no "admin/admin".
7. **`/api/*` and `/admin/*` are different contracts**: `/api` is tenant-facing and whitelisted field by field; `/admin` is operator-only. Never let a field of one leak into the other.
8. **Browser UI changes** need Playwright against the real stack (`e2e/stack.ts` boots Postgres, kine, the CP, a mock upstream, the gateway and `vite preview`). No stubbed network.
9. **A deployment is not done until the checks pass** — `check-host`, liveness, `check-edge`, `check-csp`, `signup_open`, HTTP/3 — and `deploy-log.txt` has the entry.

---

## 7. Where to start, by task

- **"Add a field to a resource"** — model in `aisix-core/src/models/`, regenerate schemas, control-plane validation and projection, Dashboard form, a `FIELD_FLOORS` row if an older gateway would misread it, and the lenient-load test.
- **"Change pricing"** — `aisix-pricing` first, then the control plane's pricing module; keep `equivalence.rs`/`golden.rs` green and prove the money test by mutation.
- **"Change what tenants can reach"** — `tenants.rs` (`model_scope`, grants), `keys.rs` (key documents), `channels.rs` (generated documents), then the `/api` views and both web apps; check the tenant views stay whitelisted.
- **"Change the gateway's request path"** — read `crates/aisix-proxy/AGENTS.md` in full first; run both gateway e2e modes.
- **"Deploy"** — build the tarball, run `gp-upgrade.sh run`, keep the previous release, log it.

When something is ambiguous, the design documents in `.superpowers/platform/design/` and the ledger in `.superpowers/sdd/2026-11-08-group-pricing-plan/progress.md` usually answer it, with the reasoning and the date.
