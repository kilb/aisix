import type { ApiError, Detail } from "./api";

/**
 * The Dashboard's own text for each error code it knows. A code missing
 * here (a newer control plane's) falls back to the backend's message.
 */
const TEXT: Record<string, string> = {
  invalid_credentials: "邮箱或口令不正确",
  unauthenticated: "登录已过期，请重新登录",
  forbidden: "当前账号没有执行此操作的权限",
  not_found: "资源不存在或已被删除",
  unknown_kind: "不支持的资源类型",
  validation_failed: "提交的内容未通过校验",
  gateway_too_old: "有在线网关的版本过旧，不认识这项配置；请先升级网关",
  name_taken: "名称已被使用",
  email_taken: "该邮箱已被使用",
  in_use: "该资源仍被其他资源引用，无法删除",
  managed_resource: "由租户与密钥管理生成，在对应页面修改",
  last_admin: "至少要保留一个可用的管理员",
  last_owner: "租户至少要保留一位所有者",
  tenant_state: "租户当前的状态不允许此操作",
  tenant_not_active: "租户尚未开通或已停用",
  instance_limit: "网关实例数已达上限",
  version_mismatch: "该资源已被他人修改。重新载入后再编辑。",
  if_match_required: "缺少版本号，请重新载入后再保存",
  budget_exists: "该租户已有同类型、同周期的组织额度，请直接修改限额",
  budget_limit: "额度数量已达上限",
  budget_managed: "该额度由租户设置，运维无法修改",
  topup_state: "该申请已被处理，请刷新",
  busy: "操作过于频繁，请稍后再试",
  payload_too_large: "提交的内容过大（上限 1 MiB）",
  internal: "控制面内部错误，请稍后重试",
  unavailable: "控制面暂时不可用，请稍后重试",
  stale_preview: "预览之后价格或数据有变化，请重新预览",
  run_state: "这次重新计价已经执行过，无法再次执行",
  proposal_state: "该提议已被处理，请刷新",
  group_in_use: "该分组仍在使用中，无法停用",
  group_retired: "该分组已停用，不能再改名或设为默认分组",
  // The upstream's own model list: each refusal says what to do instead, and
  // every one of them leaves free text as the way forward.
  upstream_models_unsupported: "该类型的上游不提供模型列表，请直接填写模型名称",
  upstream_unreachable: "无法连接该上游获取模型列表，请直接填写模型名称",
  upstream_timeout: "获取该上游的模型列表超时，请直接填写模型名称",
  upstream_bad_response: "该上游的模型列表无法读取，请直接填写模型名称",
  // The price library: its fetch from the dataset's address, and a sync
  // whose preview moved before it was confirmed.
  library_fetch_failed: "无法获取价格库数据",
  library_fetch_timeout: "获取价格库数据超时",
  library_stale: "价格库或模型已变化，请刷新预览后重试",
  // Public models and the accounts that serve them.
  public_model_in_use: "该公共模型仍在使用中，无法停用",
  public_model_retired: "该公共模型已停用",
};

export function messageFor(e: ApiError): string {
  return TEXT[e.code] ?? e.message;
}

/** The messages of the refusal details at a path `at` accepts, joined; null when there are none. */
export function problemAt(details: Detail[], at: (path: string) => boolean): string | null {
  const messages = details.filter((d) => at(d.path)).map((d) => d.message);
  return messages.length > 0 ? messages.join("；") : null;
}

/** The Dashboard's text for each price-write warning code it knows; any other shows the backend's message. */
const WARNING_TEXT: Record<string, string> = {
  time_of_day_price: "该目录价是非高峰价，高峰时段请用加价覆盖",
  superseded_later: "已有一条更晚生效的价格，将从该时间起取代这次写入的价格",
};

export function warningText(code: string, message: string): string {
  return WARNING_TEXT[code] ?? message;
}
