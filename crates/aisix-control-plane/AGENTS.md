# aisix-control-plane

The control plane is kine's only writer, and Postgres is its source of truth.
Most of what can go wrong here does not error: a write still answers 200 and a
gateway keeps serving, and the gap shows only in what a tenant or an older
gateway can now reach. The rules below are the ones a change can break that
way.

## Verify with one workspace-wide test run

- **Run `cargo test --workspace --all-features` once, with the control-plane
  services up (`scripts/cp-test-services.sh start`, then `eval` its `env`), not
  only `-p aisix-control-plane`.** A per-crate build never unifies features
  with the gateway crates. A workspace build turns on rustls's `aws-lc-rs`
  beside `ring`, and from then on a TLS client that takes the process default
  provider panics with "no process-level CryptoProvider". `start` installs
  `ring` as the process default for that reason. A new entry point that builds
  such a client without going through `start` (a subcommand, a test harness)
  installs it too.
- Locally, a test returns early when `CP_TEST_*` is unset, so a green run
  without the services ran none of this crate's integration tests. On CI the
  same test panics instead.

## Gateway documents go only through `write_resource`

- **Every create, replace and delete of a `gw_resources` row goes through
  `resources::write::write_resource`**: the operators' generic API, and every
  document derived from tenants, members and keys (the tenant's team, a
  member's user, a key, a tenant's limits policy). It is the only place that
  normalises aliases, restores redacted credentials, validates with
  aisix-core's strict schema and semantic checks, checks references, runs the
  gateway-version gate, seals credentials and assigns `seq`. A direct
  `INSERT` or `UPDATE` skips all of that, and the projector ships the row
  anyway. `only_the_resources_module_writes_gw_resources` (`tests/boot.rs`)
  fails on one outside `src/resources/`, except the two that touch no
  document: the projector's projection columns and housekeeping's tombstone
  prune.
- Within one transaction, write referents before referrers, and delete
  referrers before referents. Each write checks its own references.
- Every builder of a derived document has a test that loads what it builds on
  the lenient schema (`schemas/resources-lenient/`, the
  `every_*_document_loads_on_the_lenient_schema` tests). A new builder adds
  one.

## Lock order: tenant rows, then billing accounts, then `RESOURCES_LOCK_KEY`

- **A transaction that touches a tenant and a gateway document takes the tenant
  first**: `tenants::lock`, or `tenants::lock_many` (ascending id) for several,
  and only then `write_resource`, which takes `RESOURCES_LOCK_KEY`. Nothing
  takes a tenant lock while holding the resources lock. An `INSERT` of a row
  that references a tenant takes a key-share lock on that tenant's row, so a
  path that inserts one counts as touching the tenant and locks it first.
  Operator-wide invariants take `OPERATORS_LOCK_KEY` and touch no tenant.
  `every_tenant_write_takes_the_tenant_before_the_resources_lock`
  (`tests/tenants.rs`) sends each path that writes a tenant's documents while
  the tenant is held elsewhere and fails if it already holds the resources
  lock; a new such path adds a case there.
- **A path that moves money or decides a mark takes the tenant's billing
  account right after its row**: `tenants::lock_billing`, as the first lock of
  its transaction. That is every ledger posting, credit-limit or quota change,
  and every `tenants::project`, whose billing step writes the balance mark
  (`marks::project`). An account is never taken after the resources lock, so
  a path that locks its tenant twice moves only the first lock to
  `lock_billing`. Every other tenant edit keeps `tenants::lock`, which leaves
  the account to usage intake. Intake locks accounts only, in ascending tenant
  id, never waits on a tenant row and never takes the resources lock: it flags
  a crossing, and the settler (`billing::settler`) writes the mark after
  commit. Intake updates each account once, flag included
  (`ledger::post_flagging`): a second update of a row in one transaction has
  the foreign key's check key-share its tenant row, which a held tenant would
  make intake wait on. So every flag is decided before the posting, the
  balance's (`marks::crossings`) and the quotas' (`budgets::count_charges`,
  which writes only spend rows);
  `a_crossing_batch_never_waits_on_a_held_tenant_row` and
  `a_batch_that_crosses_only_a_quota_never_waits_on_a_held_tenant_row` pin
  it. The
  account lock is `FOR NO KEY UPDATE` (`ledger::lock_account`), so a row that
  references an account, such as a top-up request, is inserted without
  waiting on intake (`a_topup_request_never_waits_on_intakes_account_lock`).
  `every_marking_path_takes_the_account_before_the_resources_lock`
  (`tests/billing_marks.rs`) sends each such path while the account is held
  elsewhere; a new one adds a case there.
- **The settler is the one exception to the account-before-resources order,
  and never waits while it is out of order.** It takes each tenant row
  `NOWAIT`; then the account and the resources lock only if that is free.
  Once a tenant's settle has found the resources lock busy for 10 s, its
  attempts queue for the resources lock instead (for at most 2 s each,
  holding only the tenant row) and take the account after it, `NOWAIT`. The
  attempt that first queues logs a WARN naming the tenant and how long it had
  been skipped, repeated at most once a minute while the tenant stays
  unsettled. A lock taken without waiting is in no wait cycle, and no settle
  holds an account while queued, so intake never waits behind the resources
  lock (`an_escalated_settle_never_holds_up_intake`,
  `an_escalated_settle_never_waits_for_the_account_while_it_holds_the_resources_lock`,
  `a_crossing_batch_never_waits_on_a_held_tenant_row`).
- **Advisory lock keys must be unique.** They are the `*_LOCK_KEY` constants,
  all `0x6169_7369_7863_70xx`: operators `7030`, projector `7031`, resources
  `7032`, boot `7033`, signup `7073`. Postgres has one key space per database.
  A reused value serialises unrelated work, and a value shared with the
  projector's session-level lock makes a writer wait out the leader's whole
  tenure (then `lock_timeout`, then `503`). No test checks this; grep
  `0x6169_7369_7863_70` before adding a key.

## A field older gateways would misread gets a `FIELD_FLOORS` row

- A gateway loads documents leniently and ignores a field it does not know.
  **When that absence is fail-open** (the field restricts: an allow-list, a
  scope, a switch that closes something), the field gets a row in
  `compat::FIELD_FLOORS`, with `Since::ThisRelease` until the release that
  ships it is tagged (`RELEASING.md`). A new value of a field that older
  gateways already know (an enum value, an endpoint family) gets a row keyed
  on that value (`Trigger::Equals`, `Trigger::Contains`).
- **Each row's `widens(stored, doc)` decides which replaces are judged
  again.** A replace that keeps a floored field is judged only when `widens`
  says the older gateway, which ignores the field, would now serve more: a key
  switched back on, living longer, or granted another model, route or
  endpoint family. Keep narrowing out of it. Disables, revocations and
  catch-up re-projections run during every mixed-version window, and refusing
  them would block revocation.
- **The gate fails closed.** A gateway version that cannot be read refuses, a
  credential that hit its instance cap within the window refuses, and a
  control plane whose own stamp cannot be read does not boot. With no gateway
  registered, the write passes. Do not add a path on which the gate judges
  nothing without saying so.
- A caller that can meet the refusal during an upgrade (boot, the key catch-up,
  the tenant routes) tests it with `compat::refused` and defers. On `/api` it
  answers a neutral `503 unavailable`, never a `422` that tells a tenant about
  the platform's gateways.

## One projector writes, fenced, in `seq` order, for an environment it owns

- **Only the leader writes kine**: the instance whose own connection holds
  `PROJECTOR_LOCK_KEY` as a session lock. It checks that it still holds the
  lock before every kine write, retries included. A new kine write goes
  through the projector and that check. An admin action that writes kine
  (reconcile) is handed to the leader through `cp_settings` and never written
  beside it.
- **Rows go in ascending `seq` and a pass never skips one.** `seq` is commit
  order because `write_resource` holds the resources lock. A pass stops at the
  first row it cannot project. That keeps "the key written last is live" meaning
  "everything before it is live", which the e2e readiness gate relies on.
- **One database owns an environment's prefix.** The claim
  `<prefix>/_cp/<env_id>/owner` names it. The leader re-reads the claim on
  every pass and before each batch, and stops writing when it is not the
  owner. Ownership moves only by the audited `POST /admin/rollout/ownership`
  takeover.
- **Reconcile never mass-deletes unasked.** It deletes stray keys on its own
  only when they are at most `max(10, 5%)` of the prefix's keys and the
  database holds live rows. Otherwise it deletes none, reports
  `reconcile_blocked`, and waits for an admin's `allow_deletes`. Documents the
  boot itself writes (the internal tenant's) are not live rows for this.

## Key documents follow `KEY_DOC_VERSION` and their owners' state

- **A release that changes what `keys::build_key_doc` writes raises
  `KEY_DOC_VERSION`.** A key's document is rewritten only when its own state
  changes, or by `keys::catch_up` when its `tenant_keys.doc_version` is behind.
  Without the bump, existing keys keep the old shape for good.
- **The catch-up stays per tenant and per row.** It runs at boot and on every
  housekeeping tick, one transaction per tenant (its lock, then the resources
  lock). A global flag, or one transaction over every tenant, holds the
  resources lock across the fleet (`503` on every write, a peer's boot
  crash-looping) and misses the rows an older replica writes during a rolling
  upgrade. Every writer of a key document stamps `doc_version` after a
  successful write.
- **A change to a member or a tenant re-projects the affected keys in the same
  transaction.** That covers disable, removal, a role change (a billing member
  holds no working key), suspend, reactivate, a grant change and a model delete.
  A tenant's restrictions bracket what they guard: its limits policy is written
  before its keys open and retired after they close.

## Tenancy on `/api/*`

- **Every id in a path or a body is resolved with `tenant_id =
  MemberAuth.tenant_id` first**, and another tenant's id behaves exactly like
  a random UUID: `404 not_found` in the path, `422` at the field in the body.
  That lookup runs before the `If-Match` checks, so `428` and `412` answer only
  for the caller's own rows.
- No `/api/*` answer carries a provider, a provider key, a group name or an
  upstream model id.
- **A pending tenant projects nothing**: no team, no member `users`
  document and no key until an operator approves it, since signup is
  anonymous. The approval writes the team and every live member's user ahead
  of the limits policy and the keys.
- Every mutating browser request is JSON, bodyless ones included
  (`http::json_only`, `415` otherwise): with `SameSite=Strict` cookies that
  is the CSRF defence, so no route may be exempted from it.

## Credentials are sealed and normalised, and never logged or audited

- Every slot `aisix_core::credentials::visit_credentials` finds is stored
  sealed (`aisixsealed.v1.`) and unsealed only by the projector, into the value
  sent to kine. Reads replace every slot with `[REDACTED]`.
- **Every former spelling the strict schema accepts is rewritten to its
  canonical name before validation**, so sealing and redaction key off the
  schema's own alias list and never off one hard-coded name
  (`every_credential_alias_the_schema_accepts_is_normalised`).
- A redacted secret sent back is kept only while every destination field of
  its kind is unchanged (`resources::secrets`). A new credential kind names its
  destinations; the match is exhaustive.
- **Audit rows are written in the change's transaction**, and `detail` holds
  field names and non-secret values only. No document, credential, token,
  private key or `PLATFORM_ID_KEY` value reaches a log line or an audit row,
  and `CP_DATABASE_URL` / `CP_KINE_ENDPOINT` are displayed only through
  `aisix_core::redact_url_for_log`.

## Postgres rules

- Placeholders are `$n`. An integer decoded as `i64` is `bigint`, a flag
  `boolean`, a time `timestamptz`.
- Every `SUM` is cast `::bigint` (`the_crate_has_no_uncast_sum`).
- Every `ON CONFLICT … DO UPDATE` uses `EXCLUDED.` and table-qualified columns.
- A unique violation is recognised by SQLSTATE `23505` plus the constraint
  name, never by message text.
- A read-then-write invariant locks its parent row (`… FOR UPDATE`) or takes a
  transaction-scoped advisory lock. READ COMMITTED alone loses updates, and
  there is no `BEGIN IMMEDIATE`.
- Queries are runtime `sqlx::query` / `query_as`, never the `query!` macros.
  Migrations are additive, one file per change, and never edited once shipped.

## Defaults fail closed

- A customer tenant names at least one provider group: `[]` is refused on save,
  and a key of a tenant found that way stays disabled. A customer key always
  writes `allowed_model_ids` (`[]` grants none, where an absent list would fall
  back to names) and `allowed_endpoints`.
- A value that cannot be read closes: an unreadable gateway version, an
  unknown endpoint family on the gateway, a stored tenant default with no
  groups.
- Every route has a `ROUTE_ROLES` row. The census fails a route without one,
  and `tests/roles.rs` sends each mutating route as the role just below its
  minimum; a new route adds its fixture there.

## A bundle never carries etcd credentials

`gateways::bundle_env` never sets `AISIX_ETCD__USER` or
`AISIX_ETCD__PASSWORD_ENV`. kine has no Auth service, and a gateway that tries
to log in treats the `Unimplemented` answer as a fatal boot error.
