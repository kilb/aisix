import type { ChannelBulkItem } from "./api";

/**
 * The 获取上游模型 list of an upstream account: what each id the account
 * listed would do if it were mapped, read from the account's own rows and the
 * public models, and the shape a public model's name must have.
 *
 * Mapping an id names a public model after it: the one of that exact name is
 * reused, or one is created. An id this account already maps — under any
 * public model, its row enabled or not — is left as it is, and so is the
 * public model of that name when this account already serves it under another
 * upstream name: neither is offered for selection.
 */

/** The most upstream models one apply takes, as the control plane's own `MAX_BULK_MODELS`. */
export const MAX_BULK_MODELS = 100;

/** The longest public model name the control plane takes, in characters. */
export const MAX_PUBLIC_NAME_CHARS = 128;

/** One row of the account's checklist, as `GET …/channel-models` answers it. */
export interface AccountRow {
  public_model_name: string;
  /** `null` means the public name itself is what the upstream is asked for. */
  upstream_model: string | null;
  enabled: boolean;
  /** Whether this account has a row for the public model at all. */
  served: boolean;
}

/** Why `name` cannot be a public model's name, as the control plane checks it, or null. */
export function publicNameProblem(name: string): string | null {
  const typed = name.trim();
  if (typed === "") return "名称为空";
  // Counted in characters, as the control plane counts them.
  if (Array.from(typed).length > MAX_PUBLIC_NAME_CHARS) return `名称超过 ${MAX_PUBLIC_NAME_CHARS} 个字符`;
  // Every Unicode control character (C0, DEL and C1).
  if (/[\u0000-\u001f\u007f-\u009f]/.test(typed)) return "名称包含控制字符";
  if (typed.includes("*")) return "名称包含 *";
  if (typed.includes("#")) return "名称包含 #";
  return null;
}

/** What mapping one upstream id would do. */
export type UpstreamState =
  /** This account already maps the id, under `publicName`. */
  | { kind: "mapped"; publicName: string; enabled: boolean }
  /** This account already serves the public model of that name, asking its upstream for `upstream`. */
  | { kind: "served"; upstream: string; enabled: boolean }
  /** A live public model of that name exists, and would be reused. */
  | { kind: "reuse" }
  /** No public model has that name; one would be created. */
  | { kind: "create" }
  /** A retired public model keeps the name, so it cannot be a public model's again. */
  | { kind: "retired" }
  /** The id cannot be a public model's name. */
  | { kind: "invalid"; problem: string };

/**
 * The state of `id` for an account whose checklist is `rows`, where
 * `liveNames` are the live public models' names and `retiredNames` the
 * retired ones'.
 */
export function stateOf(
  id: string,
  rows: AccountRow[],
  liveNames: ReadonlySet<string>,
  retiredNames: ReadonlySet<string>,
): UpstreamState {
  const name = id.trim();
  const mapped = rows
    .filter((r) => r.served && (r.upstream_model ?? r.public_model_name) === name)
    .sort((a, b) => (a.public_model_name < b.public_model_name ? -1 : a.public_model_name > b.public_model_name ? 1 : 0))[0];
  if (mapped !== undefined) return { kind: "mapped", publicName: mapped.public_model_name, enabled: mapped.enabled };
  const served = rows.find((r) => r.served && r.public_model_name === name);
  if (served !== undefined) {
    return { kind: "served", upstream: served.upstream_model ?? served.public_model_name, enabled: served.enabled };
  }
  const problem = publicNameProblem(name);
  if (problem !== null) return { kind: "invalid", problem };
  if (liveNames.has(name)) return { kind: "reuse" };
  if (retiredNames.has(name)) return { kind: "retired" };
  return { kind: "create" };
}

/** Whether an id in `state` can be selected: only one whose mapping would add a row. */
export function selectable(state: UpstreamState): boolean {
  return state.kind === "reuse" || state.kind === "create";
}

/** The words the list shows for `state`. */
export function stateText(state: UpstreamState): string {
  switch (state.kind) {
    case "mapped":
      return `已映射：公共模型 ${state.publicName}${state.enabled ? "" : "（未启用）"}`;
    case "served":
      return `该账号已提供同名公共模型，上游名为 ${state.upstream}${state.enabled ? "" : "（未启用）"}`;
    case "reuse":
      return "已有同名公共模型，将复用";
    case "create":
      return "将新建同名公共模型";
    case "retired":
      return "同名公共模型已停用，不能再用这个名称";
    case "invalid":
      return `不能作为公共模型名称：${state.problem}`;
  }
}

/** What an apply did, in one sentence. */
export function appliedText(items: ChannelBulkItem[]): string {
  const created = items.filter((i) => i.outcome === "created").length;
  const added = items.filter((i) => i.outcome === "added").length;
  const unchanged = items.filter((i) => i.outcome === "unchanged").length;
  const parts = [`新建公共模型 ${created} 个`, `复用已有公共模型 ${added} 个`];
  if (unchanged > 0) parts.push(`${unchanged} 个已映射，保持不变`);
  return `已映射 ${created + added} 个上游模型：${parts.join("，")}。`;
}
