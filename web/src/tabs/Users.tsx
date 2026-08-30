import { useCallback, useEffect, useState } from "react";
import * as api from "../lib/api";
import type { PortalUser } from "../lib/api";
import { fmtUsd } from "../lib/fmt";

/**
 * 自助门户的注册用户与额度。
 *
 * 额度由管理员设定在**用户**身上。用户自己再决定怎么把它分到各把密钥上 ——
 * 那一步在门户里做，各把密钥的额度之和不会超过这里设的总额。
 *
 * 两个动作分工不同：**设定**是绝对值（「他一共 20 块」），**发放**是增量
 * （「再给他 5 块」）。日常调额用设定 —— 用增量调额得先算差值，算错就是白送
 * 或误封。
 *
 * 对象从这份列表里**选**，不给手输的口子。一期密钥的 `user_id` 曾经靠手填，
 * 填错一个字符网关照常放行、指标打错标签、用量查不到 —— 于是永不扣款，用户
 * 免费用而没人会发现。
 */

interface Topup {
  id: number;
  email: string;
  micro_usd: number;
  note: string | null;
  created_at: string;
}

export function Users() {
  const [rows, setRows] = useState<PortalUser[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [absent, setAbsent] = useState(false);
  /**
   * 选中的是**用户 ID**，不是那一行的对象快照。
   *
   * 存对象的话：改完额度后 `load()` 换掉了整份列表，而这个快照还留在原地，
   * 于是「当前总额度」那行显示的是改之前的数 —— 管理员看着没变，很可能再设
   * 一次。所以选中的是 id，显示的数每次从最新列表里现算。
   */
  const [targetId, setTargetId] = useState("");
  const [usd, setUsd] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<string | null>(null);

  const [topups, setTopups] = useState<Topup[]>([]);

  const target: PortalUser | null = rows?.find((u) => u.user_id === targetId) ?? null;

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

  /**
   * 提交一次额度改动。
   *
   * `mode` 决定语义：`set` 是设定成这个数，`grant` 是再加这么多。两者共用同一
   * 段换算与校验，因为差别只在语义 —— 分开写两份换算，迟早只有一份被改对。
   */
  async function submit(mode: "set" | "grant") {
    if (!target) return;
    // 金额按 micro-USD 整数下发。浮点做钱会累积误差，而这个产品的花费到千分
    // 之一美分 —— 在这里就换算成整数，别把浮点带进账本。
    // 空输入必须先挡住。`Number("")` 是 0，而「设为 0」是把这个人的额度整个
    // 收回 —— 金额栏留空时点主按钮就会静默切断一个客户。
    if (usd.trim() === "") {
      setErr("请先填金额");
      return;
    }
    const micro = Math.round(Number(usd) * 1_000_000);
    // 设定允许 0（把额度收回），发放不允许 —— 发放 0 是个空操作，静默成功会让
    // 人以为发出去了。
    if (!Number.isFinite(micro) || micro < 0 || (mode === "grant" && micro === 0)) {
      setErr(mode === "set" ? "额度不能是负数" : "发放金额必须是正数");
      return;
    }
    setBusy(true);
    setErr(null);
    setDone(null);
    try {
      if (mode === "set") {
        await api.portalSetQuota(target.user_id, micro, note.trim() || null);
        setDone(`${target.email} 的总额度已设为 ${fmtUsd(micro)}`);
      } else {
        await api.portalGrant(target.user_id, micro, note.trim() || null);
        setDone(`已给 ${target.email} 发放 ${fmtUsd(micro)}`);
      }
      setUsd("");
      setNote("");
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "提交失败");
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

  /**
   * 停用或恢复一个账号。
   *
   * 停用同时挡住两侧：他登不进门户，名下密钥也会在下一轮对账后被网关拒绝。
   * 之前这个开关只能直接改库 —— 那是运维手术，而这张表已经在显示「已停用」了。
   */
  async function suspend(u: PortalUser) {
    setBusy(true);
    setErr(null);
    setDone(null);
    try {
      await api.portalSuspend(u.user_id, !u.disabled);
      setDone(
        u.disabled
          ? `已恢复 ${u.email}，名下密钥会在下一轮对账后重新可用`
          : `已停用 ${u.email}，名下密钥会在下一轮对账后被拒绝`,
      );
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "操作失败");
    } finally {
      setBusy(false);
    }
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
            核对到账后确认，<strong>确认那一刻才入账</strong>。同一笔被重复确认时{""}
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
        <h2>用户额度</h2>
        <p className="hint">
          额度挂在用户身上，他名下的密钥共用这一份。用户可以在门户里给每把密钥{""}
          单独设额度，各把之和不会超过这里设的总额。总额被消耗完时那些密钥会被{""}
          自动停用，调高之后自动恢复。
        </p>
        <div className="r">
          <label className="f">
            <span>用户</span>
            <select value={targetId} onChange={(e) => setTargetId(e.target.value)}>
              <option value="">选择一个用户</option>
              {(rows ?? []).map((u) => (
                <option key={u.user_id} value={u.user_id}>
                  {u.email}（总额度 {fmtUsd(u.granted_micro_usd)}）
                </option>
              ))}
            </select>
          </label>
          <label className="f">
            <span>金额 USD</span>
            <input
              inputMode="decimal"
              value={usd}
              placeholder="20.00"
              onChange={(e) => setUsd(e.target.value)}
            />
          </label>
          <label className="f">
            <span>备注</span>
            <input
              value={note}
              placeholder="季度额度"
              onChange={(e) => setNote(e.target.value)}
            />
          </label>
          <button className="act" disabled={busy || !target} onClick={() => void submit("set")}>
            {busy ? "处理中…" : "设为该额度"}
          </button>
          <button className="ghost" disabled={busy || !target} onClick={() => void submit("grant")}>
            改为追加
          </button>
        </div>
        {target && (
          <p className="hint">
            {target.email} 当前总额度 <strong>{fmtUsd(target.granted_micro_usd)}</strong>，已用{" "}
            <strong>{fmtUsd(target.granted_micro_usd - target.balance_micro_usd)}</strong>，其中{" "}
            <strong>{fmtUsd(target.allocated_micro_usd)}</strong> 已由他自己分配到具体密钥上。
          </p>
        )}
        {/* 调低总额度可能让「已分配」反过来超过总额。花费仍受用户级那道闸约束，
            所以不拦这次设定；但不说出来的话，管理员看不到分配已经对不上，而用户
            那边只会看到一个负的「可再分配」而不知道原因。 */}
        {target && target.allocated_micro_usd > target.granted_micro_usd && (
          <div className="note warn">
            他已分配到各把密钥上的额度（{fmtUsd(target.allocated_micro_usd)}）超过了总额度。
            总花销仍以总额度为准；他需要自己调低某几把密钥的额度才能重新分配。
          </div>
        )}
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
                  <th className="right">总额度</th>
                  <th className="right">余额</th>
                  <th className="right">已分配到密钥</th>
                  <th>状态</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((u) => (
                  <tr key={u.user_id}>
                    <td>{u.email}</td>
                    <td className="num" style={{ fontSize: 11 }}>
                      {u.user_id}
                    </td>
                    <td className="right num">{fmtUsd(u.granted_micro_usd)}</td>
                    <td className="right num">{fmtUsd(u.balance_micro_usd)}</td>
                    <td className="right num">{fmtUsd(u.allocated_micro_usd)}</td>
                    <td>
                      {u.disabled
                        ? "已停用"
                        : u.balance_micro_usd <= 0
                          ? "额度已用完"
                          : "正常"}
                    </td>
                    <td className="right">
                      <button
                        className="ghost"
                        disabled={busy}
                        onClick={() => void suspend(u)}
                      >
                        {u.disabled ? "恢复" : "停用"}
                      </button>
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
