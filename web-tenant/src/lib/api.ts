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
    throw new ApiError(
      e.error ?? `HTTP ${r.status}`,
      r.status,
      e.code ?? "bad_response",
      e.details ?? [],
      e.current_version ?? null,
    );
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
