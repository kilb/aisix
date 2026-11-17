import type { Detail, ResourceView, TokenCounting, TokenCountModelMapping, TokenCountTarget } from "./api.ts";

export const TOKEN_COUNT_TARGETS = ["messages_count_tokens", "responses_input_tokens"] as const;
export type TokenCountKey = (typeof TOKEN_COUNT_TARGETS)[number];
export type TokenCountChanges = Partial<Record<TokenCountKey, TokenCountTarget | null>>;

export interface TokenCountAccountDoc {
  display_name?: string;
  adapter?: string | null;
  provider?: string;
  apis?: { messages?: unknown | null; responses?: unknown | null } | null;
}

// 与 aisix_core::token_count_provider_serves 保持一致，只接受直接提供计数协议的账号。
export function tokenCountProviderServes(account: TokenCountAccountDoc, key: TokenCountKey): boolean {
  if (["bedrock", "vertex", "azure-openai"].includes(account.adapter ?? "")) return false;
  if (key === "messages_count_tokens") return account.provider === "anthropic" || account.adapter === "anthropic" || account.apis?.messages != null;
  return account.apis != null ? account.apis.responses != null : account.provider === "openai";
}

export interface TokenCountSourceDoc {
  display_name?: string;
  model_name?: string;
  provider_key_id?: string;
  routing?: unknown;
  ensemble?: unknown;
  semantic?: unknown;
  embedding?: unknown;
}
export type TokenCountSource = ResourceView<TokenCountSourceDoc>;

function direct(doc: TokenCountSourceDoc): boolean {
  return doc.routing == null && doc.ensemble == null && doc.semantic == null && doc.embedding == null && Boolean(doc.provider_key_id && doc.model_name);
}

function wildcard(doc: TokenCountSourceDoc): boolean {
  return Boolean(doc.model_name?.includes("*") || doc.display_name?.includes("*"));
}

// 公共别名背后的内部直连 Model 也必须按自己的稳定 ID 列出。
export function tokenCountSources(models: TokenCountSource[]): TokenCountSource[] {
  return models.filter((model) => direct(model.doc) && !wildcard(model.doc));
}

export function selectTokenCountSource(mapping: TokenCountModelMapping, source: TokenCountSource): TokenCountModelMapping {
  return { ...mapping, model_id: source.id, source_model_name: source.doc.model_name ?? "" };
}

// 只替换用户编辑过的块，保留其他协议与未来字段；移除块即关闭。
export function tokenCountBody(stored: TokenCounting, changes: TokenCountChanges): TokenCounting {
  const doc = { ...stored };
  for (const key of TOKEN_COUNT_TARGETS) {
    if (!(key in changes)) continue;
    const target = changes[key];
    if (target == null) delete doc[key];
    else doc[key] = target;
  }
  return doc;
}

export function tokenCountProblems(doc: TokenCounting, models: TokenCountSource[]): Detail[] {
  const problems: Detail[] = [];
  const sources = new Map(models.map((model) => [model.id, model]));
  for (const key of TOKEN_COUNT_TARGETS) {
    const target = doc[key];
    if (target == null) continue;
    if (!target.provider_key_id) problems.push({ path: `/${key}/provider_key_id`, message: "请选择计数账号" });
    const seen = new Set<string>();
    for (const [index, mapping] of (target.model_mappings ?? []).entries()) {
      const path = `/${key}/model_mappings/${index}`;
      const source = sources.get(mapping.model_id);
      if (!mapping.model_id) problems.push({ path: `${path}/model_id`, message: "请选择源模型" });
      else if (seen.has(mapping.model_id)) problems.push({ path: `${path}/model_id`, message: "同一计数接口不能重复映射源模型" });
      else if (!source) problems.push({ path: `${path}/model_id`, message: "源模型不存在，请重新选择" });
      else if (!direct(source.doc)) problems.push({ path: `${path}/model_id`, message: "请选择实际源直连模型，不能选择嵌入模型或父级路由" });
      else if (wildcard(source.doc)) problems.push({ path: `${path}/model_id`, message: "通配符模型不能配置固定映射" });
      else if (mapping.source_model_name !== source.doc.model_name) problems.push({ path: `${path}/source_model_name`, message: "源上游模型已变化，请重新选择源模型以确认版本" });
      seen.add(mapping.model_id);
      if (!mapping.model_name.trim()) problems.push({ path: `${path}/model_name`, message: "请填写计数上游模型" });
      else if (mapping.model_name.includes("*")) problems.push({ path: `${path}/model_name`, message: "计数上游模型不能使用通配符" });
    }
  }
  return problems;
}
