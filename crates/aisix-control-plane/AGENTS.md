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

## Lock order: tenant rows, billing accounts, group rows, then `RESOURCES_LOCK_KEY`

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
  its transaction, or `tenants::lock_many_billing` (each row, then its
  account, in ascending id) for several, as a re-pricing apply does. That is
  every ledger posting, credit-limit or quota change, and every tenant
  projection that runs its billing step: `tenants::project`, whose billing
  step writes the balance and quota marks (`marks::project`), called by a
  tenant's creation, its approval, suspension and reactivation, and a change
  of its billing mode. A tenant edit that moves no money and decides no mark
  (a rename, a grant, its limits or its groups) and a model delete, which
  withdraws the model from every tenant holding it, project with
  `tenants::project_without_marks`, which skips the billing step, and take
  the tenant's row alone (`no_edit_that_moves_no_money_waits_on_a_held_account`,
  `a_model_delete_never_makes_intake_wait`). An account is never taken after
  the resources lock, so a path that locks its tenant twice moves only the
  first lock to `lock_billing`. Every other tenant edit keeps `tenants::lock`,
  which leaves the account to usage intake. Intake locks accounts only, in
  ascending tenant id, never waits on a tenant row and never takes the
  resources lock: it flags a crossing, and the settler (`billing::settler`)
  writes the mark after commit. Intake updates each account once, flag included
  (`ledger::post_flagging`): a second update of a row in one transaction has
  the foreign key's check key-share its tenant row, which a held tenant would
  make intake wait on. So every flag is decided before the posting, the
  balance's (`marks::crossings`) and the quotas' (`budgets::count_charges`,
  which writes only spend rows);
  `a_crossing_batch_never_waits_on_a_held_tenant_row` and
  `a_batch_that_crosses_only_a_quota_never_waits_on_a_held_tenant_row` pin
  it. The account lock is `FOR NO KEY UPDATE` (`ledger::lock_account`), so a
  row that references an account, such as a top-up request, is inserted
  without waiting on intake
  (`a_topup_request_never_waits_on_intakes_account_lock`). A quota row is
  locked only by the writes in `billing::budgets`, after the billing lock:
  intake's spend upsert key-shares it while holding the account, so a lock
  on a quota row taken before the account would deadlock with intake.
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
- **A provider group's row sits between the billing account and
  `RESOURCES_LOCK_KEY`, and a public model's row beside it there.** A path
  that names a group takes its row `FOR SHARE` — `groups::lock_one`,
  `groups::lock_named` (ascending id) and `groups::key_group`, which every
  key create and every key `group_id` change goes through — and does so
  before `write_resource`, as `keys::create_key` does. A path that changes or
  retires one takes it `FOR UPDATE` (`LockMode::Update`): `retire`,
  `make-default` and the rate writes. A retire holds the group's row **and**
  `RESOURCES_LOCK_KEY` across its reference count and its update, so a
  provider-key document cannot name a group that is about to be retired: a
  write that gets the resources lock second is refused at `/groups`, and one
  that gets it first has committed before the count reads it
  (`a_provider_key_write_and_a_retire_never_end_with_a_retired_group_named`,
  `a_group_edit_takes_its_groups_before_the_resources_lock`,
  `a_provider_key_write_takes_its_groups_before_the_resources_lock`,
  `waits_on_the_group_not_the_resources_lock`, `tests/groups.rs`).
  A channel edit takes the public model's row `FOR UPDATE` before
  `write_resource` (`channels::lock_public`), and its retire takes that row
  and then `refs::lock_resources` itself, because its reference counts are
  plain `SELECT`s (`a_retire_waits_for_the_resources_lock`,
  `a_grant_that_commits_first_makes_the_retire_refuse`,
  `tests/channel_models.rs`). Never take a group or public-model row after
  the resources lock; a path that needs both takes the row first.
- **A generated document's owner is `public:<public_model_id>`, and only
  `src/channels.rs` writes one.** The direct and routing documents a public
  model implies go through `write_resource` like any other, with
  `managed_by: Some(&channels::owner(id))`, so a hand edit answers
  `409 managed_resource` and a regeneration cannot silently overwrite one.
  `only_the_resources_module_writes_gw_resources` (`tests/boot.rs`) enforces
  the write half.
- **The upstream-model route is the control plane's only outbound call: it
  is operator-only, bounded, scrubbed and writes nothing.**
  `GET /admin/provider-keys/{id}/upstream-models` (`src/upstream_models.rs`)
  reaches the account's own configured host on that account's own credential,
  through the SSRF guard, with a 5 s timeout and a 1 MiB body cap behind a
  60 s cache keyed on the account id **and its document version**, so editing
  the account misses at once. One call per account is in flight; a second
  request waits for it and shares the answer. It follows no redirect and
  writes no row, document or audit entry — the list feeds a picker, and a
  value it offers is stored only when the operator saves it. Its errors go
  through the shared scrubber, so neither the credential nor the operator's
  infrastructure appears in an answer or a log line.
- **Advisory lock keys must be unique.** They are the `*_LOCK_KEY` constants,
  all `0x6169_7369_7863_70xx`: operators `7030`, projector `7031`, resources
  `7032`, boot `7033`, signup `7073`, and the test harness's template lock
  `7074` (`tests/support/mod.rs`). Postgres has one key space per database.
  A reused value serialises unrelated work, and a value shared with the
  projector's session-level lock makes a writer wait out the leader's whole
  tenure (then `lock_timeout`, then `503`). No test checks this; grep
  `0x6169_7369_7863_70` before adding a key.

## Billing: the ledger, the marks and the quotas

Money fails the same quiet way: a wrong entry still answers 200, and shows
only in reconciliation (`GET /admin/billing/reconciliation`) or on a tenant's
statement.

- **Money in the database is micro-CNY, in the `_cny` tables, and the USD
  tables take no new rows.** `pricing_charges_cny`, `billing_ledger_cny`,
  `billing_topups_cny`, `billing_repricing_runs_cny`, `pricing_adjustments_cny`
  and the `*_micro_cny` columns on `billing_accounts` and `billing_budgets`
  are the money this release reads and writes (`0027`, `0028`). The tables
  they replace still exist, holding the rows a USD database had, and a
  `BEFORE INSERT` trigger on each refuses every new row (`billing_usd_fenced`,
  `0027`): an older replica's intake batch therefore fails and the gateway
  sends it again, which is the fence doing its job, not a bug to route
  around. Nothing converts between the two, anywhere: there is no exchange
  rate in this platform, and `cost_micro_usd` is the one USD figure left,
  because an upstream's cost is in the upstream's own currency. A migration
  that drops a fence, or a query that writes a USD table, is what
  `the_fences_refuse_every_usd_money_table_and_the_discount_still_inserts`
  and `the_usd_quota_tables_take_no_new_rows` (`tests/billing_migration.rs`)
  catch.
- **`0027`'s guard refuses a database that still holds customer USD money,
  at migration time, by name and by count.** It counts customer tenants with
  ledger entries, pending top-ups, live quotas and a credit limit, and raises
  rather than convert, move or delete a single amount. The upgrade path for
  such a database is a human settling it, not a migration; the counts are
  what the operator needs to do that.
  `customer_usd_money_refuses_the_upgrade_and_names_the_counts` and
  `a_platform_with_only_internal_usd_history_migrates`
  (`tests/billing_migration.rs`) pin both sides.
- **The internal tenant is priced and posted, and never marked.** Its usage
  is charged at its keys' group rates and debited from its ledger like any
  customer's, postpaid, so its balance runs negative as the platform's own
  consumption. `marks::project` and `marks::settle_locked` return before
  writing anything for it (`is_internal`): it gets no balance mark and no
  blocking quota mark, so nothing refuses its traffic when the balance or a
  quota crosses. An operator credit settles the negative balance for
  bookkeeping; it is the only thing the tenant's ledger needs from a human.
  A charge path that stops posting for `is_internal`, or a mark path that
  starts writing one, is the regression to look for here.
- **The ledger is append-only.** `billing_ledger_cny` and
  `pricing_adjustments_cny` take inserts only: a trigger refuses every
  `UPDATE`, `DELETE` and `TRUNCATE` (migrations `0014`, `0018`, and their
  `_cny` twins in `0027`), and a correction is a new entry, a re-pricing
  run's `adjustment`, never an edit of an old one. The balance cached on
  `billing_accounts` is moved only by `ledger::post_flagging`, in the same
  transaction as the entries it sums. A migration that drops or replaces a
  trigger turns `the_ledger_refuses_update_and_delete`,
  `the_ledger_refuses_truncate` or `the_adjustments_refuse_update_and_delete`
  red; a cache moved without an entry shows in
  `each_check_catches_its_own_drift` (`tests/billing_reconcile.rs`).
- **An entry is posted in its cause's transaction, and only for an amount
  other than 0.** The causes are a charge (`pricing::price_stored`, in
  intake), a top-up approval (`topups::approve`), an operator credit
  (`credits::create`) and an applied re-pricing run (`repricing::apply`).
  Each posts before it commits, so a cause and its entry commit or roll back
  together. A charge of 0 (a free model, a 100 % discount) and a run whose
  deltas for a tenant sum to 0 post nothing: the ledger's
  `CHECK (amount_micro_cny <> 0)` would fail the whole transaction, which for
  intake is every tenant's rows in the batch.
  `a_zero_charge_row_posts_nothing_and_does_not_fail_its_batch`,
  `a_charged_row_posts_one_debit_with_the_balance_after_it`,
  `a_re_sent_batch_and_a_repeated_event_post_once`,
  `approving_credits_the_balance_once_in_the_same_transaction`,
  `approving_twice_or_racing_two_approvals_credits_once`,
  `an_operator_credit_needs_a_reason_and_posts_one_entry` and
  `applying_twice_is_refused_and_posts_once` pin it; reconciliation's
  `charged_without_entry` finds a charge that lost its entry.
- **The lock order is the tenant row, then its account, then
  `RESOURCES_LOCK_KEY`, and intake never takes a tenant row.** The rules and
  their tests are under "Lock order" above. The one stated exception is
  `ledger::lock_account`'s fallback insert of a missing account, the only row
  intake inserts that references `tenants`: it key-shares the tenant row
  `NOWAIT` first, so a held tenant fails the batch at once (`503`, and the
  gateway sends it again) instead of making intake wait, and
  `ledger::backfill_accounts`, run at boot and on every housekeeping tick,
  keeps it unreachable in practice. Any other intake insert that references
  a tenant would wait on an operator's edit
  (`a_missing_account_never_makes_intake_wait_on_its_tenant`,
  `the_backfill_restores_a_missing_account`,
  `the_boot_and_every_housekeeping_tick_run_the_backfill`).
- **Marks are written only by `marks::project`, after intake commits.** It
  writes the balance document and, through `budgets::project`, every live
  quota's, under the tenant's billing lock, from the account and spend as
  they stand. Its callers are the billing step of `tenants::project` and
  `marks::settle` / `settle_locked`: the settler after intake's commit, and
  each admin money path inline in its own transaction, except a re-pricing
  apply, which changes up to `MAX_REPRICE_TENANTS` tenants at once: it flags
  them in its transaction and settles each after it commits, in a
  transaction of its own, so it never holds every tenant's intake, or the
  resources lock, while their marks are written
  (`an_apply_holds_no_other_tenants_intake_while_it_settles_marks`). Intake
  writes no document and never takes the resources lock; it only flags the
  account in its one update and notifies `billing_marks_dirty`, which is
  delivered at commit. A mark written from intake would take the resources lock there,
  and a document `write_resource` refused would fail every batch; a mark
  written anywhere else could be decided from a balance or spend another
  transaction is still moving. The one other write of a `budgets` document
  is the retirement of a deleted quota's (`budgets::delete`).
  `ingest_never_waits_on_the_resources_lock`,
  `a_batch_that_crosses_the_floor_wakes_the_settler`,
  `the_sweep_settles_an_account_whose_notification_was_lost` and
  `reactivating_a_tenant_writes_its_balance_mark_before_its_keys_open` pin
  the path. No test checks that `marks::project` is the only writer; grep
  `Kind::Budgets` before adding one.
- **One period function is shared with the gateway.** A quota's period is
  `aisix_core::models::BudgetPeriod::start_of` and nothing else: the control
  plane counts a charge in the period `start_of(price_instant)` gives it and
  writes a periodic mark's `period_start` as `start_of(now)`, and the gateway
  (`aisix_proxy::budget::period_start`) holds a mark only while its
  `period_start` equals `start_of(now)`. A second computation (a SQL
  `date_trunc('week', …)`, a local-time conversion) that disagrees at a
  boundary writes a mark that is never in force, or counts spend in a period
  no mark names. The summaries' `date_trunc(…, 'UTC')` day and month buckets
  are display only and decide no mark. `a_period_starts_on_the_utc_calendar`
  (aisix-core), `periods_follow_the_utc_calendar` (aisix-proxy),
  `a_charge_on_the_boundary_counts_in_the_period_it_starts` and
  `a_periodic_quota_is_marked_again_once_its_period_rolls_over`
  (`tests/billing_budgets.rs`) pin it.
- **Tenant views and `/dp/v1` lookups are whitelists, and a lookup's tenant,
  member and role come from the control plane's own key record.** Every
  `/api/*` billing answer and every `/dp/v1/*` answer is built field by field
  from named columns and event fields, never by passing a stored event or row
  through, so a field added to a usage event or a column added to
  `pricing_charges` (a provider, a cost, an upstream error) reaches no tenant
  until someone names it. The request lookup renders its rows through the
  call log's own `calls::tenant_view` and `VISIBLE`, so the two cannot drift.
  A lookup resolves the key through `access::key_access` (the key record
  joined to its tenant and its member's current role, read at lookup time);
  the gateway sends only the key id it authenticated, and nothing a usage row
  claims (`team_id`, `user_id`) decides whose rows are read.
  `a_tenants_call_log_names_no_channel_and_no_cost` and
  `the_request_lookup_names_no_channel` (exact key sets),
  `the_quota_lookup_names_no_channel`,
  `a_forbidden_models_call_reads_exactly_like_an_unknown_models`,
  `a_failed_failover_attempt_shows_no_upstream_status`,
  `rows_filed_under_another_tenant_never_appear`,
  `another_tenants_request_id_answers_like_an_unknown_one`,
  `a_role_change_shows_at_the_next_lookup` and
  `an_unknown_or_revoked_key_is_not_found` pin it.
- **The `/dp/v1` lookups run on a pool of their own, so they can never starve
  intake.** `GET /dp/v1/keys/{key_id}/{quota,prices,requests}` run on
  `AppState::lookups`: `access::LOOKUP_POOL_SIZE` (3) connections, each
  waited for at most `access::LOOKUP_ACQUIRE_TIMEOUT` (1 s) before the lookup
  answers `503`, which a gateway reads as the control plane being
  unavailable. A gateway asks on its callers' behalf, as often as they ask;
  it caps each key's request-cost lookups in flight, but nothing caps the
  keys or the gateways, so on the shared pool the lookups could hold every
  connection usage intake and the browser routes need. A new gateway lookup
  runs on `state.lookups`, never `state.db`, and takes no lock.
  `stalled_lookups_never_hold_up_usage_intake` pins it.
- **Each quota level writes only its own rows, and no write compares
  levels.** A quota is a `billing_budgets` row with its own document, one per
  setter: the operators' `team` rows (`set_by 'operator'`), an owner's or
  admin's `member` and `api_key` rows (`org_admin`), and a member's
  `api_key` rows on its own keys (`member`). An operator writes only
  `operator` rows; an owner or admin writes the tenant's own levels, a
  member's caps on its keys included, and never an operator's; a member
  writes only its own. A row above the caller answers `403 budget_managed`
  (`budget_routes`, `editable_row`). No write reads another level to accept,
  refuse or cap a limit: a child may exceed its parent (owner's decision),
  and the gateway refuses a request when any document that applies is
  exhausted, so the smallest remaining binds by construction.
  `MAX_BUDGETS_PER_TENANT` counts the tenant's own levels only.
  `each_level_edits_only_its_own_rows`, `an_operator_edits_only_operator_rows`,
  `a_member_caps_only_its_own_keys`, `a_child_may_exceed_its_parent` and
  `operator_rows_do_not_count_toward_the_tenant_limit` pin it.

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
- **A key's group is one routing name, chosen once, and it *is* the key's
  routing.** `keys::effective_groups` writes `[<the group's name>]` and
  nothing else, so a grouped key reaches that group's upstream accounts
  alone. A key never changes group: `PATCH` accepts `group_id` only while the
  key has none, because a silent move would silently change its price. A
  customer key with no group, or one whose group has left its tenant's list,
  is `closed_for_group` — projected disabled until a group is set once and
  the group is the tenant's again. The internal tenant's keys are never closed
  this way (R2): they may name any live group, and are priced and posted like
  any other tenant's. A customer key falling closed is a live product state on
  a database upgraded through `0024`, which backfills only what it can infer
  and leaves the rest NULL.

## Tenancy on `/api/*`

- **Every id in a path or a body is resolved with `tenant_id =
  MemberAuth.tenant_id` first**, and another tenant's id behaves exactly like
  a random UUID: `404 not_found` in the path, `422` at the field in the body.
  That lookup runs before the `If-Match` checks, so `428` and `412` answer only
  for the caller's own rows.
- No `/api/*` answer carries a provider, a provider key, a group name or an
  upstream model id. **"Group name" means the routing name** — the immutable
  identifier a provider key is matched against — never a group's
  `display_name`: a tenant is shown the display name of the group its key is
  priced in (design §5.1's `group: {id, display_name}` on the call log, on
  `/api/groups` and on its keys), and nothing tells it which routing name
  stands behind it.
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
- Every `SUM` is cast `::bigint` (`the_crate_has_no_uncast_sum`), or
  `::numeric` explicitly where the total can pass the `bigint` range (the
  u64 drop counts gateways report), narrowed before it is decoded.
- Every `ON CONFLICT … DO UPDATE` uses `EXCLUDED.` and table-qualified columns.
- A unique violation is recognised by SQLSTATE `23505` plus the constraint
  name, never by message text.
- A read-then-write invariant locks its parent row (`… FOR UPDATE`) or takes a
  transaction-scoped advisory lock. READ COMMITTED alone loses updates, and
  there is no `BEGIN IMMEDIATE`.
- Queries are runtime `sqlx::query` / `query_as`, never the `query!` macros.
  Migrations are additive, one file per change, and never edited once shipped.
  A migration may also fence an old path, as `0027` does with its `BEFORE
  INSERT` triggers: that is a one-way change, and while both versions run an
  older replica's intake fails and its batch is resent (see `Db::migrate`).

## Defaults fail closed

- A customer tenant names at least one provider group: `[]` is refused on save,
  and a key of a tenant found that way stays disabled. A customer key always
  writes `allowed_model_ids` (`[]` grants none, where an absent list would fall
  back to names) and `allowed_endpoints`.
- **A customer key names exactly one live group, and a key without one is
  closed.** `groups::key_group` refuses an id naming nothing, a retired group
  or another tenant's with `422 /group_id` ("分组不存在或不可用"), and an
  omitted id with "请选择分组" unless the tenant is assigned exactly one
  group. `closed_for_group` then keeps the key disabled while its group is
  absent or has left the tenant's list, and reopening is the same test the
  other way: assigning the group back opens its keys. The internal tenant's
  keys are the exception the owner set (R2): `closed_for_group` never closes
  one for a group reason, it may name any live group, and a key left with no
  group is priced only once it has one — the upgrade gives every such key the
  designated default instead (`0024`).
- A value that cannot be read closes: an unreadable gateway version, an
  unknown endpoint family on the gateway, a stored tenant default with no
  groups, a group name on a tenant, a tenant default or an upstream account
  that names no live group. A new group's routing name must match
  `[a-z0-9][a-z0-9_-]{0,63}` and never changes.
- Every route has a `ROUTE_ROLES` row. The census fails a route without one,
  and `tests/roles.rs` sends each mutating route as the role just below its
  minimum; a new route adds its fixture there.

## A bundle never carries etcd credentials

`gateways::bundle_env` never sets `AISIX_ETCD__USER` or
`AISIX_ETCD__PASSWORD_ENV`. kine has no Auth service, and a gateway that tries
to log in treats the `Unimplemented` answer as a fatal boot error.
