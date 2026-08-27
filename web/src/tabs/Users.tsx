import { useCallback, useEffect, useState } from "react";
import * as api from "../lib/api";
import { fmtUsd } from "../lib/fmt";

/**
 * 自助门户的注册用户与额度发放。
 *
 * 额度挂在**用户**身上，不在密钥上：一个用户的所有密钥共用同一份余额。所以
 * 这里发放的对象是人，不是某一把密钥。
 *
 * 发放对象从这份列表里**选**，不给手输的口子。一期密钥的 `user_id` 曾经靠手
 * 填，填错一个字符网关照常放行、指标打错标签、用量查不到 —— 于是永不扣款，
 * 用户免费用而没人会发现。
 */

interface Topup {
  id: number;
  email: string;
  micro_usd: number;
  note: string | null;
  created_at: string;
}

interface PortalUser {
  user_id: string;
  email: string;
  display_name: string | null;
  disabled: boolean;
  balance_micro_usd: number;
}

export function Users() {
  const [rows, setRows] = useState<PortalUser[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [absent, setAbsent] = useState(false);
  const [target, setTarget] = useState<PortalUser | null>(null);
  const [usd, setUsd] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<string | null>(null);

  const [topups, setTopups] = useState<Topup[]>([]);

  const load = useCallback(async () => {
    try {
      const [r, t] = await Promise.all([api.portalUsers(), api.portalTopups()]);
      setRows(r.users);
      setTopups(t.topups);
      setErr(null);
      setAbsent(false);
    } catch (e) {
      const msg = e instanceof Error ? e.message : "读取失败";
      // 没配门户不是错误，控制台可以独立部署。
      if (msg.includes("未配置自助门户")) setAbsent(true);
      else setErr(msg);
      setRows([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function grant() {
    if (!target) return;
    // 金额按 micro-USD 整数下发。浮点做钱会累积误差，而这个产品的花费到千分
    // 之一美分 —— 在这里就换算成整数，别把浮点带进账本。
    const micro = Math.round(Number(usd) * 1_000_000);
    if (!Number.isFinite(micro) || micro <= 0) {
      setErr("发放金额必须是正数");
      return;
    }
    setBusy(true);
    setErr(null);
    setDone(null);
    try {
      await api.portalGrant(target.user_id, micro, note.trim() || null);
      setDone(`已给 ${target.email} 发放 ${fmtUsd(micro)}`);
      setUsd("");
      setNote("");
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "发放失败");
    } finally {
      setBusy(false);
    }
  }

  if (absent) {
    return (
      <div className="panel">
        <h2>门户用户</h2>
        <div className="note warn">
          这台控制台没有配自助门户（<code>PORTAL_ADMIN_TOKEN</code> 未设置）。
          配上之后，注册用户与额度发放会出现在这里。
        </div>
      </div>
    );
  }

  async function decide(id: number, d: "approve" | "reject") {
    setBusy(true);
    setErr(null);
    setDone(null);
    try {
      await api.portalDecideTopup(id, d, null);
      setDone(d === "approve" ? "已确认，额度已入账" : "已驳回");
      await load();
    } catch (e) {
      // 409 = 别人已经处理过。照实说，别让人以为自己刚入了账。
      setErr(e instanceof Error ? e.message : "处理失败");
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      {topups.length > 0 && (
        <div className="panel">
          <h2>待确认的充值单</h2>
          <p className="hint">
            核对到账后确认，<strong>确认那一刻才入账</strong>。同一笔被重复确认时
            服务端会拒绝第二次，不会重复入账。
          </p>
          <div className="scroll">
            <table>
              <thead>
                <tr>
                  <th>时间</th>
                  <th>用户</th>
                  <th className="right">金额</th>
                  <th>备注</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {topups.map((t) => (
                  <tr key={t.id}>
                    <td className="num" style={{ fontSize: 11 }}>
                      {t.created_at.replace("T", " ").slice(0, 16)}
                    </td>
                    <td>{t.email}</td>
                    <td className="right num">{fmtUsd(t.micro_usd)}</td>
                    <td>{t.note ?? ""}</td>
                    <td className="right">
                      <button
                        className="ghost"
                        disabled={busy}
                        onClick={() => void decide(t.id, "approve")}
                      >
                        确认
                      </button>{" "}
                      <button
                        className="ghost"
                        disabled={busy}
                        onClick={() => void decide(t.id, "reject")}
                      >
                        驳回
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {err && <div className="note crit">{err}</div>}
          {done && <div className="note">{done}</div>}
        </div>
      )}

      <div className="panel">
        <h2>发放额度</h2>
        <p className="hint">
          额度挂在用户身上，他名下的所有密钥共用这一份余额。余额归零时那些密钥
          会被自动停用，补上之后自动恢复。
        </p>
        <div className="r">
          <label className="f">
            <span>用户</span>
            <select
              value={target?.user_id ?? ""}
              onChange={(e) =>
                setTarget(rows?.find((u) => u.user_id === e.target.value) ?? null)
              }
            >
              <option value="">选择一个用户</option>
              {(rows ?? []).map((u) => (
                <option key={u.user_id} value={u.user_id}>
                  {u.email}（余额 {fmtUsd(u.balance_micro_usd)}）
                </option>
              ))}
            </select>
          </label>
          <label className="f">
            <span>金额 USD</span>
            <input
              inputMode="decimal"
              value={usd}
              placeholder="5.00"
              onChange={(e) => setUsd(e.target.value)}
            />
          </label>
          <label className="f">
            <span>备注</span>
            <input
              value={note}
              placeholder="首充赠送"
              onChange={(e) => setNote(e.target.value)}
            />
          </label>
          <button className="act" disabled={busy || !target} onClick={() => void grant()}>
            {busy ? "处理中…" : "发放"}
          </button>
        </div>
        {err && <div className="note crit">{err}</div>}
        {done && <div className="note">{done}</div>}
      </div>

      <div className="panel">
        <h2>注册用户</h2>
        {rows === null ? (
          <p className="hint">读取中…</p>
        ) : rows.length === 0 ? (
          <p className="hint">还没有人注册。</p>
        ) : (
          <div className="scroll">
            <table>
              <thead>
                <tr>
                  <th>邮箱</th>
                  <th>用户 ID</th>
                  <th className="right">余额</th>
                  <th>状态</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((u) => (
                  <tr key={u.user_id}>
                    <td>{u.email}</td>
                    <td className="num" style={{ fontSize: 11 }}>
                      {u.user_id}
                    </td>
                    <td className="right num">{fmtUsd(u.balance_micro_usd)}</td>
                    <td>
                      {u.disabled
                        ? "已停用"
                        : u.balance_micro_usd <= 0
                          ? "无额度"
                          : "正常"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </>
  );
}
