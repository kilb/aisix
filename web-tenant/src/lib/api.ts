/**
 * A thin client for the control plane's tenant API (`/api/*`).
 *
 * No function here takes a tenant id, and none should: every route answers
 * for the session's own tenant, so the client never gets a parameter it
 * could slip another tenant's id into. (The rule of
 * web-portal/src/lib/api.ts:1-6.)
 *
 * The session is an HttpOnly cookie, so every request carries same-origin
 * credentials. Types use the API's own snake_case field names; only
 * `session()` and the calls that answer with a session return a small
 * camelCase wrapper.
 */

/**
 * The API prefix, derived from the build's `base` (copied from
 * web-portal/src/lib/api.ts:8-16): a fixed `/api` could only be deployed at
 * the root, while derived from `BASE_URL` the portal can be mounted under a
 * prefix with the API beside it, and development and e2e stay at the root.
 */
const API = `${import.meta.env.BASE_URL.replace(/\/$/, "")}/api`;

/**
 * The shortest password the control plane accepts, in characters. The form
 * states it and checks it before sending; the control plane checks it again.
 */
export const MIN_PASSWORD_CHARS = 12;

// ── errors ──────────────────────────────────────────────────────────────────

/** One problem in a request body, at a JSON path of it. */
export interface Detail {
  path: string;
  message: string;
}

/**
 * An error answer: the envelope's `error` as the message, its `code`, and
 * the optional `details` and `current_version`. An answer that is not JSON
 * has `code` `bad_response`; a request that never got an answer has `code`
 * `network` and status 0.
 */
export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: string,
    readonly details: Detail[],
    readonly currentVersion: number | null,
  ) {
    super(message);
  }
}

interface Envelope {
  error?: string;
  code?: string;
  details?: Detail[];
  current_version?: number;
}

let sessionEnded: (() => void) | null = null;

/**
 * Registers what to do when the control plane answers `401
 * unauthenticated`: the session ended (it expired, or a password change or
 * a removal elsewhere ended it), so the page it was behind must go. One
 * listener, the app's; `null` clears it.
 */
export function onSessionEnded(listener: (() => void) | null): void {
  sessionEnded = listener;
}

/** The version in an `ETag` (`"12"`, `12` or `W/"12"`), or null. */
function etagVersion(etag: string | null): number | null {
  if (!etag) return null;
  const n = Number(etag.replace(/^W\//, "").replace(/"/g, ""));
  return Number.isInteger(n) ? n : null;
}

/**
 * One request to `path` under the API prefix. `ifMatch` sends `If-Match:
 * "<n>"`, which the versioned routes require on PATCH and DELETE; `version`
 * is the answer's `ETag`.
 */
export async function call<T>(
  method: string,
  path: string,
  body?: unknown,
  ifMatch?: number,
): Promise<{ data: T; version: number | null }> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (ifMatch !== undefined) headers["if-match"] = `"${ifMatch}"`;
  let r: Response;
  try {
    r = await fetch(`${API}${path}`, {
      method,
      credentials: "same-origin",
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError("无法连接平台", 0, "network", [], null);
  }
  const text = await r.text();
  let parsed: unknown = null;
  // A proxy's 502 page or a transport failure is not JSON. Without this the
  // caller would see a parse error instead of the failed request.
  if (text) {
    try {
      parsed = JSON.parse(text);
    } catch {
      throw new ApiError(`服务端返回了非 JSON 响应（HTTP ${r.status}）`, r.status, "bad_response", [], null);
    }
  }
  if (!r.ok) {
    const e = (parsed ?? {}) as Envelope;
    const err = new ApiError(
      e.error ?? `HTTP ${r.status}`,
      r.status,
      e.code ?? "bad_response",
      e.details ?? [],
      e.current_version ?? null,
    );
    if (err.code === "unauthenticated") sessionEnded?.();
    throw err;
  }
  return { data: parsed as T, version: etagVersion(r.headers.get("etag")) };
}

// ── types (the API's field names) ───────────────────────────────────────────

export type MemberRole = "owner" | "admin" | "member" | "billing";

export type TenantStatus = "pending" | "active" | "suspended";

/** The signed-in member, as a session describes them. */
export interface SessionMember {
  id: string;
  email: string;
  display_name: string | null;
  role: MemberRole;
}

/** The member's tenant, as a session describes it. */
export interface SessionTenant {
  id: string;
  name: string;
  status: TenantStatus;
}

export interface Session {
  authed: boolean;
  apiContract: number;
  member: SessionMember | null;
  tenant: SessionTenant | null;
}

/** `GET /api/org`: the caps a key of the tenant may be given, and no more. */
export interface Org {
  id: string;
  name: string;
  status: TenantStatus;
  limits: { key_concurrency_max?: number; key_rpm_max?: number };
}

// ── session ─────────────────────────────────────────────────────────────────

interface SessionBody {
  authed?: boolean;
  api_contract?: number;
  member?: SessionMember;
  tenant?: SessionTenant;
}

function toSession(data: SessionBody): Session {
  return {
    authed: data.authed === true,
    // A backend without the field is older than every contract.
    apiContract: data.api_contract ?? 0,
    member: data.member ?? null,
    tenant: data.tenant ?? null,
  };
}

export async function session(): Promise<Session> {
  return toSession((await call<SessionBody>("GET", "/session")).data);
}

/** Signs up a new organisation, pending approval, with the caller as its owner, and signs in. */
export async function signup(
  org: string,
  email: string,
  password: string,
  displayName?: string,
): Promise<Session> {
  const body = { org_name: org, email, password, display_name: displayName };
  return toSession((await call<SessionBody>("POST", "/signup", body)).data);
}

export async function login(email: string, password: string): Promise<Session> {
  return toSession((await call<SessionBody>("POST", "/login", { email, password })).data);
}

export async function logout(): Promise<void> {
  await call<null>("POST", "/logout");
}

/** Joins the invitation's organisation with a password of one's own, and signs in. */
export async function acceptInvite(token: string, password: string, displayName?: string): Promise<Session> {
  const body = { token, password, display_name: displayName };
  return toSession((await call<SessionBody>("POST", "/invites/accept", body)).data);
}

export async function org(): Promise<Org> {
  return (await call<Org>("GET", "/org")).data;
}

/** Ends the caller's other sessions. */
export async function changePassword(current: string, next: string): Promise<void> {
  await call<null>("POST", "/password", { current_password: current, new_password: next });
}

// ── models, keys and members ────────────────────────────────────────────────

/** A model the organisation is granted, by its platform name only. */
export interface PortalModel {
  id: string;
  name: string;
}

/** A key's own limits; one left unset follows the organisation's per-key cap. */
export interface KeyLimits {
  concurrency?: number;
  rpm?: number;
}

/** `TenantKey`: a key as its organisation sees it. It never holds the plaintext. */
export interface TenantKey {
  id: string;
  label: string;
  prefix: string;
  /** The member the key belongs to; null for a key of the organisation as a whole. */
  owner: { id: string; display_name: string | null } | null;
  /** The models selected from the grants; null follows every grant. */
  model_ids: string[] | null;
  rate_limit: KeyLimits;
  /** The organisation's own switch. */
  disabled: boolean;
  /** The platform's switch, which only the platform turns off again. */
  disabled_by_platform: boolean;
  expires_at: string | null;
  created_at: string;
  version: number;
}

/** `POST /api/keys`. `member_id` left out binds the key to the caller. */
export interface NewKey {
  label: string;
  member_id?: string;
  model_ids?: string[];
  rate_limit?: KeyLimits;
  expires_at?: string;
}

/** `Member`. */
export interface Member {
  id: string;
  email: string;
  display_name: string | null;
  role: MemberRole;
  disabled: boolean;
  created_at: string;
}

/** An invitation still open. `invited_by` is null for one the platform issued. */
export interface Invite {
  id: string;
  email: string;
  role: MemberRole;
  expires_at: string;
  created_at: string;
  invited_by: { id: string; display_name: string | null } | null;
}

/** A new invitation: the token, given this once, and the link when the platform builds one. */
export interface IssuedInvite {
  id: string;
  invite_token: string;
  invite_url?: string;
  expires_at: string;
}

const at = (id: string) => encodeURIComponent(id);

export const models = {
  async list(): Promise<PortalModel[]> {
    return (await call<{ items: PortalModel[] }>("GET", "/models")).data.items;
  },
};

export const keys = {
  /** Every key of the organisation for an owner or an admin; a member's own for a member. */
  async list(): Promise<TenantKey[]> {
    return (await call<{ items: TenantKey[] }>("GET", "/keys")).data.items;
  },
  /** The new key, and its plaintext, which no later answer carries. */
  async create(body: NewKey): Promise<{ key: TenantKey; plaintext: string }> {
    return (await call<{ key: TenantKey; plaintext: string }>("POST", "/keys", body)).data;
  },
  /**
   * The organisation's own switch, at `version`. The body names this one
   * field: an edit that both narrows and widens a key is refused whole while
   * an older gateway is registered, so no request here mixes the two.
   */
  async setDisabled(id: string, disabled: boolean, version: number): Promise<TenantKey> {
    return (await call<TenantKey>("PATCH", `/keys/${at(id)}`, { disabled }, version)).data;
  },
  async revoke(id: string, version: number): Promise<void> {
    await call<null>("DELETE", `/keys/${at(id)}`, undefined, version);
  },
};

export const members = {
  async list(): Promise<Member[]> {
    return (await call<{ items: Member[] }>("GET", "/members")).data.items;
  },
  async setRole(id: string, role: MemberRole): Promise<Member> {
    return (await call<Member>("PATCH", `/members/${at(id)}`, { role })).data;
  },
  /** Removes the member and revokes every key of theirs. */
  async remove(id: string): Promise<void> {
    await call<null>("DELETE", `/members/${at(id)}`);
  },
  async invites(): Promise<Invite[]> {
    return (await call<{ items: Invite[] }>("GET", "/members/invites")).data.items;
  },
  async invite(email: string, role: MemberRole): Promise<IssuedInvite> {
    return (await call<IssuedInvite>("POST", "/members/invites", { email, role })).data;
  },
  async revokeInvite(id: string): Promise<void> {
    await call<null>("DELETE", `/members/invites/${at(id)}`);
  },
};

// ── billing: balance, top-ups, ledger and a key's quota ─────────────────────

/** `GET /api/billing/balance`. `available` is the balance for prepaid, balance plus credit limit for postpaid. */
export interface Balance {
  billing_mode: "prepaid" | "postpaid";
  balance_micro_usd: number;
  credit_limit_micro_usd: number;
  available_micro_usd: number;
  exhausted: boolean;
  pending_topups: number;
}

export type LedgerKind = "usage" | "topup" | "adjustment";

export interface LedgerItem {
  seq: number;
  posted_at: string;
  kind: LedgerKind;
  amount_micro_usd: number;
  balance_after_micro_usd: number;
  event_id: string | null;
  /** Only a usage entry has a model; its `id` is null unless the model is currently granted. */
  model: { id: string | null; name: string } | null;
  topup_id: string | null;
  repricing_run_id: string | null;
}

export interface LedgerSummaryRow {
  period_start: string;
  usage_micro_usd: number;
  topups_micro_usd: number;
  adjustments_micro_usd: number;
  closing_balance_micro_usd: number;
}

/** A top-up: requested by the organisation, or credited by the platform (`origin "operator"`, `requested_by` null). */
export interface Topup {
  id: string;
  origin: "tenant" | "operator";
  amount_micro_usd: number;
  note: string | null;
  reference: string | null;
  status: "pending" | "approved" | "rejected";
  requested_by: { id: string; display_name: string | null } | null;
  requested_at: string;
  decided_at: string | null;
  reject_reason: string | null;
  credit_reason: string | null;
}

/** One quota that applies to a key, with its spend now. */
export interface QuotaFigure {
  id: string;
  scope: "team" | "member" | "api_key";
  set_by: "operator" | "org_admin" | "member";
  kind: "periodic" | "cumulative";
  period: "day" | "week" | "month" | null;
  limit_micro_usd: number;
  spent_micro_usd: number;
  remaining_micro_usd: number;
  period_start: string | null;
  counted_from: string | null;
  exhausted: boolean;
}

export type QuotaLevel = "organization" | "member" | "key";

/** `GET /api/keys/{id}/quota`: every level that applies to one key, and the smallest remaining amount. */
export interface KeyQuota {
  key_id: string;
  as_of: string;
  org: {
    billing_mode: "prepaid" | "postpaid";
    balance_micro_usd: number;
    credit_limit_micro_usd: number;
    available_micro_usd: number;
  } | null;
  quotas: QuotaFigure[];
  effective_remaining_micro_usd: number | null;
  binding_level: QuotaLevel | null;
}

/** A page of the ledger or of the top-ups: `next_before` is null on the last. */
export interface Page<T> {
  items: T[];
  next_before: number | null;
}

/** The query a ledger or summary request takes; a left-out part is not sent. */
function queryOf(q: Record<string, string | number | undefined>): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

export const billing = {
  async balance(): Promise<Balance> {
    return (await call<Balance>("GET", "/billing/balance")).data;
  },
  /** Newest first; `before` is the previous page's `next_before`. `from` and `to` are RFC 3339, half-open. */
  async ledger(q: { from?: string; to?: string; kind?: LedgerKind; limit?: number; before?: number }): Promise<Page<LedgerItem>> {
    return (await call<Page<LedgerItem>>("GET", `/billing/ledger${queryOf(q)}`)).data;
  },
  async ledgerSummary(q: { group: "day" | "month"; from: string; to: string }): Promise<LedgerSummaryRow[]> {
    return (await call<{ items: LedgerSummaryRow[] }>("GET", `/billing/summary${queryOf(q)}`)).data.items;
  },
  async topups(q: { status?: Topup["status"]; limit?: number; before?: number } = {}): Promise<Page<Topup>> {
    return (await call<Page<Topup>>("GET", `/billing/topups${queryOf(q)}`)).data;
  },
  /** `amount_usd` is dollars with at most two decimals. */
  async requestTopup(body: { amount_usd: string; note?: string; reference?: string }): Promise<Topup> {
    return (await call<Topup>("POST", "/billing/topups", body)).data;
  },
  async keyQuota(keyId: string): Promise<KeyQuota> {
    return (await call<KeyQuota>("GET", `/keys/${at(keyId)}/quota`)).data;
  },
};
