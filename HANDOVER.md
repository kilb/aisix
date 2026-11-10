# Handover — Routio platform on the AISIX gateway

Written 2026-11-10 at `main` = `dad89946` (the P5 batch plus this document), for whoever — person or agent — picks this up next. It assumes no context from earlier sessions. Read it once end to end, then read the `CLAUDE.md` / `AGENTS.md` files it points at: those are the binding rules, this is the map.

---

## 1. What this project is

A multi-tenant AI reseller platform — tenants buy API access to language models under our own brand, **Routio** — built on a fork of the AISIX gateway. A tenant signs up, gets approved by an operator, is granted models and a provider group, mints an API key, and calls `/v1/chat/completions`, `/v1/messages`, `/v1/responses` and the other model endpoints. Every call is charged to a CNY ledger; an exhausted tenant is refused.

| Part | What it is | Where |
|---|---|---|
| **AISIX gateway** (data plane) | Forwards model traffic upstream. In this deployment it boots in *managed mode*: it reads its resources from etcd (kine) and writes none of its own configuration. | `crates/aisix-*` except the control plane |
| **Control plane (CP)** | Postgres is its source of truth. It validates every resource, projects the gateway's documents into kine, prices usage in CNY, keeps the ledger and the budget marks, and serves both the operator Dashboard and the tenant portal APIs. | `crates/aisix-control-plane` |
| **Two React apps** | Operator Dashboard (`web-cp`, admin.routio.ai) and tenant portal (`web-tenant`, routio.ai). | repo root |

**Production** is one host, `root@routio.ai` (the kit reaches it over `ssh` with `BatchMode=yes`, so passwordless root SSH must work before any deploy). Domains: `api.routio.ai` (gateway API), `admin.routio.ai` (Dashboard), `routio.ai` (portal). Stack: nginx with HTTP/3 in front, Postgres 18, kine 0.17.5, one gateway instance, one control plane, systemd units per component.

**Remotes.** `origin` is the fork `git@github.com:kilb/aisix.git` — that is where `main`, the PRs and the issues live. `upstream` is `api7/aisix` (the open-source gateway we fork). "`main` carries the release" always means the *fork's* `main`.

---

## 2. Repository map

### Rust workspace (`crates/`)

Gateway side — upstream AISIX code; prefer small, upstream-shaped changes since we merge periodically:

- `aisix-server` — the gateway binary's entry, config loading, listeners.
- `aisix-proxy` — HTTP handlers for every client-facing endpoint, failover dispatch, rate limits, budgets, guardrails, telemetry. Its `AGENTS.md` is long and binding; read it before touching a request path.
- `aisix-gateway` — provider dispatch, error scrubbing, SSE helpers; `upstream_http.rs` is where outbound calls go.
- `aisix-core` — the gateway's resource models (`src/models/`) and the source of the generated JSON schemas; `config.rs` defines every startup key.
- `aisix-etcd` — kine/etcd client and the snapshot loader.
- `aisix-obs`, `aisix-ratelimit`, `aisix-cache`, `aisix-redis`, `aisix-mcp`, `aisix-a2a`, `aisix-admin`, `aisix-guardrails`, `aisix-provider-*` — supporting crates.

Platform side — ours:

- **`aisix-control-plane`** — the platform brain. The files that matter most:
  - `tenants.rs` — tenants, grants, the model scope (`all` | `selected`), the tenant's team document. `update` is the operator write surface; every field it accepts also has an entry in its `fields` list for the audit row.
  - `keys.rs` — key minting (`sk-rt-` + 32 base62), the key document and its `KEY_DOC_VERSION` catch-up, plaintext reveal, group binding.
  - `channels.rs` — public models and channel models (§3), the generated direct/routing documents, retire and in-use counts.
  - `groups.rs` — provider groups (routing names) and group-rate revisions.
  - `members.rs` — `/api/*` signup, membership, roles; `/api/org` and the other tenant reads are here.
  - `billing/` — `accounts.rs` (balance, credit limit, exhausted), `ledger.rs` (append-only `_cny` tables), `marks.rs` (**the only writer of a blocking mark**), `budgets.rs` (quotas), `settler.rs`, `repricing.rs`, `calls.rs`, `lookup_prices.rs`, `lookup_quota.rs`.
  - `pricing/` — `engine`-driven charges, `margin.rs`, `import.rs` (dataset import), `library.rs` (price library + fetch), `cost_rates.rs` (per-account cost rate), `store.rs`, `api.rs`.
  - `resources/` — `write.rs` (`write_resource`, the only writer of a gateway document), `secrets.rs` (sealing, redaction, the "destination changed" rule), `refs.rs` (the resources lock).
  - `projector.rs` — the single kine writer, per environment prefix, fenced by an advisory lock.
  - `upstream_models.rs` — the CP's first outbound call: an upstream account's model list (SSRF-guarded, writes nothing).
  - `http.rs` — the router, `ROUTE_ROLES`, and `API_CONTRACT` (mirrored by `web-cp/src/lib/contract.ts`, pinned by a test).
  - `migrations/` — `0001`…`0034`, additive, never edited once shipped.
- **`aisix-pricing`** — the pure pricing engine: rate cards, the charge formula, `catalogue/list-prices.json`. Its `AGENTS.md` states the formula; `tests/equivalence.rs` and `golden.rs` (regenerate with `UPDATE_GOLDEN=1`) check the CNY engine against the gateway's own `Model.cost`.

### Web apps (`web-cp`, `web-tenant`)

Vite + React 19, no component library. Shared UI files (`Button`, `Field`, `Toast`, `TwoStep`, `Lamp`, `EmptyState`, `DataTable`, `Icon`, `CopyButton`, `KeySecret`, and `CONTRACT.md`) live in `web-cp/src/ui/` and are copied byte-for-byte into `web-tenant/src/ui/`; the money helper (`src/lib/money.ts`) is pinned the same way but lives under `lib/`. `web-tenant/src/lib/tokenParity.test.ts` lists both sets and fails if a copy drifts. UI copy is Chinese; code comments are English.

### Deploy kit and artefacts — **not in the repo**

Everything below lives under `/home/ubuntu/code/aisix/.claude/worktrees/merge-upstream/.superpowers/`, which is **gitignored**: it exists only on this machine, in the `merge-upstream` worktree, and every other worktree shares it. A fresh clone has none of it.

- `platform/deploy/routio/`
  - `build-release.sh <commit>` — builds and packs one commit. Needs `PROTOC`. Writes `releases/routio-<sha7>/` and `releases/routio-<sha7>.tar.gz` plus `.sha256`; the tarball holds `bin/aisix`, `bin/aisix-control-plane`, `bin/kine`, `config.managed.yaml`, `web/admin/`, `web/portal/`.
  - `gp-upgrade.sh` — the in-place upgrader: `preflight` (read-only), `precheck`, `run`, `checks`, `rollback <gateway|cp|all>`. **It is not generic**: its `NEW`, `NEW_SHA`, `PREV`, `ROLLBACK`, `APPLIED`, `NEW_MIGS`, `KEY_DOC` and `NEW_MIG_FILES` constants bind it to one upgrade, and `preflight` refuses unless `origin/main` is exactly `NEW_SHA`. The next upgrade starts by copying it and retargeting those constants, not by running it.
  - `host/` — `install-release.sh`, `switch-release.sh`, `check-host.sh`, `cp-admin.py`, `issue-gateway-bundle.py`, `pg-backup.sh`, `create-databases.sh`.
  - `nginx/` — the live `routio.ai.conf` and its bootstrap variant.
  - `secrets/` — 0600 credentials (§8).
  - `deploy-log.txt` — append-only; the script writes its own lines into it. `releases/` beside it holds each tarball, the preflight/precheck outputs and each run's log; that is where you reconstruct the last successful upgrade.
- `platform/design/` — `2026-11-06-platform-control-plane-design.md` (the design), the P0–P3 plans, the group-pricing design and plan, the P4 quota evaluation, `follow-up-issues.md`, UI direction, recon notes.
- `platform/node-dl/`, `platform/kine-dl/` — pinned Node 22.23.4 and kine 0.17.5 for tests.
- `sdd/2026-11-07-p3-deploy-plan/progress.md` — the deploy ledger (rulings, checks). The group-pricing ledger lives in a worktree, `.claude/worktrees/gp/.superpowers/sdd/2026-11-08-group-pricing-plan/progress.md`; between them they record every ruling and test count, and are the fastest way to learn why something is the way it is.

**Both kit scripts hardcode `REPO=/home/ubuntu/code/aisix`** (the main checkout) and the Node path above, so they build from and read tarballs out of the main checkout, not out of the worktree you are in.

---

## 3. The domain model in one page

- **Provider group** (`groups.rs`): a *routing name* (lowercase, immutable, never shown to tenants) plus a display name tenants see. An upstream account belongs to one or more groups; a tenant key belongs to exactly one; a grouped key is served only by accounts in that group. A **group rate** is an effective-dated row (a group default plus per-model overrides) in `pricing_group_rates`, in CNY per USD of official price — not a field on the group.
- **Upstream account** = a provider key record: provider family (`openai`, `anthropic`, …), `api_base`, the sealed credential, its groups. Editing a field that changes where the credential is sent (`provider`, `adapter`, `api_base`, `apis`, `resolve_addresses`, `tls`) requires re-entering it — deliberate, so a stored secret can never be redirected to a new destination.
- **Public model** (`channels.rs`): the tenant-facing model name, owning a generated routing document. Every account serving it gets a generated *direct* document `public#account` (`managed_by = public:<id>`). Those generated directs are internal: never in baselines, group rates, grants or tenant views. A **channel model** is one (public model, account) row.
- **Hand-made models** (模型与路由) are ordinary gateway documents, visible to pricing and grants but not reachable by an "all models" tenant.
- **Tenant**: status (`pending`/`active`/`suspended`), `billing_mode` (prepaid/postpaid, changeable in the Dashboard), `credit_limit_micro_cny`, `allowed_provider_groups`, `model_scope` (`all` = every public model including future ones; `selected` = the `allowed_model_ids` list). Keys stay closed while their group is absent from the tenant's list.
- **Money**: micro-CNY in `_cny` tables. Charge = official USD card × the key's group rate, rounded up once. Cost = the same card × the serving account's cost rate (`pricing_charges_cny.cost_micro_cny`); margin = charge − cost. `available = balance + credit_limit`; a tenant is exhausted when `available <= 0`, an i64 overflow counting as exhausted. The internal tenant (平台内部) is priced and posted but never marked.
- **A charge is written once.** A price mistake is fixed by a re-pricing run (an `adjustment` entry), never by editing the charge. The ledger is append-only.
- **Tenant-facing isolation**: `/api/*` answers never carry a provider, an upstream account, a routing name or an upstream model id — only display names. On the gateway, platform mode hides the channel best-effort; the strict part is isolation (sealed Responses ids, team-scoped cache, stored-object refusals).

---

## 4. How to work in this repo

### Build and test

```bash
# Rust workspace, the way CI runs it. PROTOC is the single most common first-command failure.
PROTOC=$HOME/.local/protoc-23.4/bin/protoc taskset -c 0-95 \
  CI=1 cargo test --workspace --all-features --no-fail-fast

# Control plane and pricing alone, with their services.
# Defaults are CP_TEST_DIR=$TMPDIR/aisix-cp-test-$USER, PG port 55432, kine 52379 —
# ALWAYS set both ports: 55432 is the Postgres §6 forbids you to touch.
export CP_TEST_DIR=<scratch>/cp-test-x CP_TEST_PG_PORT=57xxx CP_TEST_KINE_PORT=57xxx
export CP_TEST_KINE_BIN=/home/ubuntu/code/aisix/.claude/worktrees/merge-upstream/.superpowers/platform/kine-dl/bin-v0.17.5/kine-amd64
scripts/cp-test-services.sh start && eval "$(scripts/cp-test-services.sh env)"
cargo test -p aisix-control-plane -p aisix-pricing --all-features
scripts/cp-test-services.sh stop

# Gateway e2e (520+ spec files), from tests/e2e. It runs target/debug/aisix
# (AISIX_BIN overrides), and refuses a binary older than the sources under CI unset.
cd tests/e2e && pnpm install --frozen-lockfile
cargo build -p aisix-server            # bin name `aisix`; in the repo root, so target/debug/aisix is fresh
E2E_THREAD_PER_CORE=true  pnpm test    # then =false for the work-stealing leg
# AISIX_E2E_REDIS_PLAIN / AISIX_E2E_ETCD_ENDPOINTS are needed by some cases; see ci.yml

# Web apps: unit, types, build, then Playwright against the real stack.
# stack.ts needs CP_E2E_PG_URL (a role that may CREATE DATABASE) and CP_E2E_KINE_BIN,
# the debug binaries above, and dist/ built for "/" (web-tenant WITHOUT TENANT_BASE).
cd web-cp && npm ci && npm run typecheck && npm run test:unit && npm run build && npx playwright test
```

`crates/aisix-control-plane/CLAUDE.md` explains why the workspace run matters (feature unification turns on `aws-lc-rs` beside `ring`; a per-crate build hides it) and why a run without `CP_TEST_*` silently executes none of its integration tests. `tests/e2e/AGENTS.md` explains the readiness-gate and log-line rules those specs follow.

The Playwright stack is `scripts/cp-dev-stack.mjs` (Postgres, kine, the CP, a mock upstream, the gateway, `vite preview`). **Serialize e2e across worktrees** with `flock` on the shared lock file under the kit; two stacks at once produce port flakes that look like product bugs.

### Format, lint, audit

`cargo fmt --all -- --check`; `cargo clippy --workspace --all-features --all-targets -- -D warnings`; `cargo audit` (its two ignores are justified inline in `.cargo/audit.toml`).

### Schemas and contracts

- **A gateway resource field** starts in `crates/aisix-core/src/models/`, then `cargo run -p aisix-core --bin dump-schema` regenerates `schemas/resources/` and `schemas/resources-lenient/` (commit them), and `cargo test -p aisix-core --test resource_schema_characterization`. The strict set is the write path; the lenient set is what the gateway loads. Prove a new required field is required only in the strict set.
- **A CP-only field** (a tenant column, an `/admin` or `/api` field) has no schema entry: it needs a migration, the handler, a `ROUTE_ROLES` row, and — if `/api` answers change shape — an `API_CONTRACT` bump mirrored in `web-cp/src/lib/contract.ts` (and the portal's copy).

### Deploy to routio.ai

```bash
KIT=/home/ubuntu/code/aisix/.claude/worktrees/merge-upstream/.superpowers/platform/deploy/routio
$KIT/build-release.sh <sha>          # tarball in $KIT/releases/
# copy gp-upgrade.sh, retarget NEW/NEW_SHA/PREV/ROLLBACK/APPLIED/NEW_MIGS/KEY_DOC/NEW_MIG_FILES
$KIT/gp-upgrade.sh preflight         # read-only: fork main, tarball, host state, CSP hashes, data pre-check
$KIT/gp-upgrade.sh run               # dump → install → CP+apps → gateway → every check
$KIT/gp-upgrade.sh rollback <gateway|cp|all>
```

`run` takes a Postgres dump to `/var/backups/routio/pg/`, switches the control plane and both apps together (they share `api_contract`), then the gateway. It writes its own lines to `deploy-log.txt`. The hourly host check is paused for the switch and resumed by a trap; **a run killed mid-switch leaves the CP and apps on the new release and the gateway on the old one** — resume the timer by hand and finish or roll back.

**Data-loss checklist for an upgrade:** the dump exists and restores (`pg_restore --list`); `database_id` is unchanged; migrations are monotonic and every checksum matches the release files; kine's uptime is unbroken; the rollback release is still on the host. A full restore is a separate, owner-approved operation — the script never runs it.

### Rules that have bitten us

- **One writer per thing.** Gateway documents: only `write_resource`. Marks: only `marks::project`. kine: only the projector leader. Bypassing any still answers 200, and the gap shows days later.
- **Lock order**: tenant row → billing account → group / public-model row → `RESOURCES_LOCK_KEY`. Anything that moves money or decides a mark takes `lock_billing` first. `tests/billing_marks.rs` and `tests/tenants.rs` census the paths; a new one adds a case.
- **A migration number is taken once.** Three branches in one batch each grabbed `0032`; merge serially and renumber the later ones. Migrations are additive; a projected field's shape or meaning never changes in place.
- **Census tests are load-bearing**: `ROUTE_ROLES`, `guardrail_coverage.rs`, `metric-endpoint-parity-e2e.test.ts`, `endpoint_acl.rs`'s family census, `tokenParity.test.ts`, `ui_contracts_match`. A route, endpoint or shared UI file added without them fails a test you did not run.
- **An e2e that passes on the broken code is not a test.** For a money or permission rule, break the code and watch the test go red.
- **The SSRF guard is real** in the CP's outbound calls; tests reach a loopback stand-in only through a library-level seam (`TestCp::start_reaching_loopback`, `start_with_sources`), never an env var, and there is deliberately no build-time bypass (#80).
- **Flaky-looking failures are usually the environment**: a locked table under `lock_timeout` (#81), a `datetime-local` input that drops `:00`, a port another worktree holds. Reproduce before "fixing" a product.

---

## 5. Current state (2026-11-10)

**Live on routio.ai** (runs `dad89946`): signup (rate-limited), tenant approval, groups and rates, public models and channel mapping including bulk fetch from an upstream account, baselines plus an imported price library with one-click attested sync, cost rate per account, the CNY ledger with top-ups/credits/quotas/re-pricing, the tenant portal, and API key plaintext reveal.

**Done in the platform plan**: P0 (gateway), P1 (control plane + Dashboard), P2 (pricing and billing), P3 (deploy), the group-pricing phase, the P4 dependency and quota-reduction batch, and the P5 batch (cost rate, price library, key reveal, Routio branding, credit limit in both modes). PRs #64 and #79 merged.

**Open** (all on the **fork's** tracker, i.e. `kilb/aisix`, where `main` and the PRs live):

- `follow-up-issues.md` in the design kit lists the follow-ups filed after PR #64; on the fork they are issues **#65–#76 and #78, #80, #81** (PR #79 is the code batch, not an issue). Read each issue's state in the tracker rather than this file. The ones worth knowing: **#77** (audit advisories: the four are fixed, and one justified ignore remains in `.cargo/audit.toml`), **#78** (one flaky dev-stack port; its TwoStep half is fixed), **#80** (the upstream-model list cannot be driven in e2e past the SSRF guard — deliberate, options recorded), **#81** (a body-slot test that raced `lock_timeout` — fixed and closed).
- **P4 remainder**: adapt and bill the second-phase endpoints (images, audio, video, MCP, A2A) for platform mode — each its own plan, written into `platform/design/`.
- **Functional gaps**: no online payment (manual top-ups only); bulk upstream mapping is capped at 100 names per confirmation (`MAX_BULK_MODELS` in `channels.rs`, and the Dashboard refuses an over-cap selection before sending); the price library fetches one fixed URL (`LIBRARY_URL` in `pricing/library.rs`).

---

## 6. Things a new agent will trip on

1. **No user-facing prose in the repo.** User documentation lives in `api7/docs`. `docs/design/` holds two historical gateway design notes, kept deliberately and marked as such; do not add pages there or anywhere else under `docs/`.
2. **The kit is not in the repo** (`.superpowers/` is gitignored), so a fresh clone cannot deploy or run the UI tests. It is shared by every worktree.
3. **The fork tracks upstream.** Gateway crates are upstream's; expect periodic merges from `upstream`. `aisix-control-plane`, `aisix-pricing` and both web apps are ours.
4. **Worktrees.** Development happens in `git worktree`s; each has its own `target/` (minutes of compile). Reuse one worktree for a run of small changes, and stop the services you start.
5. **Ports.** Never touch the Postgres on 55432/55532 (55432 is `cp-test-services.sh`'s own default — always override it). Serialize e2e with the shared `flock`.
6. **Secrets.** Never print an environment dump. Credentials are 0600 files: on the host `/root/routio/operator.{email,pass}` and `/etc/aisix/*`; locally the kit's `secrets/`. Never put a secret on a command line, in a log, an audit row or a commit.
7. **The operator account** is `ops-…@routio.ai` (the deploy log records the current one); its password is in `secrets/operator.pass`. There is no admin/admin.
8. **`/api/*` and `/admin/*` are different contracts**: `/api` is tenant-facing and whitelisted field by field; `/admin` is operator-only. Never let a field of one leak into the other.
9. **Browser UI changes need Playwright against the real stack** — no stubbed network. Build the debug binaries and `dist/` first (see §4).
10. **A deployment is not done until the checks pass** — `check-host`, liveness, `check-edge`, `check-csp`, `signup_open`, HTTP/3 — and `deploy-log.txt` has the entry.

---

## 7. Where to start, by task

- **"Add a field the operator edits"** (CP-only): migration → the row and `tenants::update` (or the matching handler) with its `fields` audit entry → a `ROUTE_ROLES` row → `web-cp` form and, if tenants see it, a whitelisted `/api` route with an `API_CONTRACT` bump and both `contract.ts` copies → tests. A CP field is **not** an `aisix-core` model.
- **"Add a field to a gateway resource"**: model in `aisix-core/src/models/`, `dump-schema`, CP validation and projection, Dashboard form, a `compat::FIELD_FLOORS` row if an older gateway would misread it, and the lenient-load test.
- **"Change pricing"**: `crates/aisix-pricing/AGENTS.md` first (the formula, one rounding, the discount inside the same ceiling), then the control plane's pricing module; keep `equivalence.rs`/`golden.rs` green (`UPDATE_GOLDEN=1` to regenerate) and prove the money test by mutation. Remember a charge is immutable — a correction is a re-pricing run.
- **"Change what tenants can reach"**: `tenants.rs` (`model_scope`, grants), `keys.rs` (key documents), `channels.rs` (generated documents), then the `/api` views and both web apps; check the tenant views stay whitelisted.
- **"Change the gateway's request path"**: read `crates/aisix-proxy/AGENTS.md` in full first; run both gateway e2e modes.
- **"Deploy"**: build the tarball, retarget and run `gp-upgrade.sh run`, keep the previous release, check `deploy-log.txt`.

When something is ambiguous, the design documents in `platform/design/` and the ledgers named in §2 usually answer it, with the reasoning and the date.
