# Handover — Routio platform on the AISIX gateway

Written 2026-11-10 at `main` = `5b10cf23` (PR #82), production at `dev+sha-5b10cf2`, for whoever — person or agent — picks this up next. It assumes no context from earlier sessions. Read it once end to end, then read the `CLAUDE.md` / `AGENTS.md` files it points at: those are the binding rules, this is the map.

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

**Working style with the owner** (learned over many rounds, and it shapes every task):

- The conversation is Chinese; **code comments, commit messages, plans and documents are English**; UI copy is Chinese.
- Small platform features go the **fast path**: one implementer agent, tests for the crates and Playwright specs the change reaches, no independent audit, merge and deploy without another round trip. The full gate (workspace test run, both gateway e2e modes, an independent audit agent) is for money arithmetic, tenant isolation, auth, or the gateway request path.
- Deploying to routio.ai and merging into the fork's `main` are standing authorizations. Anything destructive (deleting data, rotating credentials, restoring a backup) is not.
- When a decision is genuinely the owner's and the recommendation is clear, **decide and report** rather than blocking.

---

## 2. Repository map

### Rust workspace (`crates/`, 20 crates)

Gateway side — upstream AISIX code; prefer small, upstream-shaped changes since we merge periodically:

- `aisix-server` — the gateway binary's entry, config loading, listeners.
- `aisix-proxy` — HTTP handlers for every client-facing endpoint, failover dispatch, rate limits, budgets, guardrails, telemetry. Its `AGENTS.md` is long and binding; read it before touching a request path.
- `aisix-gateway` — provider dispatch, error scrubbing, SSE helpers; `upstream_http.rs` is where outbound calls go.
- `aisix-core` — the gateway's resource models (`src/models/`) and the source of the generated JSON schemas; `config.rs` defines every startup key.
- `aisix-etcd` — kine/etcd client and the snapshot loader.
- `aisix-obs`, `aisix-ratelimit`, `aisix-cache`, `aisix-redis`, `aisix-mcp`, `aisix-a2a`, `aisix-admin`, `aisix-guardrails`, `aisix-provider-*` — supporting crates.

Platform side — ours:

- **`aisix-control-plane`** — the platform brain. Files that matter most:
  - `tenants.rs` — tenants, grants, the model scope (`all` | `selected`), the tenant's team document. `update` is the operator write surface; every field it accepts also has an entry in its `fields` list for the audit row.
  - `keys.rs` — key minting (`sk-rt-` + 32 base62 chars, rejection sampling), the key document and its `KEY_DOC_VERSION` catch-up, plaintext reveal, group binding.
  - `channels.rs` — public models and channel models (§3), the generated direct/routing documents, the bulk upstream mapping, retire and in-use counts.
  - `groups.rs` — provider groups (routing names) and group-rate revisions.
  - `members.rs` — `/api/*` signup, membership, roles; `/api/org` and the other tenant reads.
  - `billing/` — `accounts.rs` (balance, credit limit, exhausted), `ledger.rs` (append-only `_cny` tables), `marks.rs` (**the only writer of a blocking mark**), `budgets.rs` (quotas), `settler.rs`, `repricing.rs`, `calls.rs`, `lookup_prices.rs`, `lookup_quota.rs`.
  - `pricing/` — where most recent work landed:
    - `api.rs` — the operator pricing routes, incl. `/admin/pricing/status`.
    - `store.rs` — loads the `PriceBook`; `unpriced_by_code` feeds the status page.
    - `baselines.rs` — effective-dated official price cards; built-in catalogue writes.
    - `cost_rates.rs` — the per-account cost rate (CNY per USD).
    - `import.rs` — the dataset importer. **Read its module doc first**: it defines how a dataset entry becomes a card, and the flag taxonomy (`bad_number`, `unsupported_field`, `truncated_field`, `unsupported_variant`, `time_of_day`, `invalid_card`). `dimension()` is the field→dimension table; `Flag::blocks_sync()` decides whether a flagged entry may still be confirmed/synced.
    - `library.rs` — the price library: fetch from one fixed `LIBRARY_URL`, store the dataset text, preview a sync per model (`create` / `update` / `same` / `flagged` / `unmatched` / `ambiguous`), candidate binding, attested sync.
    - `margin.rs` — charge − cost by group/model.
  - `resources/` — `write.rs` (`write_resource`, the only writer of a gateway document), `secrets.rs` (sealing, redaction, the "destination changed" rule), `refs.rs` (the resources lock).
  - `projector.rs` — the single kine writer, per environment prefix, fenced by an advisory lock.
  - `upstream_models.rs` — the CP's first outbound call: an upstream account's model list (SSRF-guarded, writes nothing).
  - `http.rs` — the router, `ROUTE_ROLES`, and `API_CONTRACT` (mirrored by `web-cp/src/lib/contract.ts`, pinned by a test).
  - `migrations/` — `0001`…`0037`, additive, never edited once shipped.
- **`aisix-pricing`** — the pure pricing engine: rate cards, the charge formula, `catalogue/list-prices.json` (27 entries). Its `AGENTS.md` states the formula; `tests/equivalence.rs` and `tests/golden.rs` check the CNY engine against the gateway's own `Model.cost` (`UPDATE_GOLDEN=1` to re-record, which the rules mostly forbid).

### Web apps (`web-cp`, `web-tenant`)

Vite + React 19, no component library. Shared UI files (`Button`, `Field`, `Toast`, `TwoStep`, `Lamp`, `EmptyState`, `DataTable`, `Icon`, `CopyButton`, `KeySecret`, and `CONTRACT.md`) live in `web-cp/src/ui/` and are copied byte-for-byte into `web-tenant/src/ui/`; the money helper (`src/lib/money.ts`) is pinned the same way but lives under `lib/`. `web-tenant/src/lib/tokenParity.test.ts` lists both sets and fails if a copy drifts. UI copy is Chinese; code comments are English.

Dashboard tabs: 概览 / 网关 / 上游账号 / 模型与路由 / 价格 / 租户 / 计费 / 密钥 / 缓存策略 / 全部资源 / 运维账号 / 审计日志. 价格 is a section host (计价状态, 基准价, 导入审核, 公共模型, 价格库, 价格目录, 计费明细).

### Deploy kit and artefacts — **not in the repo**

Everything below lives under `/home/ubuntu/code/aisix/.claude/worktrees/merge-upstream/.superpowers/`, which is **gitignored**: it exists only on this machine, in the `merge-upstream` worktree, and every other worktree shares it. A fresh clone has none of it.

- `platform/deploy/routio/`
  - `build-release.sh <commit>` — builds and packs one commit. Needs `PROTOC` (`$HOME/.local/protoc-23.4/bin/protoc`). Writes `releases/routio-<sha7>/` and `releases/routio-<sha7>.tar.gz` plus `.sha256`.
  - `gp-upgrade.sh` — the in-place upgrader: `preflight` (read-only), `precheck`, `run`, `checks`, `rollback <gateway|cp|all>`. **It is not generic**: its `NEW`, `NEW_SHA`, `PREV`, `PREV_SHA`, `ROLLBACK`, `APPLIED`, `NEW_MIGS`, `KEY_DOC`, `NEW_MIG_FILES`, `DBID`, `ENV_ID` and `RETIRED_FAMILY` constants bind it to one upgrade, and `preflight` refuses unless `origin/main` is exactly `NEW_SHA`. **Its header comment is stale by design** — it describes the upgrade it was first written for — so read the constants, never the prose. The next upgrade edits it **in place** (keep a `.bak-<sha>` beside it): `KIT` is derived from `${BASH_SOURCE[0]}`, so a copy anywhere outside `deploy/routio/` resolves `releases/` and `deploy-log.txt` to the wrong directory and dies on the tarball checksum. Two of its checks are derived rather than hard-coded: the migration set (`NEW_MIG_FILES` compared against the files actually changed between `PREV` and `NEW`), and the "new objects already present" gate, asked only when `NEW_MIGS > APPLIED`; a migration that adds a column also has to update that gate's query.
  - `host/` — `install-release.sh`, `switch-release.sh`, `check-host.sh`, `cp-admin.py`, `issue-gateway-bundle.py`, `pg-backup.sh`, `create-databases.sh`.
  - `nginx/` — the live `routio.ai.conf` and its bootstrap variant.
  - `secrets/` — 0600 credentials.
  - `deploy-log.txt` — append-only; the script writes its own lines into it (dump path, money before/after, switch, checks). `releases/` beside it holds each tarball, the preflight/precheck outputs and each run's log; that is where you reconstruct the last successful upgrade.
- `platform/design/` — the design (`2026-11-06-platform-control-plane-design.md`), the P0–P3 plans, the group-pricing design and plan, the P4 quota evaluation, `follow-up-issues.md`, UI direction, recon notes.
- `platform/node-dl/`, `platform/kine-dl/` — pinned Node 22.23.4 and kine 0.17.5 for tests.
- `sdd/2026-11-07-p3-deploy-plan/progress.md` and `sdd/2026-11-08-group-pricing-plan/progress.md` — the two ledgers (rulings, checks). Between them they record every ruling and test count, and are the fastest way to learn why something is the way it is.

**Both kit scripts hardcode `REPO=/home/ubuntu/code/aisix`** (the main checkout) and the Node path above, so they build from and read tarballs out of the main checkout, not out of the worktree you are in.

---

## 3. The domain model in one page

- **Provider group** (`groups.rs`): a *routing name* (lowercase, immutable, never shown to tenants) plus a display name tenants see. An upstream account belongs to one or more groups; a tenant key belongs to exactly one; a grouped key is served only by accounts in that group. A **group rate** is an effective-dated row (a group default plus per-model overrides) in `pricing_group_rates`, in CNY per USD of official price — not a field on the group.
- **Upstream account** = a provider key record: provider family (`openai`, `anthropic`, …), `api_base`, the sealed credential, its groups. Editing a field that changes where the credential is sent (`provider`, `adapter`, `api_base`, `apis`, `resolve_addresses`, `tls`) requires re-entering it — deliberate, so a stored secret can never be redirected to a new destination.
- **Public model** (`channels.rs`): the tenant-facing model name. It generates:
  - a **routing document** whose id IS the public model's id — the row a caller names, and the row the operator addresses;
  - one **direct document per serving account**, named `<public>#<account>`, carrying `internal: true` — each one is a routing TARGET and nothing else.
  All of them carry `managed_by = "public:<id>"`, so a hand edit or delete of the document itself is refused `409 managed_resource`. Two consequences a change must respect:
  - **The routing settings belong to the public model, not to the document.** `public_models.routing` (migration `0037`) stores the non-target settings (strategy, retries, fallbacks, retry statuses, all-unavailable policy); `routing_doc()` merges them and always regenerates `targets` from `channel_models`, so a hand edit to the document would be silently reverted and `targets` can never drift from the accounts. `PATCH /admin/public-models/{id}` writes the column and regenerates. A setting equal to the gateway default is dropped before storing, so a later default reaches models that never overrode it.
  - **`internal: true` is what keeps a direct document unreachable.** The gateway's `/v1/models` and `/v1/models/{id}` skip such a model, and `resolve_model` answers a request that names one exactly like an unknown model (same 404, same envelope, no attribution note). Without this a pattern-granted key (`claude-*`) matched `claude-fable-5#wj-cc` and could pin one upstream account. Routing, ensemble and semantic targets resolve by **id**, so marking them internal does not break dispatch.
  In the Dashboard's 模型与路由: routing rows are 对外 and open an editable routing form (settings plus per-target 移除该目标); direct rows are 内部直连, folded behind a checkbox, read-only with 移除该目标. Retiring the name is 公共模型's own 删除, which removes every account's mapping and then the documents.
- **Channel model**: one (public model, account) row, `channel_models`. Removing one account's mapping tombstones that account's direct document and rewrites the routing document — possibly to `targets: []`, which answers `503 upstream_unavailable` rather than deleting anything.
- **Hand-made models** (模型与路由 → 新增) are ordinary gateway documents, visible to pricing and grants; they are the only rows with `managed_by = null`.
- **Tenant**: status (`pending`/`active`/`suspended`), `billing_mode` (prepaid/postpaid, changeable in the Dashboard), `credit_limit_micro_cny`, `allowed_provider_groups`, `model_scope` (`all` = every public model including future ones; `selected` = the `allowed_model_ids` list). Keys stay closed while their group is absent from the tenant's list.
- **Money**: micro-CNY in `_cny` tables. `charge = ⌈Σ q_d × official USD rate_d × group cny_per_usd_e4 × (10000 − discount_bps) / 10¹⁴⌉` — one rounding, upward, everything inside it. `cost` is the same card × the serving account's cost rate; `margin = charge − cost`. `available = balance + credit_limit`; a tenant is exhausted when `available <= 0` (an i64 overflow counts as exhausted). The internal tenant (平台内部) is priced and posted but never marked.
- **A charge is written once.** A price mistake is fixed by a **re-pricing run** (`billing/repricing.rs`): it stores a `previewed` run with totals, and `apply` appends `pricing_adjustments_cny` rows plus ledger entries. It never edits `pricing_charges_cny`. **Known wart:** because the charge row keeps `status = 'unpriced'`, a repriced row still counts toward 计价状态's 最近 24 小时未计价 for the rest of its window, even though the money is correct. Fixing that means deciding what a corrected row's status should read; see the plan in §6.
- **Price library** (`pricing/library.rs` + `import.rs`): one stored copy of the public `pydantic/genai-prices` dataset. A preview compares every priceable model's library card to the baseline in force and says `create` / `update` / `same` / `flagged` / `unmatched` / `ambiguous`; POST writes the chosen items as `third_party` baselines under an operator's attestation. An entry whose only issue is a price field the rate card cannot meter (`unsupported_field`: storage searches, audio hours, video tokens, request counts…) is **not** blocked — the field is dropped and listed in `dropped`, which the review, the confirmation panel and the audit row all show. A field the card *does* meter but could not hold faithfully (`truncated_field`, more tiers than `MAX_TIERS_PER_FIELD`) does block, because billing the surviving bands would under-bill.
- **Service tiers**: a card carries `default`, plus `flex` / `priority` / `fast` / `batch` where the provider prices them. `ServiceTier::normalize` maps an absent tier, `default`, `standard` and OpenAI's `auto` to `default`; any other served name selects that tier and is refused as `missing_tier_price` if the card lacks it. `auto` is OpenAI's request-side default ("unless otherwise configured, the Project will use `default`"), which some OpenAI-compatible upstreams echo back as the served tier.
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
# ALWAYS set both ports: 55432 is the Postgres another session may own.
export CP_TEST_DIR=<scratch>/cp-test-x CP_TEST_PG_PORT=57xxx CP_TEST_KINE_PORT=57xxx
scripts/cp-test-services.sh start && eval "$(scripts/cp-test-services.sh env)"
cargo test -p aisix-control-plane -p aisix-pricing --all-features
scripts/cp-test-services.sh stop

# Gateway e2e (527 *.test.ts files under tests/e2e/src). It runs target/debug/aisix
# (AISIX_BIN overrides) and refuses a binary older than the sources ONLY when CI is
# unset (harness/app.ts:323 skips the check under CI), so rebuild it yourself.
cd tests/e2e && pnpm install --frozen-lockfile
cargo build -p aisix-server            # bin name `aisix`; build from the repo root so target/debug/aisix is fresh
E2E_THREAD_PER_CORE=true  pnpm test    # then =false for the work-stealing leg
# AISIX_E2E_REDIS_PLAIN / AISIX_E2E_ETCD_ENDPOINTS are needed by some cases; see ci.yml

# Web apps: unit, types, build, then Playwright against the real stack.
# stack.ts needs CP_E2E_PG_URL (a role that may CREATE DATABASE) and CP_E2E_KINE_BIN,
# the debug binaries above, and dist/ built for "/" (web-tenant WITHOUT TENANT_BASE).
cd web-cp && npm ci && npm run typecheck && npm run test:unit && npm run build
CP_E2E_PG_URL="postgres://postgres@127.0.0.1:57xxx/postgres" \
  CP_E2E_KINE_BIN=<scratch>/kine npx playwright test e2e/models.spec.ts
```

`crates/aisix-control-plane/CLAUDE.md` explains why the workspace run matters (feature unification turns on `aws-lc-rs` beside `ring`; a per-crate build hides it) and why a run without `CP_TEST_*` silently executes none of its integration tests. `tests/e2e/AGENTS.md` explains the readiness-gate and log-line rules those specs follow.

The Playwright stack is `web-cp/e2e/stack.ts`, which shells out to `scripts/cp-dev-stack.mjs` (Postgres, kine, the CP, a mock upstream, the gateway, `vite preview`). **Serialize e2e across worktrees by hand** — `flock` on `<kit>/platform/e2e.lock` is the convention the older lane scripts use, and nothing creates that file for you; two stacks at once produce port flakes that look like product bugs. Each spec file starts its own stack on real ports, so a run of several files takes minutes.

### Format, lint, audit

`cargo fmt --all -- --check`; `cargo clippy --workspace --all-features --all-targets -- -D warnings`; `cargo audit` (its ignores are justified inline in `.cargo/audit.toml`; RUSTSEC-2026-0459 for the lockfile-only `libsqlite3-sys` is one).

### Schemas and contracts

- **A gateway resource field** starts in `crates/aisix-core/src/models/`, then `cargo run -p aisix-core --bin dump-schema` regenerates `schemas/resources/` and `schemas/resources-lenient/` (commit them), and `cargo test -p aisix-core --test resource_schema_characterization`. The strict set is the write path; the lenient set is what the gateway loads. Prove a new required field is required only in the strict set.
- **A CP-only field** (a tenant column, an `/admin` or `/api` field) has no schema entry: it needs a migration, the handler, a `ROUTE_ROLES` row, and — if `/api` answers change shape — an `API_CONTRACT` bump mirrored in `web-cp/src/lib/contract.ts` (and the portal's copy).

### Deploy to routio.ai

```bash
KIT=/home/ubuntu/code/aisix/.claude/worktrees/merge-upstream/.superpowers/platform/deploy/routio
$KIT/build-release.sh <sha>          # tarball in $KIT/releases/
# edit gp-upgrade.sh in place (keep a .bak-<sha>): NEW/NEW_SHA/PREV/PREV_SHA/ROLLBACK/
# APPLIED/NEW_MIGS/NEW_MIG_FILES/DBID/ENV_ID, and ignore its stale header prose
$KIT/gp-upgrade.sh preflight         # read-only: fork main, tarball, host state, CSP hashes, data pre-check
$KIT/gp-upgrade.sh run               # dump → install → CP+apps → gateway → every check
$KIT/gp-upgrade.sh checks            # re-run the checks only
$KIT/gp-upgrade.sh rollback <gateway|cp|all>
```

`run` takes a Postgres dump to `/var/backups/routio/pg/`, switches the control plane and both apps together (they share `api_contract`), then the gateway. It writes its own lines to `deploy-log.txt`. The hourly host check is paused for the switch and resumed by a trap; **a run killed mid-switch leaves the CP and apps on the new release and the gateway on the old one** — resume the timer by hand and finish or roll back.

Checks that must pass: `check-host` (17), liveness (4), `check-edge` (25), `check-csp` (5), `signup_open` (1), HTTP/3 (4). `all checks passed` is the only acceptable ending.

**Data-loss checklist for an upgrade:** the dump exists and restores (`pg_restore --list`); `database_id` is unchanged; migrations are monotonic and every checksum matches the release files; kine's uptime is unbroken; the rollback release is still on the host. A full restore is a separate, owner-approved operation — the script never runs it.

### Rules that have bitten us

- **One writer per thing.** Gateway documents: only `write_resource`. Marks: only `marks::project`. kine: only the projector leader. Bypassing any still answers 200, and the gap shows days later.
- **Lock order**: tenant row → billing account → group / public-model row → `RESOURCES_LOCK_KEY`. Anything that moves money or decides a mark takes `lock_billing` first. `tests/billing_marks.rs` and `tests/tenants.rs` census the paths; a new one adds a case.
- **A migration number is taken once.** Three branches in one batch each grabbed `0032`; merge serially and renumber the later ones. Migrations are additive; a projected field's shape or meaning never changes in place.
- **Census tests are load-bearing**: `ROUTE_ROLES`, `guardrail_coverage.rs`, `metric-endpoint-parity-e2e.test.ts`, `endpoint_acl.rs`'s family census, `tokenParity.test.ts`, `ui_contracts_match`. A route, endpoint or shared UI file added without them fails a test you did not run.
- **An e2e that passes on the broken code is not a test.** For a money or permission rule, break the code and watch the test go red. Two recent examples of this paying off: the `auto` tier test, and the models-page spec that proved the old delete button was there before the retire path replaced it.
- **A UI change that reorders or filters a list breaks specs you did not think of.** The first cut of the model list defaulted to 对外 only and hid hand-made models; nine specs caught it. When a default changes what is visible, run the whole spec file, not just the new case.
- **The SSRF guard is real** in the CP's outbound calls; tests reach a loopback stand-in only through a library-level seam (`TestCp::start_reaching_loopback`, `start_with_sources`), never an env var, and there is deliberately no build-time bypass (#80).
- **Flaky-looking failures are usually the environment**: a locked table under `lock_timeout` (#81), a `datetime-local` input that drops `:00`, a port another worktree holds, a `playwright-core` resolved from a deleted worktree. Reproduce before "fixing" a product.
- **A field added to a generated document is not live until every existing one is regenerated.** `channels::direct_doc` writes `internal: true` only when a document is written, so after deploying that marker 71 of 71 production documents still lacked it and the leak was still open. The fix shipped, the behaviour did not. Any change to a generated shape needs a re-emit step: `PATCH /admin/public-models/{id}` with the model's stored `routing` regenerates its whole family, and 26 of them covered all 71 documents. Check the live rows after deploying, not only that `cp_version` moved.
- **A setting the operator edits must outlive every automatic rewrite.** A generated routing document is rewritten from `channel_models` on every account change, so a strategy written into the document would vanish on the next tick. Settings live in the public model's own column and the generator merges them. When a surface offers an edit to generated data, name what regenerates it and prove survival with a test that changes the account afterwards.
- **Parallel agents in worktrees share more than they look like they do.** `git stash` is one stack for every worktree of the repo: two agents popped each other's work (round of #82) — never use it; set work aside with a patch file or a WIP commit. A "read-only" auditor that runs `git checkout --detach` in the integration worktree detaches it, and the next merges land off the branch — brief auditors to read other revisions with `git show`/`git diff` only. Test service ports in the 32768–60999 range are Linux's ephemeral range and can be taken by an outbound connection at the moment kine binds; give each lane ports below 32768.
- **Run the cross-plane suite before merging a billing change.** `tests/e2e` with `-c vitest.cp.config.ts` (CI job "e2e control plane") is not part of `pnpm test`, and nobody ran it locally after dbdcd1f0: it stayed red on `main` for a day, as did a `models-discovery` case with a wrong expectation and `cargo fmt`. CI on `main` was red at every push of that day — read the CI result after pushing `main`, not only the deploy checks.
- **Verifying a fix on production means driving the real page.** Two of this round's bugs were invisible from the API and the database: the row actions were rendered by a bundle nobody had re-read, and a key's `model_ids` held ids the catalogue had already deleted. A Playwright probe against `admin.routio.ai` with the operator account found both, and confirmed each fix.

---

## 5. Current state (2026-11-10)

**Live on routio.ai** (`dev+sha-5b10cf2`, migrations through `0039`): signup (rate-limited, open), tenant approval with model scope, groups and per-model rates, public models and channel mapping including bulk fetch from an upstream account, baselines plus an imported price library with one-click attested sync (and per-model candidate choice for ambiguous matches), cost rate per account, the CNY ledger with top-ups / credits / quotas / re-pricing, the tenant portal, API key plaintext reveal at any time (`sk-rt-…`), the Dashboard's model list split into 对外 / 内部直连 with the routing settings editable there, a 公共模型 删除 that clears every account's mapping with the model, and (PR #82) 恢复 for a retired public model, reachable tenant/key counts and a priced state judged on the groups that serve the model, per-reason 计价状态 details with re-priced rows counted apart, 计费明细 reasons and deleted-model names, tiered/long-context manual baselines, per-1,000 web-search display, plus the 99 retained items of the fourth-round review (`REVIEW-2026-11-10*.md`, kept outside the repo).

**Done in the platform plan**: P0 (gateway), P1 (control plane + Dashboard), P2 (pricing and billing), P3 (deploy), the group-pricing phase, the P4 dependency and quota-reduction batch, the P5 batch (cost rate, price library, key reveal, Routio branding, credit limit in both modes), and the post-P5 work: ambiguous library match resolution; the library flag split with `dropped` (`0036`); the catalogue's Fable 5.2 entry; `auto` billed at default; the model page's 对外 / 内部直连 split, retire and per-account removal; public-model routing settings (`0037`); the `internal` marker that hides generated documents from callers; and the Keys table no longer marking granted models as ungranted (nor printing a uuid for a deleted one).

**Open issues** are on the fork's tracker (`kilb/aisix`); read their live state rather than this list. #83–#88 hold what #82's review and audit left (LOW items, R-056, passthrough streamed argument deltas, charges filters). The ones that matter for planning are summarised in §6.

---

## 6. Development plan — what to do next

Ordered by what blocks a product, then by cost. Each item states the outcome, the touch points, and how to know it is done. Nothing here is a commitment made to the owner; the owner's priorities override it.

### P6 — the money surface, honestly

0. **Decide whether `/internal` needs a `compat::FIELD_FLOORS` row.** It shipped without one, deliberately: an older gateway ignores the marker and lists and serves those documents (the fail-open shape the rule names), but generation is a create-or-replace inside `channels::regenerate`, so a `Since::ThisRelease` row would answer `422 gateway_too_old` to every channel edit for the whole fleet window — blocking the very operation that fixes the exposure. Today's state is the status quo, not a new hole. Revisit only if a fleet ever runs mixed versions.

1. ~~**A corrected row should stop reading as unpriced.**~~ Done in #82: 计价状态 counts a charge with an applied adjustment apart (`corrected_last_24h`). The original note: after a re-pricing run reprices an unpriced charge, 计价状态's 最近 24 小时未计价 keeps counting it for the remaining window, because `status` on `pricing_charges_cny` is written once. Decide the representation (a `pricing_adjustments_cny`-aware read in `unpriced_by_code`, or a status the correction moves the row to via a new column), implement it, and pin it with a test that fails if the correction is invisible. Touch: `pricing/store.rs`, `billing/repricing.rs`, `pricing/api.rs`, `web-cp/src/tabs/pricing/PricingStatus.tsx`, `web-cp/src/lib/pricingStatus.ts`.
2. **A model created in the Dashboard should prompt for a price** (#40) and the form should edit `cost` including cache rates (#8). Today a model can be reachable and unpriced, and the first sign is an unpriced-charge count.
3. **The catalogue's own gaps** (#55, #46): notes for charges two provider pages publish, and a marker for tiers published without long-context rows. Then a periodic refresh procedure (a documented date, or a small admin action) so `list-prices.json` stops being the thing nobody updates.

### P7 — the remaining platform surfaces

4. **Adapt the second-phase endpoints for platform mode** (#26): completions, rerank, passthrough, media, MCP, A2A; then the specific gaps in #27 (realtime + speech voices), #28 (files, batches, fine-tuning tenant binding), #29 (stored-object checks), #30 (forwarded workspace/profile headers), #58 (realtime re-hold), #59, #60, #57, #37 (running sessions vs. a narrowed `allowed_endpoints`).
   These are the largest remaining functional holes: a tenant can call those endpoints and the platform's isolation and billing rules do not all apply. Each endpoint family is one plan in `platform/design/`, one PR, and its own e2e across the handler family (see the "Handler Families Stay in Lockstep" rule in `CLAUDE.md`).
5. **Online payment.** Today top-ups are manual: a tenant files a request, an operator approves it, `topups::approve` posts the ledger entry. A payment provider integration is a product decision first (which provider, which currency, refunds) and a code change second.
6. **Tenant and operator scale limits** (#31 pagination, #32 per-tenant key/invite caps, #36 delete reference checks under the global lock, #35 unknown resource kinds on an older CP). None of these bite at 3 tenants; all of them bite at 300.

### P8 — correctness debt, in the order the code says it matters

7. **Pricing correctness**: #56 (the book loads every revision ever written — a growth problem with a time dimension), #52 (a malformed `pricing_version` fails every price write), #51 (backdating checks the transaction start), #49 (price instant may lead a revision by up to 5 minutes), #54 (one INSERT per charge), #53 (two unused indexes), #45 (`reasoning_beside_completion` identity), #48 (a `/v1/messages` cache-hit row lacks fields a chat row carries), #44 (an abandoned stream bills prompt tokens only), #43 (time-of-day pricing for peak/off-peak providers), #42 (cache retention in the usage event), #65 (a re-priced row records no cost).
8. **Gateway correctness**: #22 (truncated last frame + closing error), #24 (wildcard model upstream attribution), #21 (invisible provider-group refusals), #16 (name-form reference to a wildcard-served model), #19 (refusal without `error.code`), #11, #13, #17.
9. **Test and process debt**: #23 (two log-capture tests fail alone), #50 (token-mint port races), #71 (split `PricingImport.tsx`), #72, #73, #74, #80, #78.

### How to run a task from this list

1. Read the issue and the code it names; if the issue is customer-facing, scope it from the quoted words, not from the proposed solution.
2. Write the plan into `platform/design/` as `YYYY-MM-DD-<topic>-plan.md` if it is more than a day's work; otherwise a one-paragraph plan in the task message is enough.
3. Build in a worktree, test the crates and specs the change reaches, then run `npm run typecheck` and the affected Playwright files.
4. If the change touches money arithmetic, tenant isolation, auth, or the gateway request path, run the workspace test suite and both gateway e2e modes, and get an independent audit agent to read the diff.
5. Commit (English message, `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` if a Claude agent authored it), push the branch, fast-forward `main`, build the release, retarget the kit, `preflight`, `run`, and confirm `all checks passed`.
6. Write the behaviour change into the PR body or the commit message: what an existing deployment experiences after upgrading, and whether anything must be edited.

---

## 7. Things a new agent will trip on

1. **No user-facing prose in the repo.** User documentation lives in `api7/docs`. `docs/design/` holds two historical gateway design notes, kept deliberately and marked as such; do not add pages there or anywhere else under `docs/`.
2. **The kit is not in the repo** (`.superpowers/` is gitignored), so a fresh clone cannot deploy or run the UI tests. It exists **only** in the `merge-upstream` worktree (`<repo>/.superpowers/` holds an empty stub); a `p5-fixes` or `p3-ship-*` worktree has none, so reach for the absolute path in §2.
3. **The fork tracks upstream.** Gateway crates are upstream's; expect periodic merges from `upstream`. `aisix-control-plane`, `aisix-pricing` and both web apps are ours.
4. **Worktrees.** Development happens in `git worktree`s; each has its own `target/` (minutes of compile). Reuse one worktree for a run of small changes, and stop the services you start. The `p3-ship-*` worktrees are detached checkouts `build-release.sh` creates; deleting them after a deploy is safe and reclaims about 3 GB each.
5. **Ports.** Never touch a Postgres on 55432/55532 that you did not start; `cp-test-services.sh` defaults to 55432, so always override it. Serialize e2e across worktrees with the shared `flock`.
6. **Secrets.** Never print an environment dump. Credentials are 0600 files: on the host `/root/routio/operator.{email,pass}` and `/etc/aisix/*`; locally the kit's `secrets/`. Never put a secret on a command line, in a log, an audit row or a commit. Every agent brief must forbid printing the environment, because the session carries a `GH_TOKEN`.
7. **The operator account** is `ops-…@routio.ai` (the deploy log records the current one); its password is in `secrets/operator.pass`. There is no admin/admin.
8. **`/api/*` and `/admin/*` are different contracts**: `/api` is tenant-facing and whitelisted field by field; `/admin` is operator-only. Never let a field of one leak into the other.
9. **Browser UI changes need Playwright against the real stack** — no stubbed network. Build the debug binaries and `dist/` first (see §4). A UI change in `web-cp/src/ui/` must be copied byte-for-byte to `web-tenant/src/ui/` or `tokenParity.test.ts` fails.
10. **A deployment is not done until the checks pass** — `check-host`, liveness, `check-edge`, `check-csp`, `signup_open`, HTTP/3 — and `deploy-log.txt` has the entry.
11. **The library is re-parsed on every read.** A deploy that changes how a dataset entry parses changes the preview immediately, with no re-fetch; a deploy that changes the shape of a stored card does not rewrite baselines. When a pricing fix lands, check whether existing unpriced rows need a re-pricing run.
12. **`gp-upgrade.sh` is single-use.** Its constants describe one upgrade; `preflight` refuses anything else. Edit it in place between releases (keeping a `.bak`), never copy it out of `deploy/routio/`, and read the constants rather than the header comment. A release with a migration also needs `NEW_MIG_FILES`, `APPLIED`/`NEW_MIGS` and the "new objects already present" query retargeted at the new object; a release without one leaves `NEW_MIG_FILES=""` and the gate skips itself.
13. **An interrupted `run` leaves a real state.** The switch installs the release and repoints `cp`, `gateway`, `admin` and `portal` before it verifies them. A run killed mid-way leaves the host on the new release with the timer paused; `preflight` then refuses because the links no longer match `PREV`. Read `readlink` on the four links, run `gp-upgrade.sh checks`, and resume the timer — do not roll back reflexively.

---

## 8. Where to start, by task

- **"Add a field the operator edits"** (CP-only): migration → the row and `tenants::update` (or the matching handler) with its `fields` audit entry → a `ROUTE_ROLES` row → `web-cp` form and, if tenants see it, a whitelisted `/api` route with an `API_CONTRACT` bump and both `contract.ts` copies → tests. A CP field is **not** an `aisix-core` model.
- **"Add a field to a gateway resource"**: model in `aisix-core/src/models/`, `dump-schema`, CP validation and projection, Dashboard form, a `compat::FIELD_FLOORS` row if an older gateway would misread it, and the lenient-load test.
- **"Change pricing"**: `crates/aisix-pricing/AGENTS.md` first (the formula, one rounding, the discount inside the same ceiling), then the control plane's pricing module; keep `equivalence.rs`/`golden.rs` green and prove the money test by mutation. A charge is immutable — a correction is a re-pricing run.
- **"Change what a dataset entry imports as"**: `pricing/import.rs`'s module doc, `dimension()` for the field table, `Flag::blocks_sync()` for whether it may sync anyway, then `library.rs`'s `state_of` and the preview UI. Add a dataset fixture to the import tests for each new field shape.
- **"Change what tenants can reach"**: `tenants.rs` (`model_scope`, grants), `keys.rs` (key documents), `channels.rs` (generated documents), then the `/api` views and both web apps; check the tenant views stay whitelisted.
- **"Change the model page"**: `web-cp/src/tabs/Models.tsx` and `lib/modelSource.ts` for classification; `web-cp/src/tabs/providers/ChannelModels.tsx` for per-account actions; a generated row's only removal is `publicModels.retire`, and `lib/retireOutcome.ts` exists because that write can half-succeed.
- **"Change the gateway's request path"**: read `crates/aisix-proxy/AGENTS.md` in full first; run both gateway e2e modes.
- **"Deploy"**: build the tarball, retarget `gp-upgrade.sh` in place, `run`, keep the previous release, check `deploy-log.txt`.

When something is ambiguous, the design documents in `platform/design/` and the ledgers named in §2 usually answer it, with the reasoning and the date.
