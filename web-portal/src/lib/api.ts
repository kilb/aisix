/**
 * 门户 API 层。
 *
 * 这里没有、也不该有任何「传一条查询给后端」的函数。租户隔离是端点的形状：
 * 用量与余额只按会话返回本人的数，前端连一个可以夹带 user_id 的参数都拿不到。
 */

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const r = await fetch(path, { credentials: "same-origin", ...init });
  const text = await r.text();
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    throw new Error(`服务端返回的不是 JSON（${r.status}）`);
  }
  if (!r.ok) {
    const msg = (body as { error?: string } | null)?.error;
    throw new Error(msg ?? `请求失败（${r.status}）`);
  }
  return body as T;
}

function post<T>(path: string, data?: unknown): Promise<T> {
  return req<T>(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(data ?? {}),
  });
}

export interface Session {
  authed: boolean;
  user_id?: string;
  email?: string;
  display_name?: string | null;
}

export interface Entry {
  id: number;
  delta_micro_usd: number;
  source: string;
  note: string | null;
}

export interface Balance {
  balance_micro_usd: number;
  entries: Entry[];
}

export interface Usage {
  range: string;
  linked_keys: number;
  disabled_keys: number;
  requests: number | null;
  tokens: number | null;
  spend_micro_usd: number | null;
  note: string | null;
}

export const session = () => req<Session>("/api/session");
export const register = (email: string, password: string) =>
  post<{ user_id: string }>("/api/register", { email, password });
export const login = (email: string, password: string) =>
  post<{ ok: true }>("/api/login", { email, password });
export const logout = () => post<{ ok: true }>("/api/logout");
export interface KeyRow {
  name: string;
  masked_hash: string;
  disabled: boolean;
}

export interface MintedKey {
  /** 明文只在这一次出现。此后任何接口都拿不到。 */
  plaintext: string;
  name: string;
  label: string;
  disabled: boolean;
  note: string | null;
}

export const balance = () => req<Balance>("/api/balance");
export const listKeys = () => req<{ keys: KeyRow[] }>("/api/keys");
export const createKey = (label: string) => post<MintedKey>("/api/keys", { label });
export const revokeKey = (name: string) =>
  req<{ ok: true }>(`/api/keys/${encodeURIComponent(name)}`, { method: "DELETE" });
/** 只有窗口长度是参数。没有、也不能有 user_id。 */
export const usage = (rangeHours: number) =>
  req<Usage>(`/api/usage?range_hours=${rangeHours}`);
