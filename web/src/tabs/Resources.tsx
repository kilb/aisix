import type { DocState } from "../lib/useDoc";
import type { TabId } from "../App";

type Rec = Record<string, unknown>;

/**
 * 配置文件支持的全部集合 —— 包括没有专用界面的那几种。
 *
 * 列出来而不是隐身：一个不在界面上出现的资源类型，运维会以为网关不支持它。
 */
const RESOURCE_KINDS: [string, string, TabId | null][] = [
  ["provider_keys", "供应商密钥", "providers"],
  ["models", "模型", "models"],
  ["api_keys", "调用方密钥", "keys"],
  ["rate_limit_policies", "限流与预算", "limits"],
  ["guardrails", "护栏", null],
  ["cache_policies", "缓存策略", null],
  ["mcp_servers", "MCP 服务", null],
  ["a2a_agents", "A2A 智能体", null],
  ["observability_exporters", "可观测导出器", null],
  ["oidc_providers", "OIDC 提供方", null],
  ["claim_mappings", "声明映射", null],
  ["passthrough_routes", "透传路由", null],
];

export function Resources({
  doc,
  onGoto,
}: {
  doc: DocState;
  onGoto: (t: TabId) => void;
}) {
  return (
    <div className="panel">
      <h2>全部资源</h2>
      <p className="hint">
        配置文件支持的全部集合。没有专用界面的那几种可以在「配置原文」页直接编辑——
        保存前会用网关自己的校验器过一遍，不通过就不落盘。
      </p>
      <div className="scroll">
        <table>
          <thead>
            <tr>
              <th>资源</th>
              <th className="right">数量</th>
              <th>条目</th>
              <th>编辑方式</th>
            </tr>
          </thead>
          <tbody>
            {RESOURCE_KINDS.map(([key, label, tab]) => {
              const items = (doc.doc?.[key] as Rec[] | undefined) ?? [];
              const names = items.map((x) =>
                String(x.display_name ?? x.name ?? "（未命名）"),
              );
              return (
                <tr key={key}>
                  <td>
                    <strong>{label}</strong>
                    <div className="num" style={{ fontSize: 11, color: "var(--ink-3)" }}>
                      {key}
                    </div>
                  </td>
                  <td className="right num">{items.length}</td>
                  <td style={{ fontSize: 12 }}>
                    {names.length ? (
                      <>
                        {names.slice(0, 6).join("、")}
                        {names.length > 6 ? ` 等 ${names.length} 项` : ""}
                      </>
                    ) : (
                      <span style={{ color: "var(--ink-3)" }}>无</span>
                    )}
                  </td>
                  <td>
                    <button className="ghost" onClick={() => onGoto(tab ?? "raw")}>
                      {tab ? "专用界面" : "配置原文"}
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
