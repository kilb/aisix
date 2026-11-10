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
 * the optional `details`, `referenced_by`, `counts` and `current_version`.
 * An answer that is not JSON has `code` `bad_response`; a request that never
 * got an answer has `code` `network` and status 0.
 */
export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: string,
    readonly details: Detail[],
    readonly referencedBy: { kind: string; id: string; name: string | null }[],
    readonly currentVersion: number | null,
    /** The figures a refusal explains itself with, as the backend read them. */
    readonly counts: Record<string, number> | null = null,
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
  counts?: Record<string, number>;
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
 * Reads a response into `{data, version}` or throws an [`ApiError`]: a body
 * that is not JSON is `bad_response` (a proxy's 502 page, or a transport
 * failure, would otherwise reach the caller as a parse error), a `401
 * unauthenticated` ends the session for every listener, and any other `!ok`
 * keeps the envelope's own message, code, details, references and version.
 *
 * Shared by [`call`] and the upload that splices the dataset in as raw text,
 * so the session listener cannot be wired at one and missed at the other.
 */
async function readAnswer<T>(r: Response): Promise<{ data: T; version: number | null }> {
  const text = await r.text();
  let parsed: unknown = null;
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
      e.counts ?? null,
    );
  }
  return { data: parsed as T, version: etagVersion(r.headers.get("etag")) };
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
  return readAnswer<T>(r);
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
  /** `all`: every public model, resolved live. `selected`: `allowed_model_ids`. */
  model_scope: "all" | "selected";
  /** The stored selection; it grants nothing on its own while the scope is `all`. */
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
  /** Sending `allowed_model_ids` alone means `selected`. */
  model_scope?: "all" | "selected";
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

/** The one group a key names, fixed when it was created. */
export interface KeyGroup {
  id: string;
  /** The routing name: immutable, operator-only. */
  name: string;
  display_name: string;
}

export interface AdminKey {
  id: string;
  tenant_id: string;
  label: string;
  prefix: string;
  member_id: string | null;
  model_ids: string[] | null;
  allowed_models: string[] | null;
  /** The override a key minted before groups were data carries; no request body sets it. */
  allowed_provider_groups: string[] | null;
  /** The group the key routes to, chosen at creation; `null` for a legacy key that has none yet. */
  group: KeyGroup | null;
  /** What the gateway is given: the key's group's name, or the old rule for a groupless key. */
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
  /** Whether the plaintext can be shown again; a key minted before the sealed copy was stored cannot. */
  plaintext_available: boolean;
}

/** What `POST /admin/tenants/{id}/keys` takes; a field left out is unset. */
export interface NewKey {
  label: string;
  /** Selected from the tenant's grants; left out, the key follows every grant. */
  model_ids?: string[];
  /** Model name patterns, for the platform's own tenant only. */
  allowed_models?: string[];
  /** The one group the key routes to. Required: any live group for the internal tenant, one of its assigned groups otherwise. */
  group_id?: string;
  rate_limit?: { concurrency?: number; rpm?: number };
  /** RFC 3339. */
  expires_at?: string;
}

/** A key at creation, with its plaintext. A later reveal answers the same plaintext. */
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
  /** Never store or log the plaintext: neither this answer nor a reveal of it. */
  async create(tenantId: string, body: NewKey): Promise<MintedKey> {
    return (await call<MintedKey>("POST", `/admin/tenants/${seg(tenantId)}/keys`, body)).data;
  },
  /**
   * A key's plaintext again, from the copy the control plane keeps sealed.
   * Only the platform's own tenant's keys: a customer's answers `403`. A
   * POST, because reading a credential is an action: it is audited there,
   * and its answer is never cached. A key minted before that copy existed
   * answers `409 plaintext_unavailable`.
   */
  async reveal(id: string): Promise<string> {
    return (await call<{ plaintext: string }>("POST", `/admin/keys/${seg(id)}/reveal`)).data.plaintext;
  },
  /**
   * `disabled` is the operators' switch; `group_id` names the key's group and
   * is accepted only while the key has none, so a legacy key takes one once.
   */
  async update(
    id: string,
    changes: { disabled?: boolean; group_id?: string },
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

// ── pricing ─────────────────────────────────────────────────────────────────

/** Rates of one tier or band: decimal strings in USD per million units, at most 6 decimals. */
export type RateMap = Record<string, string>;

/** One tier of a price card: its base rates, and any further parts (long-context bands) as the API has them. */
export interface TierRates {
  base: RateMap;
  [more: string]: unknown;
}

/** A price card: tiers by name; `default` is the one a request without a tier is priced at. */
export type RateCard = Record<string, TierRates>;

/** Something to know about a price that was written all the same. */
export interface PriceWarning {
  code: string;
  message: string;
  revision_id?: number;
  effective_from?: string;
}

export interface CatalogueEntry {
  key: string;
  aliases: string[];
  source_url: string | null;
  as_of: string | null;
  effective_from: string | null;
  notes: string | null;
  card: RateCard;
}

export const pricing = {
  async catalogue(): Promise<{ version: string | number; items: CatalogueEntry[] }> {
    return (await call<{ version: string | number; items: CatalogueEntry[] }>("GET", "/admin/pricing/catalogue")).data;
  },
  /** One tenant's discount plan. Its prices are its keys' groups', so there are no per-tenant prices. */
  async tenantPlan(tenantId: string): Promise<TenantPlan> {
    return (await call<TenantPlan>("GET", `/admin/pricing/tenants/${seg(tenantId)}`)).data;
  },
};

// ── pricing: cost prices ────────────────────────────────────────────────────

/** One cost price revision: what an upstream model costs the platform, through one account or any. */
export interface CostRevision {
  revision_id: number;
  engine_version: number;
  upstream_provider: string;
  upstream_model: string;
  provider_key_id: string | null;
  effective_from: string;
  card: RateCard;
  source: string;
  created_by: string | null;
  created_at: string;
}

export type CostCreated = CostRevision & { warnings: PriceWarning[] };

export interface NewCostPrice {
  upstream_provider: string;
  upstream_model: string;
  provider_key_id?: string;
  effective_from: string;
  card: RateCard;
}

export const costPricing = {
  /** Cost revisions, newest first; `before` is the previous page's `next_before`. */
  async listCost(q: {
    upstream_provider?: string;
    upstream_model?: string;
    provider_key_id?: string;
    limit?: number;
    before?: number;
  }): Promise<{ items: CostRevision[]; next_before: number | null }> {
    const params = new URLSearchParams();
    if (q.upstream_provider) params.set("upstream_provider", q.upstream_provider);
    if (q.upstream_model) params.set("upstream_model", q.upstream_model);
    if (q.provider_key_id) params.set("provider_key_id", q.provider_key_id);
    if (q.limit !== undefined) params.set("limit", String(q.limit));
    if (q.before !== undefined) params.set("before", String(q.before));
    const query = params.toString();
    return (
      await call<{ items: CostRevision[]; next_before: number | null }>("GET", `/admin/pricing/cost${query ? `?${query}` : ""}`)
    ).data;
  },
  async createCost(body: NewCostPrice): Promise<CostCreated> {
    return (await call<CostCreated>("POST", "/admin/pricing/cost", body)).data;
  },
};

// ── pricing: tenant plans ───────────────────────────────────────────────────

/** One discount revision: basis points off every charge of the tenant, from `effective_from`. */
export interface DiscountRevision {
  revision_id: number;
  engine_version: number;
  tenant_id: string;
  effective_from: string;
  discount_bps: number;
  created_by: string | null;
  created_at: string;
}

/** A tenant's plan: its discount in force and the ones scheduled after it. */
export interface TenantPlan {
  tenant_id: string;
  discount: { current: DiscountRevision | null; upcoming: DiscountRevision[] };
}

/** A tenant's discount revisions: an append-only list, one write per change. */
export const tenantPricing = {
  async createDiscount(body: { tenant_id: string; effective_from: string; discount_bps: number }): Promise<DiscountRevision> {
    return (await call<DiscountRevision>("POST", "/admin/pricing/discounts", body)).data;
  },
};

// ── billing (accounts, ledger, usage, quotas) ───────────────────────────────

/** One tenant's billing account. All amounts are integer micro-cny. */
export interface BillingAccount {
  tenant_id: string;
  tenant_name: string;
  status: string;
  is_internal: boolean;
  billing_mode: "prepaid" | "postpaid";
  balance_micro_cny: number;
  credit_limit_micro_cny: number;
  available_micro_cny: number;
  /** Whether the balance rule blocks the tenant now; never for the internal tenant. */
  exhausted: boolean;
  pending_topups: number;
}

export interface LedgerItem {
  seq: number;
  posted_at: string;
  kind: "usage" | "topup" | "adjustment";
  amount_micro_cny: number;
  balance_after_micro_cny: number;
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
  usage_micro_cny: number;
  topups_micro_cny: number;
  adjustments_micro_cny: number;
  closing_balance_micro_cny: number;
}

export interface UsageSummaryRow {
  period_start: string;
  /** `id` null: calls outside the tenant's grants, or the row that rolls up everything past the row cap (`name` `other`). */
  model: { id: string | null; name: string };
  requests: number;
  charged_micro_cny: number;
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
  limit_micro_cny: number;
  spent_micro_cny: number;
  remaining_micro_cny: number;
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
  /** Operator and up; `creditLimitCny` is yuan and fen, 0 allowed. Answers the settled account. */
  async setCreditLimit(tenantId: string, creditLimitCny: string): Promise<BillingAccount> {
    return (await call<BillingAccount>("PATCH", `/admin/tenants/${seg(tenantId)}/billing`, { credit_limit_cny: creditLimitCny })).data;
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
  /** Admin only: credits the tenant now. `amount_cny` is yuan and fen above 0; answers the top-up the backend recorded. Not idempotent: send once. */
  async credit(tenantId: string, body: { amount_cny: string; reason: string; reference?: string }): Promise<CreditedTopup> {
    return (await call<CreditedTopup>("POST", `/admin/tenants/${seg(tenantId)}/credits`, body)).data;
  },
};

/** The top-up an operator credit recorded. */
export interface CreditedTopup {
  id: string;
  origin: "operator";
  amount_micro_cny: number;
  reference: string;
  credit_reason: string | null;
}
/** One top-up as an operator reads it: a tenant's request, or an operator's credit. */
export interface Topup {
  id: string;
  origin: "tenant" | "operator";
  /** Integer micro-cny. */
  amount_micro_cny: number;
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
  body: { kind: "periodic" | "cumulative"; period?: "day" | "week" | "month"; limit_cny: string },
): Promise<Budget> {
  return (await call<Budget>("POST", `/admin/tenants/${seg(tenantId)}/budgets`, body)).data;
}

/** Operator and up; only a quota the operator set (`403 budget_managed` otherwise). */
export async function updateOrgQuota(tenantId: string, id: string, limitCny: string): Promise<Budget> {
  return (await call<Budget>("PATCH", `/admin/tenants/${seg(tenantId)}/budgets/${seg(id)}`, { limit_cny: limitCny })).data;
}

export async function deleteOrgQuota(tenantId: string, id: string): Promise<void> {
  await call<null>("DELETE", `/admin/tenants/${seg(tenantId)}/budgets/${seg(id)}`);
}

/** One charge: what a stored usage row was billed in micro-cny, or why it could not be. */
export interface Charge {
  seq: number;
  event_id: string;
  tenant_id: string;
  api_key_id: string;
  user_id: string;
  requested_model_id: string;
  occurred_at: string;
  status: "charged" | "exempt" | "nothing_to_bill" | "unpriced";
  charge_micro_cny: number | null;
  /** What the call cost the platform at its upstream, in micro-dollars: the cost side stays USD. */
  cost_micro_usd: number | null;
  group_id: string | null;
  baseline_revision_id: number | null;
  rate_revision_id: number | null;
  /** The group's rate in force, as the exact decimal string it was typed as. */
  cny_per_usd_e4: number | null;
  /** CNY charged per USD of cost, four decimals; `null` when the cost is not known. */
  realised_cny_per_usd: string | null;
  service_tier: string | null;
  requested_service_tier: string | null;
  long_context_above: number | null;
  estimated: boolean;
  exemption: string | null;
  /** The pricing code behind an `unpriced` charge. */
  error_code: string | null;
}

export interface ChargeFilter {
  status?: string;
  tenant_id?: string;
  api_key_id?: string;
  event_id?: string;
  before?: number;
}

/** The per-call charges, newest first. */
export const charges = {
  async list(f: ChargeFilter = {}): Promise<{ items: Charge[]; next_before: number | null }> {
    const q = new URLSearchParams();
    for (const [k, v] of Object.entries(f)) if (v !== undefined && v !== "") q.set(k, String(v));
    const qs = q.toString();
    return (await call<{ items: Charge[]; next_before: number | null }>("GET", `/admin/pricing/charges${qs ? `?${qs}` : ""}`)).data;
  },
};

/** A price revision the pricing engine cannot use; charges that need it come out unpriced. */
export interface InvalidRevision {
  table: string;
  revision_id: string | number;
  reason: string;
}

/** The pricing health: version, unusable revisions and the last 24 hours' unpriced charges by code. */
export interface PricingStatus {
  pricing_version: number | null;
  invalid_revisions: InvalidRevision[];
  unpriced_last_24h: { total: number; by_code: Record<string, number> };
}

export const pricingStatus = {
  async get(): Promise<PricingStatus> {
    return (await call<PricingStatus>("GET", "/admin/pricing/status")).data;
  },
};

/** One tenant's part of a re-pricing run. Amounts are integer micro-cny. */
export interface RepricingTenant {
  tenant_id: string;
  tenant_name: string;
  /** The selected calls filed under the tenant. */
  events: number;
  changed: number;
  /** Positive: the tenant was charged too little before; negative: too much. */
  delta_micro_cny: number;
}

/** A re-pricing run: previewed with its totals, then (by an admin) applied. */
export interface RepricingRun {
  id: string;
  status: "previewed" | "applied";
  filter: { from: string; to: string; model_id: string | null; tenant_id: string | null; group_id: string | null; status: string | null };
  reason: string;
  events: number;
  changed: number;
  still_unpriced: number;
  skipped_unattributed: number;
  delta_micro_cny: number;
  by_tenant: RepricingTenant[];
  created_by: { id: string; email: string };
  created_at: string;
  applied_by: { id: string; email: string | null } | null;
  applied_at: string | null;
}

export interface RepricingFilterInput {
  from: string;
  to: string;
  model_id?: string;
  tenant_id?: string;
  group_id?: string;
  status?: string;
  reason: string;
}

/** Re-pricing: correcting past charges at the prices in force now. */
export const repricing = {
  /** One page of runs, newest first; `before` is the previous page's `next_before`. */
  async list(before?: number): Promise<{ items: RepricingRun[]; next_before: number | null }> {
    const query = before === undefined ? "" : `?before=${before}`;
    return (await call<{ items: RepricingRun[]; next_before: number | null }>("GET", `/admin/billing/repricing${query}`)).data;
  },
  /** Prices the selected calls again and stores the run as `previewed`. Posts nothing. */
  async preview(f: RepricingFilterInput): Promise<RepricingRun> {
    return (await call<RepricingRun>("POST", "/admin/billing/repricing", f)).data;
  },
  async get(id: string): Promise<RepricingRun> {
    return (await call<RepricingRun>("GET", `/admin/billing/repricing/${seg(id)}`)).data;
  },
  /** Admin only. Moves money. `409 stale_preview` when prices or the selected calls changed since the preview; `409 run_state` when it is no longer `previewed`. */
  async apply(id: string): Promise<RepricingRun> {
    return (await call<RepricingRun>("POST", `/admin/billing/repricing/${seg(id)}/apply`, {})).data;
  },
};

export type ReconcileCheck = "balance" | "usage" | "adjustments" | "topups" | "running_balance";

export interface ReconcileMismatch {
  tenant_id: string;
  tenant_name: string;
  check: ReconcileCheck | string;
  expected_micro_cny: number;
  actual_micro_cny: number;
  /** The ledger entry that breaks the running balance; null for the other checks. */
  seq: number | null;
}

export interface InstanceDrops {
  dp_id: string;
  credential_name: string;
  instance_id: string;
  hostname: string;
  last_seen_at: string;
  /** Usage events dropped since the process started, by reason. */
  drops: Record<string, number>;
}

export interface Reconciliation {
  checked_at: string;
  tenant_id: string | null;
  mismatches: ReconcileMismatch[];
  /** The asked-for tenant's count when a tenant is given, otherwise every tenant's. */
  charged_without_entry: number;
  /** Every tenant's, whichever tenant is asked for. */
  unpriced_last_24h: { total: number; by_code: Record<string, number> };
  /** Every gateway's, whichever tenant is asked for. */
  usage_loss: {
    since: string;
    dropped_total: number;
    unconfirmed_total: number;
    instances: InstanceDrops[];
  };
}

/** Reconciliation: whether the ledger matches what moved it, and the usage the gateways report dropped. */
export const reconciliation = {
  /** `404` when `tenantId` names no tenant; `503` when the check outran its time bound. */
  async get(tenantId?: string): Promise<Reconciliation> {
    const query = tenantId ? `?tenant_id=${seg(tenantId)}` : "";
    return (await call<Reconciliation>("GET", `/admin/billing/reconciliation${query}`)).data;
  },
};

// ── pricing: baselines ──────────────────────────────────────────────────────

/** Where a baseline's card came from: the built-in catalogue, an attested third-party dataset, or an operator's own entry. */
export type BaselineSource =
  | {
      source_kind: "catalogue";
      source: {
        catalogue_version: string;
        entry_key: string;
        entry_effective_from: string | null;
        source_url: string;
        as_of: string;
      };
    }
  | {
      source_kind: "third_party";
      source: {
        dataset: string;
        dataset_ref: string;
        pricing_url: string | null;
        prices_checked: string | null;
        import_id: string;
        proposal_id: string;
        attested_by: string;
        attested_at: string;
      };
    }
  | { source_kind: "manual"; source: { note: string } };

/** One baseline revision: the official USD list price of a model, from `effective_from`, and where it came from. */
export interface BaselineRevision {
  revision_id: number;
  engine_version: number;
  model_id: string;
  effective_from: string;
  /** As stored: a card this engine cannot read is still listed. */
  card: RateCard;
  source_kind: BaselineSource["source_kind"];
  source: Record<string, unknown>;
  /** `null` when the control plane itself wrote it. */
  created_by: { id: string; email: string } | null;
  created_at: string;
}

/** The catalogue's word on a model's baseline, when it says something else now. */
export interface CatalogueUpdate {
  catalogue_version: string;
  entry_key: string;
  entries: { effective_from: string | null; card: RateCard }[];
}

/** One model's baselines: in force now, scheduled after it, and the catalogue's update to them. */
export interface BaselineRow {
  model_id: string;
  name: string;
  current: BaselineRevision | null;
  upcoming: BaselineRevision[];
  catalogue_update: CatalogueUpdate | null;
}

/** What `POST /admin/pricing/baselines` takes: a card with its `note`, or a `catalogue_key`, never both. */
export interface NewBaseline {
  model_id: string;
  effective_from: string;
  card?: RateCard;
  catalogue_key?: string;
  note?: string;
}

export type BaselineCreated = {
  revision: BaselineRevision;
  later_revisions: BaselineRevision[];
  warnings: PriceWarning[];
};

export const baselines = {
  /** Every live model's baseline, the models without one first. */
  async list(): Promise<BaselineRow[]> {
    return (await call<{ items: BaselineRow[] }>("GET", "/admin/pricing/baselines")).data.items;
  },
  async create(body: NewBaseline): Promise<BaselineCreated> {
    return (await call<BaselineCreated>("POST", "/admin/pricing/baselines", body)).data;
  },
};

// ── pricing: imports ────────────────────────────────────────────────────────

/** Why a proposal cannot be confirmed as it stands; a code this build does not know is shown as it came. */
export type ProposalFlag = "bad_number" | "unsupported_field" | "unsupported_variant" | "time_of_day" | "invalid_card";

/** One upload of the public price dataset, and what it proposed. */
export interface ImportBatch {
  id: string;
  dataset: string;
  dataset_ref: string;
  /** `open`, or `superseded` once a newer upload took over. */
  status: string;
  proposals: number;
  flagged: number;
  created_by: { id: string; email: string };
  created_at: string;
}

/** The catalogue's entry for a proposal's own provider and model, at the proposal's start. */
export interface CatalogueEntryView {
  key: string;
  card: RateCard;
  source_url: string;
  as_of: string;
}

/** One dataset entry at one date: a card to confirm, or the reasons it made none. */
export interface Proposal {
  id: string;
  batch_id: string;
  provider: string;
  model: string;
  /** `null`: in effect at upload. */
  effective_from: string | null;
  /** The card it comes to, when its prices made one. */
  card: RateCard | null;
  flags: ProposalFlag[];
  pricing_url: string | null;
  prices_checked: string | null;
  /** A live direct model whose provider and model name are the entry's. */
  suggested_model_id: string | null;
  /** `pending`, `confirmed`, `rejected` or `superseded`. */
  status: string;
  decided_by: string | null;
  decided_at: string | null;
  reject_reason: string | null;
  baseline_revision_id: number | null;
  current_baseline: BaselineRevision | null;
  catalogue_entry: CatalogueEntryView | null;
  /** Whether the card equals the current baseline's: nothing to confirm. */
  same_as_current: boolean;
}

/** What `POST .../confirm` takes: the model it prices, its start, and the operator's attestation. */
export interface ConfirmProposal {
  model_id: string;
  effective_from: string;
  attested: true;
}

/**
 * Posts the dataset as the file's own text, spliced in as the `data` member
 * rather than as a JSON string of it: the importer reads each price's decimal
 * text, and `JSON.parse` followed by `JSON.stringify` would rewrite
 * `1.5e-05` as `0.000015`, turning a refused price into a readable one. The
 * body is assembled here for that reason, and `dataset_ref` is escaped as a
 * JSON string.
 */
async function callWithDataset<T>(path: string, datasetRef: string, data: string): Promise<T> {
  const body = `{"dataset_ref":${JSON.stringify(datasetRef)},"data":${data}}`;
  let r: Response;
  try {
    r = await fetch(path, {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body,
    });
  } catch {
    throw new ApiError("无法连接控制面", 0, "network", [], [], null);
  }
  // `readAnswer`'s `version` is the upload's `ETag`, which the route does not
  // send and its caller does not read.
  return (await readAnswer<T>(r)).data;
}

export const imports = {
  /** The newest 20 uploads, superseded ones included. */
  async list(): Promise<ImportBatch[]> {
    return (await call<{ items: ImportBatch[] }>("GET", "/admin/pricing/imports")).data.items;
  },
  /**
   * One batch with its proposals. `all` lists a pending proposal whose card
   * equals its model's current baseline too; without it, that one is left out.
   */
  async detail(id: string, all: boolean): Promise<ImportBatch & { proposals: Proposal[] }> {
    return (await call<ImportBatch & { proposals: Proposal[] }>("GET", `/admin/pricing/imports/${seg(id)}${all ? "?all=true" : ""}`))
      .data;
  },
  /** Uploads the dataset's own text under its revision. */
  async upload(dataset_ref: string, data: string): Promise<ImportBatch> {
    return callWithDataset<ImportBatch>("/admin/pricing/imports", dataset_ref, data);
  },
  /** Confirms one proposal: writes its card as a third-party baseline. */
  async confirm(id: string, pid: string, body: ConfirmProposal): Promise<BaselineRevision> {
    return (await call<BaselineRevision>("POST", `/admin/pricing/imports/${seg(id)}/proposals/${seg(pid)}/confirm`, body)).data;
  },
  /** Rejects one proposal, with the reason as the operator typed it. */
  async reject(id: string, pid: string, reason: string): Promise<Proposal> {
    return (await call<Proposal>("POST", `/admin/pricing/imports/${seg(id)}/proposals/${seg(pid)}/reject`, { reason })).data;
  },
};

// ── provider groups and their rates ─────────────────────────────────────────

/** One rate revision of a group: its default (`model_id` null) or a model's override. */
export interface GroupRateRevision {
  revision_id: number;
  engine_version: number;
  group_id: string;
  model_id: string | null;
  effective_from: string;
  /** The decimal string it was typed as; `null` on a model row that ends its override. */
  cny_per_usd: string | null;
  note: string;
  created_by: { id: string; email: string } | null;
  created_at: string;
}

/** A rate's revisions in force now and scheduled after it, by ascending start. */
export interface RatePlan {
  current: GroupRateRevision | null;
  upcoming: GroupRateRevision[];
}

/** One live provider group as the API answers it. */
export interface Group {
  id: string;
  /** The routing name: immutable, operator-only. */
  name: string;
  display_name: string;
  description: string;
  /** The designated default group: the one an upgrade gives every internal key still without a group. */
  is_default: boolean;
  default_rate: RatePlan;
  tenants: number;
  defaults: number;
  keys: number;
  /** The live upstream accounts that list it. */
  provider_keys: { id: string; display_name: string }[];
  retired_at: string | null;
}

/** Every model a group can price, with its rate plan. */
export interface GroupRates {
  group: Group;
  default: RatePlan;
  models: { model_id: string; name: string; rate: RatePlan }[];
}

/** What a group charges for one model: its rate in force and, with a baseline, the ¥/M that follows. */
export interface GroupModelPrice {
  group_id: string;
  display_name: string;
  cny_per_usd: string | null;
  /** `model` when the model's own override priced it, `default` when the group's did, `null` when neither is in force. */
  source: "model" | "default" | null;
  input_per_million_cny?: string;
  output_per_million_cny?: string;
}

/** One model's baseline and every live group's price for it. */
export interface ModelGroupPriceRow {
  model_id: string;
  name: string;
  baseline: { current: BaselineRevision | null; upcoming: BaselineRevision[] };
  groups: GroupModelPrice[];
}

export const groups = {
  /** Every live group, by routing name. */
  async list(): Promise<Group[]> {
    return (await call<{ items: Group[] }>("GET", "/admin/provider-groups")).data.items;
  },
  /** Operator and up. The routing name must match `[a-z0-9][a-z0-9_-]{0,63}` and is unique for good. */
  async create(body: {
    name: string;
    display_name: string;
    description?: string;
    default_cny_per_usd: string;
    effective_from?: string;
  }): Promise<Group> {
    return (await call<Group>("POST", "/admin/provider-groups", body)).data;
  },
  /** What tenants see. A field left out stays as it is. */
  async update(id: string, changes: { display_name?: string; description?: string }): Promise<Group> {
    return (await call<Group>("PATCH", `/admin/provider-groups/${seg(id)}`, changes)).data;
  },
  /** `409 group_in_use` with `counts` while anything still names it. */
  async retire(id: string): Promise<Group> {
    return (await call<Group>("POST", `/admin/provider-groups/${seg(id)}/retire`, {})).data;
  },
  /** Moves the designated default to this group; `409 group_retired` for a retired one. */
  async makeDefault(id: string): Promise<Group> {
    return (await call<Group>("POST", `/admin/provider-groups/${seg(id)}/make-default`, {})).data;
  },
  /** Every live model's rate in the group, by model name. */
  async rates(id: string): Promise<GroupRates> {
    return (await call<GroupRates>("GET", `/admin/provider-groups/${seg(id)}/rates`)).data;
  },
  /**
   * Writes a rate from `effective_from`: a model's override, or the group's
   * default (`model_id` left out). A model row with no `cny_per_usd` ends that
   * model's override; a default row with none is `422 /cny_per_usd`.
   */
  async writeRate(
    id: string,
    body: { model_id?: string | null; cny_per_usd?: string | null; effective_from: string; note?: string },
  ): Promise<{ revision: GroupRateRevision; warnings: PriceWarning[] }> {
    return (await call<{ revision: GroupRateRevision; warnings: PriceWarning[] }>("POST", `/admin/provider-groups/${seg(id)}/rates`, body))
      .data;
  },
};

// ── pricing: each model's price in every group, and the realised margin ─────

/**
 * Every live model's baseline and each live group's price for it
 * (`GET /admin/pricing/models`). A model with no baseline comes first. The
 * ¥ figures are the API's own exact decimal strings, before any tenant
 * discount.
 */
export const groupPrices = {
  async list(): Promise<ModelGroupPriceRow[]> {
    return (await call<{ items: ModelGroupPriceRow[] }>("GET", "/admin/pricing/models")).data.items;
  },
};

/** What one (group, model) charged against what its upstream cost. */
export interface MarginRow {
  group: { id: string; display_name: string };
  model: { id: string | null; name: string };
  charged_micro_cny: number;
  cost_micro_usd: number;
  /** CNY charged per USD of cost, four decimal places; `null` when the cost is not known. */
  realised_cny_per_usd: string | null;
  rows: number;
  rows_without_cost: number;
  /** The part of `charged_micro_cny` from rows with no known cost, which the realised rate leaves out. */
  charged_without_cost_micro_cny: number;
}

/** The realised rate: the CNY a group charged per USD its upstreams cost. */
export const margin = {
  /** Both ends are required; rows outside them are left out. */
  async list(q: { from: string; to: string; group_id?: string; model_id?: string }): Promise<MarginRow[]> {
    const params = new URLSearchParams({ from: q.from, to: q.to });
    if (q.group_id) params.set("group_id", q.group_id);
    if (q.model_id) params.set("model_id", q.model_id);
    return (await call<{ items: MarginRow[] }>("GET", `/admin/pricing/margin?${params}`)).data.items;
  },
};

// ── public models, and the accounts that serve them ─────────────────────────

/** One account that serves a public model, as `served_by` lists it. */
export interface ServedBy {
  provider_key_id: string;
  /** The account's name, as its own document gives it. */
  display_name: string;
  /** The routing names of the groups the account lists. */
  groups: string[];
  /** The upstream name the account calls; `null` means the public name itself. */
  upstream_model: string | null;
  priority: number;
  weight: number;
  enabled: boolean;
}

/**
 * A public model: the name a tenant sends. Its `id` is the generated routing
 * document's id, minted once, so a rename is a new public model.
 */
export interface PublicModel {
  id: string;
  name: string;
  enabled: boolean;
  served_by: ServedBy[];
  /** Live tenants whose grants name it. */
  tenants: number;
  /** Live keys whose model selection names it. */
  keys: number;
  created_at: string;
  retired_at: string | null;
}

/**
 * One public model as an upstream account's checklist shows it: every live
 * public model, served or not. `served` says whether this account has a
 * channel row for it; `enabled` is that row's own flag.
 */
export interface ChannelModel {
  public_model_id: string;
  public_model_name: string;
  upstream_model: string | null;
  priority: number;
  weight: number;
  enabled: boolean;
  served: boolean;
}

/**
 * The upstream account's own model list, for the pickers. Read-only: nothing
 * it returns becomes a price or a document without an operator's save.
 */
export interface UpstreamModelList {
  items: { id: string }[];
  fetched_at: string;
  truncated: boolean;
  /** What an operator should know about the answer, already in Chinese. */
  warnings: string[];
}

/** What a channel-model write answers. */
export interface ChannelModelSaved extends ChannelModel {
  warnings?: string[];
}

/** What a bulk apply did with one upstream model. */
export interface ChannelBulkItem {
  upstream_model: string;
  /** The public model it is mapped under: the reused or created one, or the one that already mapped it. */
  public_model_id: string;
  public_model_name: string;
  /** `created` a new public model, `added` to an existing one, `unchanged` already mapped by this account. */
  outcome: "created" | "added" | "unchanged";
}

export const publicModels = {
  /** Every public model, live and retired, by name. */
  async list(): Promise<PublicModel[]> {
    return (await call<{ items: PublicModel[] }>("GET", "/admin/public-models")).data.items;
  },
  /** `422 /name` when a live model of any kind already holds the name; never adopts it. */
  async create(name: string): Promise<PublicModel> {
    return (await call<PublicModel>("POST", "/admin/public-models", { name })).data;
  },
  async get(id: string): Promise<PublicModel> {
    return (await call<PublicModel>("GET", `/admin/public-models/${seg(id)}`)).data;
  },
  /** `409 public_model_in_use` with `counts {tenants, keys, channels}` while anything live names it. */
  async retire(id: string): Promise<PublicModel> {
    return (await call<PublicModel>("POST", `/admin/public-models/${seg(id)}/retire`, {})).data;
  },
};

export const channelModels = {
  /** Every live public model, with this account's row for each. */
  async list(providerKeyId: string): Promise<ChannelModel[]> {
    return (await call<{ items: ChannelModel[] }>("GET", `/admin/provider-keys/${seg(providerKeyId)}/channel-models`))
      .data.items;
  },
  /** Creates or updates the row, and regenerates every document it implies, in one transaction. */
  async put(
    providerKeyId: string,
    publicModelId: string,
    body: { upstream_model?: string | null; priority?: number; weight?: number; enabled?: boolean },
  ): Promise<ChannelModelSaved> {
    return (
      await call<ChannelModelSaved>(
        "PUT",
        `/admin/provider-keys/${seg(providerKeyId)}/channel-models/${seg(publicModelId)}`,
        body,
      )
    ).data;
  },
  /** `204`, after regenerating the documents the removal implies. */
  async remove(providerKeyId: string, publicModelId: string): Promise<void> {
    await call<null>("DELETE", `/admin/provider-keys/${seg(providerKeyId)}/channel-models/${seg(publicModelId)}`);
  },
  /**
   * Maps upstream models of this account to the public models of the same
   * names, creating the ones that do not exist, in one transaction. All or
   * nothing: `422` naming each refused model at `/upstream_models/<index>`.
   */
  async bulk(providerKeyId: string, upstreamModels: string[]): Promise<ChannelBulkItem[]> {
    return (
      await call<{ items: ChannelBulkItem[] }>("POST", `/admin/provider-keys/${seg(providerKeyId)}/channel-models`, {
        upstream_models: upstreamModels,
      })
    ).data.items;
  },
};

/** The upstream account's own model list: operator-only, cached 60 s, writing nothing. */
export const upstreamModels = {
  async list(providerKeyId: string): Promise<UpstreamModelList> {
    return (await call<UpstreamModelList>("GET", `/admin/provider-keys/${seg(providerKeyId)}/upstream-models`)).data;
  },
};
