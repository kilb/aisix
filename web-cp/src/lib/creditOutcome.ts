import type { CreditedTopup } from "./api";
import { isCnyAmount } from "./money.ts";

export function isUnknownOutcome(e: unknown): boolean {
  const answer = typeof e === "object" && e !== null ? (e as { status?: unknown; code?: unknown }) : {};
  // Only the control plane's own 4xx answer proves nothing was stored. Anything else (a network failure,
  // a 5xx, a 2xx whose body was not JSON, a proxy's 4xx page) leaves it unknown whether the request landed.
  const status = typeof answer.status === "number" ? answer.status : 0;
  return !(status >= 400 && status < 500 && typeof answer.code === "string" && answer.code !== "bad_response");
}

export type CreditOutcome<T> = { kind: "done"; value: T } | { kind: "refused"; error: unknown } | { kind: "unknown"; error: unknown };

/** Sends the credit once and classifies how it ended: done, a definite refusal, or an unknown result. */
export async function sendCredit<T>(post: () => Promise<T>): Promise<CreditOutcome<T>> {
  try {
    return { kind: "done", value: await post() };
  } catch (error) {
    return isUnknownOutcome(error) ? { kind: "unknown", error } : { kind: "refused", error };
  }
}

export type CreditLock = "pending" | "unknown";

export interface CreditRequest {
  operation_id: string;
  amount_cny: string;
  reason: string;
  reference?: string;
}

type CreditInput = Omit<CreditRequest, "operation_id"> & { operation_id?: string };

/** 只有服务端确认了这笔操作，表单才能丢弃浏览器中保存的原请求。 */
export function matchesCreditReceipt(value: unknown, tenantId: string, sent: CreditInput): value is CreditedTopup {
  if (!value || typeof value !== "object" || !isCnyAmount(sent.amount_cny, { allowZero: false })) return false;
  const row = value as Record<string, unknown>;
  const [yuan, fen = ""] = sent.amount_cny.split(".");
  const microCny = Number(yuan) * 1_000_000 + Number(fen.padEnd(2, "0")) * 10_000;
  return typeof row.id === "string" && /^[\da-f]{8}-(?:[\da-f]{4}-){3}[\da-f]{12}$/i.test(row.id)
    && row.tenant_id === tenantId && row.operation_id === sent.operation_id
    && row.status === "approved" && row.origin === "operator"
    && row.amount_micro_cny === microCny && row.credit_reason === sent.reason
    && row.reference === (sent.reference ?? "");
}

type TabStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** 在调用前保存原请求；刷新后上次的结果未知，只能按原编号重试。 */
export function createCreditLocks(storage: () => TabStorage = () => window.sessionStorage) {
  const locks = new Map<string, CreditLock>();
  const refusals = new Map<string, string>();
  const listeners = new Set<() => void>();
  const changed = () => {
    for (const listener of listeners) listener();
  };
  const key = (tenant: string) => `aisix.billing.credit.${tenant}`;
  const saved = (tenant: string): string | null => {
    try {
      return storage().getItem(key(tenant));
    } catch (error) {
      if (!(error instanceof DOMException)) throw error;
      // 禁用标签页存储时表单仍可显示；提交前的持久化会报告错误。
      return null;
    }
  };
  const get = (tenant: string): CreditLock | null => locks.get(tenant) ?? (saved(tenant) !== null ? "unknown" : null);
  function request(tenant: string): CreditRequest | null {
    const raw = saved(tenant);
    if (raw === null) return null;
    try {
      const body = JSON.parse(raw);
      if (
        typeof body?.operation_id !== "string" || !body.operation_id ||
        typeof body.amount_cny !== "string" || typeof body.reason !== "string" ||
        (body.reference !== undefined && typeof body.reference !== "string")
      ) return null;
      return body;
    } catch (error) {
      if (!(error instanceof SyntaxError)) throw error;
      return null;
    }
  }
  function clear(tenant: string) {
    try {
      storage().removeItem(key(tenant));
    } catch (error) {
      if (!(error instanceof DOMException)) throw error;
      // 写回执后标签页存储可能变成只读；残留记录仍以原操作编号恢复。
    }
    locks.delete(tenant);
  }
  return {
    get,
    request,
    acquire(tenant: string, body: CreditRequest): boolean {
      if (locks.has(tenant)) return false;
      const tab = storage();
      if (tab.getItem(key(tenant)) !== null) return false;
      tab.setItem(key(tenant), JSON.stringify(body));
      locks.set(tenant, "pending");
      refusals.delete(tenant);
      changed();
      return true;
    },
    retry(tenant: string): CreditRequest | null {
      if (get(tenant) !== "unknown") return null;
      const original = request(tenant);
      if (original === null) return null;
      locks.set(tenant, "pending");
      changed();
      return original;
    },
    settle(tenant: string, outcome: CreditOutcome<unknown>["kind"], refused?: string): void {
      if (outcome === "unknown") locks.set(tenant, "unknown");
      else clear(tenant);
      if (outcome === "refused" && refused !== undefined) refusals.set(tenant, refused);
      changed();
    },
    refusal(tenant: string): string | null {
      return refusals.get(tenant) ?? null;
    },
    release(tenant: string): void {
      if (get(tenant) !== "unknown") return;
      clear(tenant);
      changed();
    },
    subscribe(listener: () => void): () => void {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

/** The page's own locks, shared by every credit form it mounts. */
export const creditLocks = createCreditLocks();
