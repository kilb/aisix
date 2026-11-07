import type { ApiError } from "./api";

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
  busy: "操作过于频繁，请稍后再试",
  payload_too_large: "提交的内容过大（上限 1 MiB）",
  internal: "控制面内部错误，请稍后重试",
  unavailable: "控制面暂时不可用，请稍后重试",
};

export function messageFor(e: ApiError): string {
  return TEXT[e.code] ?? e.message;
}
