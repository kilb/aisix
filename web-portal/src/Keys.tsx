import { useCallback, useEffect, useState } from "react";
import * as api from "./lib/api";

/**
 * 自助密钥。
 *
 * 一个用户可以铸任意多把，它们**共享同一份余额** —— 额度挂在用户身上，不在
 * 密钥上。所以这里没有「给这把密钥分多少钱」这种东西。
 *
 * 明文只在铸出来那一次出现。界面因此必须把它当成一次性的东西对待：显眼地给
 * 出来、说清不会再显示，而不是塞进列表里等用户以后回来复制。
 */
export function Keys({ onChanged }: { onChanged: () => void }) {
  const [rows, setRows] = useState<api.KeyRow[] | null>(null);
  const [minted, setMinted] = useState<api.MintedKey | null>(null);
  const [label, setLabel] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setRows((await api.listKeys()).keys);
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "读取密钥失败");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function create() {
    setBusy(true);
    setErr(null);
    try {
      setMinted(await api.createKey(label));
      setLabel("");
      await load();
      onChanged();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "创建失败");
    } finally {
      setBusy(false);
    }
  }

  async function revoke(name: string) {
    setBusy(true);
    setErr(null);
    try {
      await api.revokeKey(name);
      await load();
      onChanged();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "吊销失败");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="panel">
      <h2>API 密钥</h2>
      <p className="hint">
        可以创建任意多把，它们共用同一份余额。余额为零时新密钥会处于停用态，
        管理员发放额度后自动启用。
      </p>

      <div className="row">
        <label className="f">
          <span>名称（可选）</span>
          <input
            value={label}
            placeholder="我的密钥"
            onChange={(e) => setLabel(e.target.value)}
          />
        </label>
        <button className="act narrow" disabled={busy} onClick={() => void create()}>
          {busy ? "处理中…" : "创建密钥"}
        </button>
      </div>

      {err && <div className="note crit">{err}</div>}

      {/* 明文只出现一次，所以必须给足提示。塞进列表里等用户以后来拿，
          那是他们再也拿不到的东西。 */}
      {minted && (
        <div className="note warn minted">
          <strong>请立刻复制并妥善保存 —— 这串明文不会再显示。</strong>
          <code>{minted.plaintext}</code>
          {minted.note && <span className="hint">{minted.note}</span>}
          <button className="ghost" onClick={() => setMinted(null)}>
            我已保存
          </button>
        </div>
      )}

      {rows === null ? (
        <p className="hint">读取中…</p>
      ) : rows.length === 0 ? (
        <p className="hint">还没有密钥。创建一把即可开始调用。</p>
      ) : (
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>名称</th>
                <th>散列</th>
                <th>状态</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rows.map((k) => (
                <tr key={k.name}>
                  <td>{k.name}</td>
                  <td className="num">{k.masked_hash}</td>
                  <td>{k.disabled ? "已停用" : "可用"}</td>
                  <td className="right">
                    <button
                      className="ghost"
                      disabled={busy}
                      onClick={() => void revoke(k.name.split(" · ")[0] ?? k.name)}
                    >
                      吊销
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
