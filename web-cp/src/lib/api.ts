/**
 * A thin client for the control plane's operator API (`/admin/*`).
 *
 * The session is an HttpOnly cookie, so every request carries same-origin
 * credentials. Types use the API's own snake_case field names; only
 * `session()` returns a small camelCase wrapper.
 */

// ── errors ──────────────────────────────────────────────────────────────────

/** One problem in a request body, at a JSON path of it. */
export interface Detail {
  path: string;
  message: string;
}

/**
 * An error answer: the envelope's `error` as the message, its `code`, and
 * the optional `details`, `referenced_by` and `current_version`. An answer
 * that is not JSON has `code` `bad_response`; a request that never got an
 * answer has `code` `network` and status 0.
 */
export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: string,
    readonly details: Detail[],
    readonly referencedBy: { kind: string; id: string; name: string | null }[],
    readonly currentVersion: number | null,
  ) {
    super(message);
  }
}

interface Envelope {
  error?: string;
  code?: string;
  details?: Detail[];
  referenced_by?: { kind: string; id: string; name: string | null }[];
  current_version?: number;
}

const sessionEndedListeners = new Set<() => void>();

/**
 * Calls `listener` whenever a request answers `401 unauthenticated`: the
 * session has ended (it expired, was signed out elsewhere, or a password
 * change or a disable ended it). Returns the unsubscribe.
 */
export function onSessionEnded(listener: () => void): () => void {
  sessionEndedListeners.add(listener);
  return () => {
    sessionEndedListeners.delete(listener);
  };
}

/** The version in an `ETag` (`"12"`, `12` or `W/"12"`), or null. */
function etagVersion(etag: string | null): number | null {
  if (!etag) return null;
  const n = Number(etag.replace(/^W\//, "").replace(/"/g, ""));
  return Number.isInteger(n) ? n : null;
}

/**
 * One request. `ifMatch` sends `If-Match: "<n>"`, which the versioned routes
 * require on PUT, PATCH and DELETE; `version` is the answer's `ETag`. A
 * request `signal` aborted rejects with the abort's own error, not an
 * `ApiError`.
 */
export async function call<T>(
  method: string,
  path: string,
  body?: unknown,
  ifMatch?: number,
  signal?: AbortSignal,
): Promise<{ data: T; version: number | null }> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (ifMatch !== undefined) headers["if-match"] = `"${ifMatch}"`;
  let r: Response;
  try {
    r = await fetch(path, {
      method,
      credentials: "same-origin",
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
    });
  } catch (e) {
    if (signal?.aborted) throw e;
    throw new ApiError("无法连接控制面", 0, "network", [], [], null);
  }
  const text = await r.text();
  let parsed: unknown = null;
  // A proxy's 502 page or a transport failure is not JSON. Without this the
  // caller would see a parse error instead of the failed request.
  if (text) {
    try {
      parsed = JSON.parse(text);
    } catch {
      throw new ApiError(
        `服务端返回了非 JSON 响应（HTTP ${r.status}）`,
        r.status,
        "bad_response",
        [],
        [],
        null,
      );
    }
  }
  if (!r.ok) {
    const e = (parsed ?? {}) as Envelope;
    if (r.status === 401 && e.code === "unauthenticated") {
      for (const listener of sessionEndedListeners) listener();
    }
    throw new ApiError(
      e.error ?? `HTTP ${r.status}`,
      r.status,
      e.code ?? "bad_response",
      e.details ?? [],
      e.referenced_by ?? [],
      e.current_version ?? null,
    );
  }
  return { data: parsed as T, version: etagVersion(r.headers.get("etag")) };
}

// ── types (the API's field names) ───────────────────────────────────────────

export type OperatorRole = "viewer" | "operator" | "admin";

export interface Operator {
  id: string;
  email: string;
  display_name: string | null;
  role: OperatorRole;
}

/** An operator as the admin's list shows it. */
export interface OperatorAccount extends Operator {
  disabled: boolean;
  created_at: string;
}

export type RolloutState = "pending" | "propagating" | "applied" | "rejected" | "no_gateways";

export interface Rejection {
  instance_id: string;
  hostname: string;
  error: string;
}

export interface Rollout {
  state: RolloutState;
  kine_revision: number | null;
  applied: number;
  online: number;
  rejections: Rejection[];
}

export interface ResourceView<D = Record<string, unknown>> {
  kind: string;
  id: string;
  /** Credential fields read back as `"[REDACTED]"`. */
  doc: D;
  version: number;
  managed_by: string | null;
  updated_at: string;
  rollout: Rollout;
}

export interface TenantLimits {
  concurrency?: number;
  rpm?: number;
  tpm?: number;
  key_concurrency_max?: number;
  key_rpm_max?: number;
}

export interface Tenant {
  id: string;
  name: string;
  status: "pending" | "active" | "suspended";
  is_internal: boolean;
  billing_mode: "prepaid" | "postpaid";
  allowed_model_ids: string[];
  allowed_provider_groups: string[];
  limits: TenantLimits;
  member_count: number;
  key_count: number;
  version: number;
  created_at: string;
  approved_at: string | null;
  rollout: Rollout;
}

export type TenantDefaults = TenantLimits & { default_provider_groups: string[] };

export interface Member {
  id: string;
  email: string;
  display_name: string | null;
  role: "owner" | "admin" | "member" | "billing";
  disabled: boolean;
  created_at: string;
}

/** What `PATCH /admin/tenants/{id}` takes: each field given replaces the stored one. */
export interface TenantChanges {
  allowed_model_ids?: string[];
  allowed_provider_groups?: string[];
  /** Replaces every limit: a limit left out is unset. */
  limits?: TenantLimits;
}

/** An invitation at issue: the only answer that carries its token. */
export interface Invite {
  id: string;
  invite_token: string;
  /** Only when the control plane knows the tenant portal's address. */
  invite_url?: string;
  expires_at: string;
}

export interface AdminKey {
  id: string;
  tenant_id: string;
  label: string;
  prefix: string;
  member_id: string | null;
  model_ids: string[] | null;
  allowed_models: string[] | null;
  allowed_provider_groups: string[] | null;
  effective_provider_groups: string[];
  rate_limit: { concurrency?: number; rpm?: number };
  /** The operators' switch. */
  disabled: boolean;
  /** The tenant's own switch. */
  disabled_by_tenant: boolean;
  effective_disabled: boolean;
  expires_at: string | null;
  created_at: string;
  created_by: { kind: string; id: string | null };
  version: number;
}

/** What `POST /admin/tenants/{id}/keys` takes; a field left out is unset. */
export interface NewKey {
  label: string;
  /** Selected from the tenant's grants; left out, the key follows every grant. */
  model_ids?: string[];
  /** Model name patterns, for the platform's own tenant only. */
  allowed_models?: string[];
  allowed_provider_groups?: string[];
  rate_limit?: { concurrency?: number; rpm?: number };
  /** RFC 3339. */
  expires_at?: string;
}

/** A key at creation: the only answer that carries its plaintext. */
export interface MintedKey {
  key: AdminKey;
  plaintext: string;
}

export interface Instance {
  dp_id: string;
  credential_name: string;
  instance_id: string;
  hostname: string;
  version: string;
  uptime_seconds: number;
  applied_revision: number;
  config_hash: string | null;
  online: boolean;
  last_seen_at: string;
  exporter_health: unknown;
  rejected_count: number;
  /** null when the two versions cannot be compared, such as an unstamped build. */
  version_matches_cp: boolean | null;
}

export interface Credential {
  dp_id: string;
  name: string;
  fingerprint_sha256: string;
  not_after: string;
  created_at: string;
  created_by: string | null;
  revoked_at: string | null;
}

/** A credential at issue: the only answer that carries its private key. */
export interface IssuedCredential {
  dp_id: string;
  name: string;
  not_after: string;
  cert_pem: string;
  key_pem: string;
  ca_pem: string;
  env: Record<string, string>;
  /** The control plane's own notes on keeping the bundle. */
  note: string;
}

export interface RolloutSummary {
  online_instances: number;
  pending: number;
  propagating: number;
  reconcile_blocked: boolean;
  rejected: { kind: string; id: string; name: string | null; errors: Rejection[] }[];
  partially_compatible: { instance_id: string; kind: string; field: string; count: number }[];
  unsafe: { instance_id: string; hostname: string; kind: string; field: string }[];
  /** `kind` and `id` are null when projection stopped at no document (kine unreachable). */
  projection_stalled: { kind: string | null; id: string | null; error: string | null } | null;
  /** The owner is null when kine holds no claim and this database may not create one. */
  projection_blocked: { owner_database_id: string | null; database_id: string } | null;
  instance_cap_hit_at: string | null;
  rejections_truncated: string[];
}

export interface AuditEntry {
  id: number;
  at: string;
  actor: { kind: string; id: string | null; email: string | null };
  action: string;
  target_kind: string | null;
  target_id: string | null;
  tenant_id: string | null;
  detail: unknown;
}

/** One page of the audit log; `next_before` is null on the last. */
export interface AuditPage {
  items: AuditEntry[];
  next_before: number | null;
}

export interface ProviderGroup {
  name: string;
  provider_keys: { id: string; display_name: string | null }[];
}

export interface Environment {
  env_id: string;
  etcd_prefix: string;
  dp_base_url: string;
  etcd_endpoint: string;
  cp_version: string;
  api_contract: number;
}

/** A JSON Schema document, read by pointer in `schema.ts`. */
export type JsonSchema = Record<string, unknown>;

// ── session ─────────────────────────────────────────────────────────────────

export async function session(): Promise<{
  authed: boolean;
  apiContract: number;
  operator: Operator | null;
}> {
  const { data } = await call<{ authed?: boolean; api_contract?: number; operator?: Operator }>(
    "GET",
    "/admin/session",
  );
  return {
    authed: data.authed === true,
    // A backend without the field is older than every contract.
    apiContract: data.api_contract ?? 0,
    operator: data.operator ?? null,
  };
}

export async function login(email: string, password: string): Promise<Operator> {
  return (await call<Operator>("POST", "/admin/login", { email, password })).data;
}

export async function logout(): Promise<void> {
  await call<null>("POST", "/admin/logout");
}

/** Ends the caller's other sessions. */
export async function changePassword(current: string, next: string): Promise<void> {
  await call<null>("POST", "/admin/password", { current_password: current, new_password: next });
}

// ── namespaces ──────────────────────────────────────────────────────────────

const seg = encodeURIComponent;

/** The generic resource API. `kind` is the plural, such as `provider_keys`. */
export const resources = {
  async list<D>(kind: string): Promise<ResourceView<D>[]> {
    return (await call<{ items: ResourceView<D>[] }>("GET", `/admin/resources/${seg(kind)}`)).data
      .items;
  },
  async get<D>(kind: string, id: string, signal?: AbortSignal): Promise<ResourceView<D>> {
    return (
      await call<ResourceView<D>>(
        "GET",
        `/admin/resources/${seg(kind)}/${seg(id)}`,
        undefined,
        undefined,
        signal,
      )
    ).data;
  },
  async create<D>(kind: string, doc: D): Promise<ResourceView<D>> {
    return (await call<ResourceView<D>>("POST", `/admin/resources/${seg(kind)}`, { doc })).data;
  },
  async save<D>(kind: string, id: string, doc: D, version: number): Promise<ResourceView<D>> {
    return (
      await call<ResourceView<D>>("PUT", `/admin/resources/${seg(kind)}/${seg(id)}`, { doc }, version)
    ).data;
  },
  async remove<D>(kind: string, id: string, version: number): Promise<ResourceView<D>> {
    return (
      await call<ResourceView<D>>("DELETE", `/admin/resources/${seg(kind)}/${seg(id)}`, undefined, version)
    ).data;
  },
};

/** The gateway's strict schemas. `resource` is the singular, such as `provider_key`. */
export const schemas = {
  async get(resource: string): Promise<JsonSchema> {
    return (await call<JsonSchema>("GET", `/admin/schemas/${seg(resource)}`)).data;
  },
};

// Filled in by the tasks that build their pages, one namespace each.

/** Tenants, and their members and keys, as operators see them. */
export const tenants = {
  async list(status?: Tenant["status"]): Promise<Tenant[]> {
    const query = status ? `?status=${seg(status)}` : "";
    return (await call<{ items: Tenant[] }>("GET", `/admin/tenants${query}`)).data.items;
  },
  async get(id: string, signal?: AbortSignal): Promise<Tenant> {
    return (await call<Tenant>("GET", `/admin/tenants/${seg(id)}`, undefined, undefined, signal)).data;
  },
  async update(id: string, changes: TenantChanges, version: number): Promise<Tenant> {
    return (await call<Tenant>("PATCH", `/admin/tenants/${seg(id)}`, changes, version)).data;
  },
  /** pending → active. */
  async approve(id: string): Promise<Tenant> {
    return (await call<Tenant>("POST", `/admin/tenants/${seg(id)}/approve`)).data;
  },
  /** active → suspended: the tenant's keys stop working. */
  async suspend(id: string): Promise<Tenant> {
    return (await call<Tenant>("POST", `/admin/tenants/${seg(id)}/suspend`)).data;
  },
  /** suspended → active. */
  async reactivate(id: string): Promise<Tenant> {
    return (await call<Tenant>("POST", `/admin/tenants/${seg(id)}/reactivate`)).data;
  },
  async members(id: string): Promise<Member[]> {
    return (await call<{ items: Member[] }>("GET", `/admin/tenants/${seg(id)}/members`)).data.items;
  },
  /** Disabling a member ends their sessions and switches their keys off with them. */
  async setMemberDisabled(memberId: string, disabled: boolean): Promise<Member> {
    return (await call<Member>("PATCH", `/admin/members/${seg(memberId)}`, { disabled })).data;
  },
  async invite(id: string, email: string, role: Member["role"]): Promise<Invite> {
    return (await call<Invite>("POST", `/admin/tenants/${seg(id)}/invites`, { email, role })).data;
  },
  async keys(id: string): Promise<AdminKey[]> {
    return (await call<{ items: AdminKey[] }>("GET", `/admin/tenants/${seg(id)}/keys`)).data.items;
  },
};

export const keys = {
  /** The answer is the only one that carries the plaintext: never store or log it. */
  async create(tenantId: string, body: NewKey): Promise<MintedKey> {
    return (await call<MintedKey>("POST", `/admin/tenants/${seg(tenantId)}/keys`, body)).data;
  },
  /**
   * `disabled` is the operators' switch; `allowed_provider_groups: null` drops
   * the key's own groups, so it follows its tenant's.
   */
  async update(
    id: string,
    changes: { disabled?: boolean; allowed_provider_groups?: string[] | null },
    version: number,
  ): Promise<AdminKey> {
    return (await call<AdminKey>("PATCH", `/admin/keys/${seg(id)}`, changes, version)).data;
  },
  async revoke(id: string, version: number): Promise<void> {
    await call<null>("DELETE", `/admin/keys/${seg(id)}`, undefined, version);
  },
};

/** Gateway instances and the credentials they boot from. */
export const gateways = {
  async instances(): Promise<Instance[]> {
    return (await call<{ items: Instance[] }>("GET", "/admin/gateways/instances")).data.items;
  },
  async credentials(): Promise<Credential[]> {
    return (await call<{ items: Credential[] }>("GET", "/admin/gateways/credentials")).data.items;
  },
  /** The only answer that carries the private key: never store or log it. */
  async issue(name: string, validityDays?: number): Promise<IssuedCredential> {
    return (
      await call<IssuedCredential>("POST", "/admin/gateways/credentials", {
        name,
        validity_days: validityDays,
      })
    ).data;
  },
  async revoke(dpId: string): Promise<Credential> {
    return (await call<Credential>("POST", `/admin/gateways/credentials/${seg(dpId)}/revoke`)).data;
  },
};

/** The environment's rollout, and the two admin actions that unblock it. */
export const rollout = {
  async summary(): Promise<RolloutSummary> {
    return (await call<RolloutSummary>("GET", "/admin/rollout")).data;
  },
  async reconcile(allowDeletes: boolean): Promise<{ deleted_strays: number; marked_dirty: number }> {
    return (
      await call<{ deleted_strays: number; marked_dirty: number }>(
        "POST",
        "/admin/rollout/reconcile",
        { allow_deletes: allowDeletes },
      )
    ).data;
  },
  /** Moves the environment to this control plane's database, from `fromDatabaseId`. */
  async takeOver(fromDatabaseId: string): Promise<{ owner_database_id: string }> {
    return (
      await call<{ owner_database_id: string }>("POST", "/admin/rollout/ownership", {
        take_over_from: fromDatabaseId,
      })
    ).data;
  },
};

export const environment = {
  async get(): Promise<Environment> {
    return (await call<Environment>("GET", "/admin/environment")).data;
  },
};

/** Operator accounts (an admin's). */
export const operators = {
  /** Every account, oldest first. */
  async list(): Promise<OperatorAccount[]> {
    return (await call<{ items: OperatorAccount[] }>("GET", "/admin/operators")).data.items;
  },
  async create(body: {
    email: string;
    display_name?: string;
    role: OperatorRole;
    password: string;
  }): Promise<Operator> {
    return (await call<Operator>("POST", "/admin/operators", body)).data;
  },
  /** `409 last_admin` when this would leave no enabled admin; disabling ends the account's sessions. */
  async update(id: string, changes: { role?: OperatorRole; disabled?: boolean }): Promise<Operator> {
    return (await call<Operator>("PATCH", `/admin/operators/${seg(id)}`, changes)).data;
  },
};

/** The audit log. */
export const audit = {
  /**
   * One page, newest first, at the control plane's own page size. `before`
   * is the previous page's `next_before`; the filters narrow every page.
   */
  async list(q: { before?: number; target_kind?: string; tenant_id?: string }): Promise<AuditPage> {
    const params = new URLSearchParams();
    if (q.before !== undefined) params.set("before", String(q.before));
    if (q.target_kind) params.set("target_kind", q.target_kind);
    if (q.tenant_id) params.set("tenant_id", q.tenant_id);
    const query = params.toString();
    return (await call<AuditPage>("GET", `/admin/audit${query ? `?${query}` : ""}`)).data;
  },
};

export const settings = {
  async tenantDefaults(): Promise<TenantDefaults> {
    return (await call<TenantDefaults>("GET", "/admin/settings/tenant-defaults")).data;
  },
  /** Replaces the whole defaults: a limit left out is unset. */
  async saveTenantDefaults(defaults: TenantDefaults): Promise<TenantDefaults> {
    return (await call<TenantDefaults>("PUT", "/admin/settings/tenant-defaults", defaults)).data;
  },
};

/** Every group a live upstream account is in, with its accounts. */
export const providerGroups = {
  async list(): Promise<ProviderGroup[]> {
    return (await call<{ items: ProviderGroup[] }>("GET", "/admin/provider-groups")).data.items;
  },
};

// ── billing (accounts, ledger, usage, quotas) ───────────────────────────────

/** One tenant's billing account. All amounts are integer micro-dollars. */
export interface BillingAccount {
  tenant_id: string;
  tenant_name: string;
  status: string;
  is_internal: boolean;
  billing_mode: "prepaid" | "postpaid";
  balance_micro_usd: number;
  credit_limit_micro_usd: number;
  available_micro_usd: number;
  /** Whether the balance rule blocks the tenant now; never for the internal tenant. */
  exhausted: boolean;
  pending_topups: number;
}

export interface LedgerItem {
  seq: number;
  posted_at: string;
  kind: "usage" | "topup" | "adjustment";
  amount_micro_usd: number;
  balance_after_micro_usd: number;
  event_id: string | null;
  model: { id: string | null; name: string } | null;
  topup_id: string | null;
  repricing_run_id: string | null;
}

export interface LedgerPage {
  items: LedgerItem[];
  /** The `before` of the next page; null on the last. */
  next_before: number | null;
}

export interface LedgerSummaryRow {
  period_start: string;
  usage_micro_usd: number;
  topups_micro_usd: number;
  adjustments_micro_usd: number;
  closing_balance_micro_usd: number;
}

export interface UsageSummaryRow {
  period_start: string;
  /** `id` null: calls outside the tenant's grants, or the row that rolls up everything past the row cap (`name` `other`). */
  model: { id: string | null; name: string };
  requests: number;
  charged_micro_usd: number;
  estimated_requests: number;
  quantities: Record<string, number>;
}

export interface Budget {
  id: string;
  scope: "team" | "member" | "api_key";
  target: { id: string; label: string };
  target_live: boolean;
  set_by: "operator" | "org_admin" | "member";
  kind: "periodic" | "cumulative";
  period: "day" | "week" | "month" | null;
  limit_micro_usd: number;
  spent_micro_usd: number;
  remaining_micro_usd: number;
  period_start: string | null;
  counted_from: string | null;
  exhausted: boolean;
  created_at: string;
}

/** A UTC half-open interval, as the ledger and summary routes take it. */
export interface TimeRange {
  from?: string;
  to?: string;
}

function rangeQuery(params: URLSearchParams, q: TimeRange): void {
  if (q.from) params.set("from", q.from);
  if (q.to) params.set("to", q.to);
}

/** Billing accounts and one tenant's ledger, usage and quotas, as operators read them. */
export const billing = {
  /** Every tenant but pending ones: customers by name, the internal tenant last. */
  async accounts(): Promise<BillingAccount[]> {
    return (await call<{ items: BillingAccount[] }>("GET", "/admin/billing/accounts")).data.items;
  },
  async account(tenantId: string, signal?: AbortSignal): Promise<BillingAccount> {
    return (await call<BillingAccount>("GET", `/admin/tenants/${seg(tenantId)}/billing`, undefined, undefined, signal)).data;
  },
  /** Operator and up; `creditLimitUsd` is dollars and cents, 0 allowed. Answers the settled account. */
  async setCreditLimit(tenantId: string, creditLimitUsd: string): Promise<BillingAccount> {
    return (await call<BillingAccount>("PATCH", `/admin/tenants/${seg(tenantId)}/billing`, { credit_limit_usd: creditLimitUsd })).data;
  },
  /** One page of entries, newest first; `before` is the previous page's `next_before`. */
  async ledger(
    tenantId: string,
    q: TimeRange & { kind?: LedgerItem["kind"]; before?: number; limit?: number },
    signal?: AbortSignal,
  ): Promise<LedgerPage> {
    const params = new URLSearchParams();
    rangeQuery(params, q);
    if (q.kind) params.set("kind", q.kind);
    if (q.before !== undefined) params.set("before", String(q.before));
    if (q.limit !== undefined) params.set("limit", String(q.limit));
    const query = params.toString();
    return (
      await call<LedgerPage>("GET", `/admin/tenants/${seg(tenantId)}/billing/ledger${query ? `?${query}` : ""}`, undefined, undefined, signal)
    ).data;
  },
  async ledgerSummary(
    tenantId: string,
    q: { group: "day" | "month"; from: string; to: string },
    signal?: AbortSignal,
  ): Promise<LedgerSummaryRow[]> {
    const params = new URLSearchParams({ group: q.group, from: q.from, to: q.to });
    return (
      await call<{ items: LedgerSummaryRow[] }>(
        "GET",
        `/admin/tenants/${seg(tenantId)}/billing/summary?${params}`,
        undefined,
        undefined,
        signal,
      )
    ).data.items;
  },
  async usageSummary(
    tenantId: string,
    q: { group: "day" | "month"; from: string; to: string },
    signal?: AbortSignal,
  ): Promise<UsageSummaryRow[]> {
    const params = new URLSearchParams({ group: q.group, from: q.from, to: q.to });
    return (
      await call<{ items: UsageSummaryRow[] }>(
        "GET",
        `/admin/tenants/${seg(tenantId)}/usage/summary?${params}`,
        undefined,
        undefined,
        signal,
      )
    ).data.items;
  },
  /** Every live quota of the tenant at every level, oldest first. */
  async budgets(tenantId: string, signal?: AbortSignal): Promise<Budget[]> {
    return (await call<{ items: Budget[] }>("GET", `/admin/tenants/${seg(tenantId)}/budgets`, undefined, undefined, signal)).data.items;
  },
  /** Admin only: credits the tenant now. `amountUsd` is dollars and cents above 0; answers the top-up the backend recorded. Not idempotent: send once. */
  async credit(tenantId: string, body: { amount_usd: string; reason: string; reference?: string }): Promise<CreditedTopup> {
    return (await call<CreditedTopup>("POST", `/admin/tenants/${seg(tenantId)}/credits`, body)).data;
  },
};

/** The top-up an operator credit recorded. */
export interface CreditedTopup {
  id: string;
  origin: "operator";
  amount_micro_usd: number;
  reference: string;
  credit_reason: string | null;
}
/** One top-up as an operator reads it: a tenant's request, or an operator's credit. */
export interface Topup {
  id: string;
  origin: "tenant" | "operator";
  /** Integer micro-dollars. */
  amount_micro_usd: number;
  note: string;
  reference: string;
  status: "pending" | "approved" | "rejected";
  tenant_id: string;
  tenant_name: string;
  requested_by: { id: string; display_name: string | null } | null;
  requested_at: string;
  decided_at: string | null;
  decided_by: { id: string; email: string | null } | null;
  reject_reason: string | null;
  credit_reason: string | null;
}

/** Top-up requests and their decisions (billing). */
export const topups = {
  /** One page, newest first; `before` is the previous page's `next_before`. */
  async list(q: {
    status?: Topup["status"];
    origin?: Topup["origin"];
    tenant_id?: string;
    before?: number;
  }): Promise<{ items: Topup[]; next_before: number | null }> {
    const params = new URLSearchParams();
    if (q.status) params.set("status", q.status);
    if (q.origin) params.set("origin", q.origin);
    if (q.tenant_id) params.set("tenant_id", q.tenant_id);
    if (q.before !== undefined) params.set("before", String(q.before));
    const query = params.toString();
    return (
      await call<{ items: Topup[]; next_before: number | null }>("GET", `/admin/billing/topups${query ? `?${query}` : ""}`)
    ).data;
  },
  /** The amount is posted to the tenant's balance at once. `409 topup_state` when it is no longer pending. */
  async approve(id: string): Promise<Topup> {
    return (await call<Topup>("POST", `/admin/billing/topups/${seg(id)}/approve`)).data;
  },
  /** The reason (1 to 500 characters) is what the tenant reads. `409 topup_state` when it is no longer pending. */
  async reject(id: string, reason: string): Promise<Topup> {
    return (await call<Topup>("POST", `/admin/billing/topups/${seg(id)}/reject`, { reason })).data;
  },
};

// ── organization quotas (operator-set `team` quotas) ────────────────────────

/** Operator and up. A periodic quota's `period` defaults to month; a cumulative one has none. `409 budget_exists` for the same kind and period. */
export async function createOrgQuota(
  tenantId: string,
  body: { kind: "periodic" | "cumulative"; period?: "day" | "week" | "month"; limit_usd: string },
): Promise<Budget> {
  return (await call<Budget>("POST", `/admin/tenants/${seg(tenantId)}/budgets`, body)).data;
}

/** Operator and up; only a quota the operator set (`403 budget_managed` otherwise). */
export async function updateOrgQuota(tenantId: string, id: string, limitUsd: string): Promise<Budget> {
  return (await call<Budget>("PATCH", `/admin/tenants/${seg(tenantId)}/budgets/${seg(id)}`, { limit_usd: limitUsd })).data;
}

export async function deleteOrgQuota(tenantId: string, id: string): Promise<void> {
  await call<null>("DELETE", `/admin/tenants/${seg(tenantId)}/budgets/${seg(id)}`);
}
