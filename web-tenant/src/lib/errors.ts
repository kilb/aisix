import type { ApiError } from "./api";

/**
 * The portal's own text for each error code it knows. A code missing here
 * (a newer control plane's) falls back to the backend's message.
 */
const TEXT: Record<string, string> = {
  // Login answers this for a wrong password and an unknown email alike.
  invalid_credentials: "邮箱或口令不正确",
  unauthenticated: "登录已过期，请重新登录",
  forbidden: "当前账号没有执行此操作的权限",
  not_found: "资源不存在或已被删除",
  validation_failed: "提交的内容未通过校验",
  // Sign-up answers this when the email already belongs to an account. The
  // text does not say so: a page that tells whoever types an address whether
  // it has an account is an oracle for it. (Invitation acceptance words it
  // its own way, in Gate.tsx.)
  email_taken: "无法完成注册，请检查填写的信息",
  last_owner: "组织至少要有一个所有者",
  tenant_not_active: "组织尚未通过审核或已被停用",
  version_mismatch: "该资源已被他人修改。重新载入后再编辑。",
  if_match_required: "缺少版本号，请重新载入后再保存",
  topup_limit: "待审批的充值申请已达上限，请等待处理后再提交",
  busy: "操作过于频繁，请稍后再试",
  payload_too_large: "提交的内容过大（上限 1 MiB）",
  internal: "平台内部错误，请稍后重试",
  unavailable: "平台暂时不可用，请稍后重试",
};

export function messageFor(e: ApiError): string {
  return TEXT[e.code] ?? e.message;
}

/**
 * `messageFor`, except that a refused body lists what was wrong with it:
 * the backend's own text for each field, since the form has no better one.
 */
export function formMessageFor(e: ApiError): string {
  if (e.code === "validation_failed" && e.details.length > 0) {
    return e.details.map((d) => d.message).join("；");
  }
  return messageFor(e);
}
