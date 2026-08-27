/** 后端接口薄封装。会话是 HttpOnly cookie，所以每个请求都要带凭据。 */

import { EXPECTED_API_CONTRACT } from "./contract";

export type Json = Record<string, unknown>;

/** 资源文档 + 它的磁盘版本。两者必须成对流动 —— 见 `saveDoc`。 */
export interface LoadedDoc {
  doc: Json;
  raw: string;
  version: string;
}

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    /** 版本冲突（409）。调用方据此提示「重新载入」而不是「你写错了」。 */
    readonly stale = false,
  ) {
    super(message);
  }
}

async function req(path: string, init?: RequestInit): Promise<Response> {
  return fetch(path, { credentials: "same-origin", ...init });
}

async function errorOf(r: Response): Promise<string> {
  // 传输层失败或 nginx 502 时 r.json() 会抛；没有这层 catch，被拒的编辑
  // 会留在内存里，下一次保存把它连同新改动一起再提交一遍。
  try {
    const j = (await r.json()) as { error?: string };
    return j.error ?? `HTTP ${r.status}`;
  } catch {
    return `服务端返回了非 JSON 响应（HTTP ${r.status}）`;
  }
}

export async function login(password: string): Promise<void> {
  const r = await req("/api/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ password }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
}

export async function logout(): Promise<void> {
  await req("/api/logout", { method: "POST" });
}

export interface Session {
  authed: boolean;
  /** 后端自报的契约版本。旧后端不带这个字段，视为 0。 */
  apiContract: number;
}

export async function session(): Promise<Session> {
  const r = await req("/api/session");
  if (!r.ok) return { authed: false, apiContract: 0 };
  const j = (await r.json()) as { authed?: boolean; api_contract?: number };
  return { authed: j.authed === true, apiContract: j.api_contract ?? 0 };
}

export async function resources(): Promise<Json> {
  const r = await req("/api/resources");
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
  return (await r.json()) as Json;
}

export async function loadDoc(): Promise<LoadedDoc> {
  const r = await req("/api/file");
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
  const j = (await r.json()) as { doc?: Json; raw?: string; version?: string };
  return {
    doc: j.doc ?? { _format_version: "1" },
    raw: j.raw ?? "",
    // 缺 version 时给空串而不是编一个：`saveDoc` 会因此拒绝保存，
    // 那比带着一个不存在的版本去写要安全。
    version: j.version ?? "",
  };
}

/**
 * 保存整份文档。`baseVersion` 是本次编辑所基于的磁盘版本，服务端在写锁内
 * 重读比对，不一致返回 409。
 *
 * 参数是必填的，不是可选的：控制台的写是「整份读-改-写」，漏带版本就等于
 * 允许后保存的那份静默盖掉先保存的改动 —— 被盖掉的如果是一次密钥吊销，
 * 那把密钥就又活了。类型层面强制它一起传。
 */
export async function saveDoc(doc: Json, baseVersion: string): Promise<string> {
  if (!baseVersion) {
    throw new ApiError(
      "配置未能读取，拒绝保存：保存会用一份不完整的文档覆盖线上配置。",
      0,
    );
  }
  const r = await req("/api/file", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      doc,
      base_version: baseVersion,
      // 声明契约，服务端据此要求必须带版本。少了这个声明，一个忘记带版本
      // 的界面会被当成脚本放过去。
      client_contract: EXPECTED_API_CONTRACT,
    }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status, r.status === 409);
  return ((await r.json()) as { detail?: string }).detail ?? "";
}

/** 同上，提交的是原始 YAML 文本。一次过期覆盖抹掉的是整份文件。 */
export async function saveRaw(raw: string, baseVersion: string): Promise<string> {
  if (!baseVersion) {
    throw new ApiError(
      "配置未能读取，拒绝保存：编辑框里是上一次成功载入的内容，保存会把此后的改动全部覆盖掉。",
      0,
    );
  }
  const r = await req("/api/file", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      raw,
      base_version: baseVersion,
      client_contract: EXPECTED_API_CONTRACT,
    }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status, r.status === 409);
  return ((await r.json()) as { detail?: string }).detail ?? "";
}

/**
 * 铸一把调用方密钥。服务端返回明文一次，之后只有散列。
 *
 * 明文必须在保存成败之前先呈现给用户：保存失败而明文已经丢掉，就留下
 * 一把没人持有的活密钥。
 */
export async function mintKey(): Promise<{ plaintext: string; key_hash: string }> {
  const r = await req("/api/mint-key", { method: "POST" });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
  return (await r.json()) as { plaintext: string; key_hash: string };
}

/** 一个 Prometheus 结果序列，标签集加数值。 */
export interface PromSample {
  l: Record<string, string>;
  v: number;
}

/** Prometheus 原生 result（瞬时查询是 `value`，区间是 `values`）。 */
interface PromResult {
  metric: Record<string, string>;
  value?: [number, string];
  values?: [number, string][];
}

/** 原始查询。返回 Prometheus 的 result 数组，形状由调用方解释。 */
export async function prom(query: string, rangeHours?: number): Promise<PromResult[]> {
  const r = await req("/api/metrics", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ query, range_hours: rangeHours }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
  const j = (await r.json()) as { status?: string; error?: string; data?: { result?: PromResult[] } };
  if (j.status !== "success") throw new ApiError(j.error ?? "查询失败", r.status);
  return j.data?.result ?? [];
}

/** 瞬时查询，按值降序。 */
export async function promTop(query: string): Promise<PromSample[]> {
  const rows = (await prom(query)).map((r) => ({
    l: r.metric,
    v: Number(r.value?.[1]) || 0,
  }));
  rows.sort((a, b) => b.v - a.v);
  return rows;
}

/** 区间查询，每条序列保留时间点，给趋势图用。 */
export async function promRange(
  query: string,
  rangeHours: number,
): Promise<{ l: Record<string, string>; points: [number, number][] }[]> {
  return (await prom(query, rangeHours)).map((r) => ({
    l: r.metric,
    points: (r.values ?? []).map(([t, v]) => [t, Number(v) || 0] as [number, number]),
  }));
}

export async function logs(apiKeyId: string, limit: number): Promise<Json[]> {
  const r = await req("/api/logs", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ api_key_id: apiKeyId, limit }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
  return ((await r.json()) as { rows?: Json[] }).rows ?? [];
}

export async function upstreamModels(providerKey: string): Promise<string[]> {
  const r = await req("/api/upstream-models", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ provider_key: providerKey }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
  return ((await r.json()) as { models?: string[] }).models ?? [];
}

// ── 自助门户的管理端（经控制台转发，凭据留在服务端）────────────────────

export interface PortalUser {
  user_id: string;
  email: string;
  display_name: string | null;
  disabled: boolean;
  balance_micro_usd: number;
  /** 总额度 —— 迄今给过他的一切之和。下推给网关的就是这个数。 */
  granted_micro_usd: number;
  /** 他自己已经分到各把密钥上的额度之和。 */
  allocated_micro_usd: number;
}

export async function portalUsers(): Promise<{ users: PortalUser[] }> {
  const r = await req("/api/portal/users");
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
  return (await r.json()) as { users: PortalUser[] };
}

/**
 * 把某个用户的总额度**设定**成某个数（绝对值，不是增量）。
 *
 * 跟 {@link portalGrant} 的区别是语义而不是实现：设定回答「他一共有多少」，
 * 发放回答「再给他多少」。日常调额用设定 —— 用增量调额时，管理员得先算出差值，
 * 算错就是白送或误封。
 */
export async function portalSetQuota(
  userId: string,
  microUsd: number,
  note: string | null,
): Promise<void> {
  const r = await req(`/api/portal/users/${encodeURIComponent(userId)}/quota`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ micro_usd: microUsd, note }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
}

export async function portalGrant(
  userId: string,
  microUsd: number,
  note: string | null,
): Promise<void> {
  const r = await req(`/api/portal/users/${encodeURIComponent(userId)}/grant`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ micro_usd: microUsd, note }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
}

export interface PortalTopup {
  id: number;
  user_id: string;
  email: string;
  micro_usd: number;
  note: string | null;
  status: string;
  created_at: string;
}

export async function portalTopups(): Promise<{ topups: PortalTopup[] }> {
  const r = await req("/api/portal/topups");
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
  return (await r.json()) as { topups: PortalTopup[] };
}

/** `decision` 只能是 approve / reject —— 服务端也会再挡一次。 */
export async function portalDecideTopup(
  id: number,
  decision: "approve" | "reject",
  note: string | null,
): Promise<void> {
  const r = await req(`/api/portal/topups/${id}/${decision}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ note }),
  });
  if (!r.ok) throw new ApiError(await errorOf(r), r.status);
}
