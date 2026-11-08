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

/** One sell price revision. A tenant override ended by a later revision has `card` null. */
export interface SellRevision {
  revision_id: number;
  engine_version: number;
  model_id: string;
  tenant_id: string | null;
  effective_from: string;
  card: RateCard | null;
  source: string;
  created_by: string | null;
  created_at: string;
}

/** Something to know about a price that was written all the same. */
export interface PriceWarning {
  code: string;
  message: string;
  revision_id?: number;
  effective_from?: string;
}

/** The answer to a sell price write: the revision starting as asked, what to know about it, and the later revisions an import also wrote. */
export type SellCreated = SellRevision & { warnings: PriceWarning[]; later_revisions: SellRevision[] };

export interface CatalogueEntry {
  key: string;
  aliases: string[];
  source_url: string | null;
  as_of: string | null;
  effective_from: string | null;
  notes: string | null;
  card: RateCard;
}

/** One model's platform price: in force now, and scheduled by ascending start. */
export interface PriceRow {
  model_id: string;
  name: string;
  current: SellRevision | null;
  upcoming: SellRevision[];
}

/** What `POST /admin/pricing/sell` takes: exactly one of `card` and `catalogue_key`. */
export interface NewSellPrice {
  model_id: string;
  tenant_id?: string;
  effective_from: string;
  card?: RateCard | null;
  catalogue_key?: string;
  markup_bps?: number;
}

export const pricing = {
  async catalogue(): Promise<{ version: string | number; items: CatalogueEntry[] }> {
    return (await call<{ version: string | number; items: CatalogueEntry[] }>("GET", "/admin/pricing/catalogue")).data;
  },
  /** Sell revisions, newest first; `before` is the previous page's `next_before`. */
  async listSell(q: {
    model_id?: string;
    tenant_id?: string;
    limit?: number;
    before?: number;
  }): Promise<{ items: SellRevision[]; next_before: number | null }> {
    const params = new URLSearchParams();
    if (q.model_id) params.set("model_id", q.model_id);
    if (q.tenant_id) params.set("tenant_id", q.tenant_id);
    if (q.limit !== undefined) params.set("limit", String(q.limit));
    if (q.before !== undefined) params.set("before", String(q.before));
    const query = params.toString();
    return (
      await call<{ items: SellRevision[]; next_before: number | null }>(
        "GET",
        `/admin/pricing/sell${query ? `?${query}` : ""}`,
      )
    ).data;
  },
  async createSell(body: NewSellPrice): Promise<SellCreated> {
    return (await call<SellCreated>("POST", "/admin/pricing/sell", body)).data;
  },
  /** Every live model's platform price, the unpriced ones first. */
  async models(): Promise<PriceRow[]> {
    return (await call<{ items: PriceRow[] }>("GET", "/admin/pricing/models")).data.items;
  },
};
