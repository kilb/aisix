export interface RealtimeSipBinding {
  route_id: string;
  webhook_secret: string;
  api_key_id: string;
  model_id: string;
  [field: string]: unknown;
}

export interface RealtimeSipDraft {
  enabled: boolean;
  routeId: string;
  webhookSecret: string | undefined;
  apiKeyId: string;
  modelId: string;
}

export type RealtimeSipField = "route_id" | "webhook_secret" | "api_key_id" | "model_id";
export type RealtimeSipErrors = Partial<Record<RealtimeSipField, string>>;

export interface RealtimeSipRequest {
  default_headers?: Record<string, unknown>;
  [field: string]: unknown;
}

const REDACTED = "[REDACTED]";
const RESOURCE_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const ROUTE_ID = /^[A-Za-z0-9_-]{1,64}$/;

// 已保存的凭据只变成占位符，永不进入表单的文本输入。
export function realtimeSipDraft(binding: RealtimeSipBinding | null | undefined): RealtimeSipDraft {
  return {
    enabled: binding != null,
    routeId: binding?.route_id ?? "",
    webhookSecret: binding?.webhook_secret ? REDACTED : "",
    apiKeyId: binding?.api_key_id ?? "",
    modelId: binding?.model_id ?? "",
  };
}

export function realtimeSipErrors(
  draft: RealtimeSipDraft,
  stored: RealtimeSipBinding | null | undefined,
  accountChanged: boolean,
): RealtimeSipErrors {
  if (!draft.enabled) return {};
  const errors: RealtimeSipErrors = {};
  if (!ROUTE_ID.test(draft.routeId.trim())) errors.route_id = "路由标识须为 1–64 个字母、数字、下划线或连字符";
  if (!RESOURCE_ID.test(draft.apiKeyId.trim())) errors.api_key_id = "请填写租户 API 密钥的小写 UUID";
  if (!RESOURCE_ID.test(draft.modelId.trim())) errors.model_id = "请填写此账号直连模型的小写 UUID";
  if (!draft.webhookSecret?.trim()) errors.webhook_secret = "请填写 Webhook 签名密钥";
  else if (draft.webhookSecret === REDACTED) {
    const moved = accountChanged || !stored || draft.routeId.trim() !== stored.route_id
      || draft.apiKeyId.trim() !== stored.api_key_id || draft.modelId.trim() !== stored.model_id;
    if (moved) errors.webhook_secret = "绑定目标或上游账号已变更，请重新填写 Webhook 签名密钥";
  }
  return errors;
}

// 保存的是固定绑定；禁用会移除整个绑定，而不是发送空字段。
export function realtimeSipBody(
  draft: RealtimeSipDraft,
  stored: RealtimeSipBinding | null | undefined,
): RealtimeSipBinding | undefined {
  if (!draft.enabled) return undefined;
  return {
    ...stored,
    route_id: draft.routeId.trim(),
    webhook_secret: draft.webhookSecret ?? "",
    api_key_id: draft.apiKeyId.trim(),
    model_id: draft.modelId.trim(),
  };
}

export function realtimeSipWebhookPath(routeId: string): string | null {
  const route = routeId.trim();
  return ROUTE_ID.test(route) ? `/internal/native/realtime/sip/${route}` : null;
}

// 项目 ID 沿用请求头的凭据保护；已保存的值不进入可见文本。
export function realtimeSipProjectDraft(request: RealtimeSipRequest | null | undefined): string {
  const header = Object.entries(request?.default_headers ?? {})
    .find(([name]) => name.toLowerCase() === "openai-project")?.[1];
  return typeof header === "string" && header !== "" ? REDACTED : "";
}

export function realtimeSipProjectError(enabled: boolean, project: string | undefined): string | null {
  return enabled && !project?.trim() ? "请填写 SIP 来电所属的上游项目 ID" : null;
}

export function realtimeSipProjectBody(
  request: RealtimeSipRequest | null | undefined,
  project: string,
): RealtimeSipRequest | null | undefined {
  if (project === REDACTED) return request;
  const headers = { ...request?.default_headers };
  for (const name of Object.keys(headers)) {
    if (name.toLowerCase() === "openai-project") delete headers[name];
  }
  headers["OpenAI-Project"] = project.trim();
  return { ...request, default_headers: headers };
}

export function isRealtimeSipProjectPath(path: string): boolean {
  const lower = path.toLowerCase();
  return lower === "/request/default_headers/openai-project" || lower === "default header openai-project";
}
