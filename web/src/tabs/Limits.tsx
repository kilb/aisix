import { useState } from "react";
import { Msg } from "../components/Msg";
import { fmtUsd, intOrNull } from "../lib/fmt";
import { useSave } from "../lib/useSave";
import type { DocState } from "../lib/useDoc";

type Rec = Record<string, unknown>;

const SCOPES = [
  ["api_key", "api_key —— 绑到某个调用方密钥"],
  ["model", "model —— 绑到某个模型"],
  ["team", "team —— 绑到某个团队"],
  ["member", "member —— 绑到某个用户"],
  ["team_member", "team_member —— 团队内的每个成员各自一份"],
] as const;

function Unset() {
  return <span style={{ color: "var(--ink-3)" }}>—</span>;
}

export function Limits({ doc }: { doc: DocState }) {
  const { note, setNote, busy, run } = useSave(doc);
  const policies = (doc.doc?.rate_limit_policies as Rec[] | undefined) ?? [];
  const keyNames = ((doc.doc?.api_keys as Rec[] | undefined) ?? [])
    .map((k) => String(k.display_name ?? ""))
    .filter(Boolean);
  const modelNames = ((doc.doc?.models as Rec[] | undefined) ?? [])
    .map((m) => String(m.display_name ?? ""))
    .filter(Boolean);

  const [name, setName] = useState("");
  const [scope, setScope] = useState<string>("api_key");
  const [ref, setRef] = useState("");
  const [window_, setWindow] = useState("day");
  const [req, setReq] = useState("");
  const [tok, setTok] = useState("");
  const [spend, setSpend] = useState("");

  /**
   * `api_key` / `model` 作用域能从配置里给出候选；`team` / `member` 是外部
   * 身份（来自密钥上的 `team_id` / `user_id`），配置里没有清单，只能手填。
   */
  const choices = scope === "api_key" ? keyNames : scope === "model" ? modelNames : null;

  async function add() {
    if (!name.trim()) {
      setNote({ text: "名称不能为空", kind: "crit" });
      return;
    }
    const r = intOrNull(req);
    const t = intOrNull(tok);
    const usd = parseFloat(spend);
    if (!r && !t && !(usd > 0)) {
      setNote({
        text: "三个上限至少要填一个——不填任何上限的策略网关会拒绝加载",
        kind: "crit",
      });
      return;
    }
    const item: Rec = {
      name: name.trim(),
      scope,
      scope_ref: ref || (choices?.[0] ?? ""),
      window: window_,
    };
    if (r) item.max_requests = r;
    if (t) item.max_tokens = t;
    // 界面用美元，落盘用 micro-USD 整数：计数器上不能出现浮点。
    if (usd > 0) item.max_spend_micro_usd = Math.round(usd * 1e6);

    const ok = await run((d) => {
      d.rate_limit_policies = [
        ...((d.rate_limit_policies as unknown[] | undefined) ?? []),
        item,
      ];
    });
    if (ok) {
      setName("");
      setReq("");
      setTok("");
      setSpend("");
    }
  }

  async function del(i: number) {
    if (!confirm("确认删除？会立即重写配置并重载网关。")) return;
    await run((d) => {
      const list = [...((d.rate_limit_policies as unknown[] | undefined) ?? [])];
      list.splice(i, 1);
      d.rate_limit_policies = list;
    });
  }

  return (
    <>
      <div className="panel">
        <h2>限流与预算策略</h2>
        <p className="hint">
          一条策略把若干上限绑到一个作用域上。三个维度互相独立、同时生效：
          请求数、token 数、花费金额——任一超限即拒。
        </p>
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>名称</th>
                <th>作用域</th>
                <th>对象</th>
                <th>窗口</th>
                <th className="right">请求上限</th>
                <th className="right">Token 上限</th>
                <th className="right">花费上限</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {policies.length === 0 ? (
                <tr>
                  <td colSpan={8} style={{ color: "var(--ink-3)" }}>
                    还没有策略。
                  </td>
                </tr>
              ) : (
                policies.map((p, i) => (
                  <tr key={`${String(p.name)}-${i}`}>
                    <td>
                      <strong>{String(p.name ?? "")}</strong>
                    </td>
                    <td>{String(p.scope ?? "—")}</td>
                    <td className="num" style={{ fontSize: 12 }}>
                      {String(p.scope_ref ?? "—")}
                    </td>
                    <td>{String(p.window ?? "—")}</td>
                    <td className="right num">
                      {p.max_requests != null ? String(p.max_requests) : <Unset />}
                    </td>
                    <td className="right num">
                      {p.max_tokens != null ? String(p.max_tokens) : <Unset />}
                    </td>
                    <td className="right num">
                      {p.max_spend_micro_usd != null ? (
                        fmtUsd(p.max_spend_micro_usd)
                      ) : (
                        <Unset />
                      )}
                    </td>
                    <td className="right">
                      <button className="ghost" disabled={busy} onClick={() => void del(i)}>
                        删除
                      </button>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
        <div className="note">
          窗口是 epoch 对齐的固定窗口：<code>day</code> 在 UTC 零点归零，
          <code>hour</code> 在整点，<code>minute</code> 在整分。
          <code>second</code> 窗口下 token 与花费上限<strong>不生效</strong>——
          它们都要等上游答复才知道数值，一秒窗口内几乎必然滞后，网关会在日志里报出来而不是假装生效。
        </div>
      </div>

      <div className="panel">
        <h2>新增策略</h2>
        <div className="grid g2">
          <label className="f">
            <span>名称</span>
            <input
              type="text"
              placeholder="team-daily"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <label className="f">
            <span>作用域</span>
            <select
              value={scope}
              onChange={(e) => {
                setScope(e.target.value);
                setRef("");
              }}
            >
              {SCOPES.map(([v, label]) => (
                <option key={v} value={v}>
                  {label}
                </option>
              ))}
            </select>
          </label>
          <label className="f">
            <span>对象</span>
            {choices ? (
              <select value={ref} onChange={(e) => setRef(e.target.value)}>
                {choices.map((c) => (
                  <option key={c} value={c}>
                    {c}
                  </option>
                ))}
              </select>
            ) : (
              <input
                type="text"
                placeholder={scope === "team" ? "团队 id" : "用户 id"}
                value={ref}
                onChange={(e) => setRef(e.target.value)}
              />
            )}
          </label>
          <label className="f">
            <span>窗口</span>
            <select value={window_} onChange={(e) => setWindow(e.target.value)}>
              <option value="day">day（UTC 零点归零）</option>
              <option value="hour">hour</option>
              <option value="minute">minute</option>
              <option value="second">second（token/花费上限在此窗口下不生效）</option>
            </select>
          </label>
        </div>
        <h3>上限（至少填一项）</h3>
        <div className="grid g3">
          <label className="f">
            <span>请求数上限</span>
            <input
              type="number"
              min="1"
              placeholder="留空不限"
              value={req}
              onChange={(e) => setReq(e.target.value)}
            />
          </label>
          <label className="f">
            <span>Token 上限</span>
            <input
              type="number"
              min="1"
              placeholder="留空不限"
              value={tok}
              onChange={(e) => setTok(e.target.value)}
            />
          </label>
          <label className="f">
            <span>花费上限 USD</span>
            <input
              type="number"
              step="0.01"
              min="0"
              placeholder="留空不限"
              value={spend}
              onChange={(e) => setSpend(e.target.value)}
            />
          </label>
        </div>
        <button className="act" disabled={busy} onClick={() => void add()}>
          {busy ? "保存中…" : "保存并重载网关"}
        </button>
        <Msg note={note} />
      </div>
    </>
  );
}
