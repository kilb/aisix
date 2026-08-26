import { useCallback, useEffect, useState } from "react";
import * as api from "./lib/api";
import { count, splitLead, usd } from "./lib/fmt";

const RANGES: [number, string][] = [
  [1, "近 1 小时"],
  [24, "近 24 小时"],
  [168, "近 7 天"],
];

/** 一条读数。数字按有效位分层 —— 见 fmt.ts 里的说明。 */
function Reading({ label, value, foot }: { label: string; value: string; foot?: string }) {
  const pending = value === "—";
  const [lead, sig] = splitLead(value);
  return (
    <div className="entry">
      <span className="lab">{label}</span>
      <span className={`val${pending ? " absent" : ""}`}>
        {pending ? (
          value
        ) : (
          <>
            {lead && <span className="lead">{lead}</span>}
            {sig}
          </>
        )}
      </span>
      {foot && <span className="foot">{foot}</span>}
    </div>
  );
}

export function Account({ sess, onOut }: { sess: api.Session; onOut: () => void }) {
  const [bal, setBal] = useState<api.Balance | null>(null);
  const [use, setUse] = useState<api.Usage | null>(null);
  const [hours, setHours] = useState(24);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [b, u] = await Promise.all([api.balance(), api.usage(hours)]);
      setBal(b);
      setUse(u);
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "读取失败");
    }
  }, [hours]);

  useEffect(() => {
    void load();
  }, [load]);

  async function signOut() {
    await api.logout().catch(() => {});
    onOut();
  }

  return (
    <div className="frame">
      <header className="book-head">
        <div>
          <h1>AISIX 用量门户</h1>
          <span className="sub">{sess.email}</span>
        </div>
        <div className="tools">
          <select value={hours} onChange={(e) => setHours(Number(e.target.value))}>
            {RANGES.map(([h, label]) => (
              <option key={h} value={h}>
                {label}
              </option>
            ))}
          </select>
          <button className="ghost" onClick={() => void signOut()}>
            登出
          </button>
        </div>
      </header>

      {err && <div className="note crit">{err}</div>}

      {/* 未绑定密钥是一期最容易被忽略的状态：管理员手输 user_id 填错时，
          用量恒为 0 —— 跟「还没开始用」在屏幕上没有区别，而它实际意味着
          这个人在免费用。所以它显式占一整条横幅，而不是一个角落里的小字。 */}
      {use?.note && <div className="note warn">{use.note}</div>}

      <div className="deck">
        <section className="panel">
          <h2>余额</h2>
          <div className="entries">
            <Reading
              label="当前余额"
              value={bal ? usd(bal.balance_micro_usd) : "—"}
              foot={
                bal && bal.balance_micro_usd <= 0
                  ? "余额已耗尽，密钥已停用。补充额度后会自动恢复。"
                  : "额度由管理员发放"
              }
            />
          </div>
          <h3>流水</h3>
          {bal?.entries.length ? (
            <div className="scroll">
              <table>
                <thead>
                  <tr>
                    <th>金额</th>
                    <th>来源</th>
                    <th>备注</th>
                  </tr>
                </thead>
                <tbody>
                  {[...bal.entries].reverse().map((e) => (
                    <tr key={e.id}>
                      <td className="right num">{usd(e.delta_micro_usd)}</td>
                      <td>{e.source === "admin_grant" ? "管理员发放" : "消费"}</td>
                      <td>{e.note ?? ""}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <p className="hint">还没有任何记账。</p>
          )}
        </section>

        <section className="panel">
          <h2>用量</h2>
          <p className="hint">{use ? use.range : ""}，只统计你自己的密钥。</p>
          <div className="entries">
            <Reading label="请求数" value={count(use?.requests ?? null)} />
            <Reading label="Token" value={count(use?.tokens ?? null)} />
            <Reading label="花费" value={usd(use?.spend_micro_usd ?? null)} />
            <Reading
              label="已绑定密钥"
              value={use ? String(use.linked_keys) : "—"}
              foot={
                use && use.disabled_keys > 0
                  ? `其中 ${use.disabled_keys} 把已停用`
                  : undefined
              }
            />
          </div>
        </section>
      </div>
    </div>
  );
}
