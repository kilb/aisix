import { useCallback, useEffect, useState } from "react";
import * as api from "./lib/api";
import { usd } from "./lib/fmt";

/**
 * 线下充值。
 *
 * 用户在这里发起一笔申请，管理员核对到账后在后台确认，确认那一刻才入账。
 * 所以界面必须说清「提交 ≠ 到账」—— 否则用户会以为余额马上会变。
 */
export function Topup({ onChanged }: { onChanged: () => void }) {
  const [rows, setRows] = useState<api.Topup[] | null>(null);
  const [amount, setAmount] = useState("");
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  const load = useCallback(async () => {
    try {
      setRows((await api.topups()).topups);
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "读取失败");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function submit() {
    // 金额在这里就换算成 micro-USD 整数。浮点做钱会累积误差，而这个产品的
    // 花费到千分之一美分 —— 别把浮点带进账本。
    const micro = Math.round(Number(amount) * 1_000_000);
    if (!Number.isFinite(micro) || micro <= 0) {
      setErr("金额必须是正数");
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await api.requestTopup(micro, note.trim() || null);
      setAmount("");
      setNote("");
      setDone(true);
      await load();
      onChanged();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "提交失败");
    } finally {
      setBusy(false);
    }
  }

  const label = (s: string) =>
    s === "pending" ? "待确认" : s === "approved" ? "已入账" : "已驳回";

  return (
    <section className="panel">
      <h2>充值</h2>
      <p className="hint">
        线下转账后在这里登记，管理员核对到账后确认。
        <strong>确认之后余额才会变</strong>，提交本身不改余额。
      </p>

      <div className="row">
        <label className="f">
          <span>金额 USD</span>
          <input
            inputMode="decimal"
            value={amount}
            placeholder="20.00"
            onChange={(e) => setAmount(e.target.value)}
          />
        </label>
        <label className="f">
          <span>备注（转账单号等）</span>
          <input value={note} onChange={(e) => setNote(e.target.value)} />
        </label>
        <button className="act narrow" disabled={busy} onClick={() => void submit()}>
          {busy ? "提交中…" : "提交充值单"}
        </button>
      </div>

      {err && <div className="note crit">{err}</div>}
      {done && <div className="note warn">已提交，等待管理员确认。确认后余额才会变。</div>}

      {rows && rows.length > 0 && (
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>时间</th>
                <th className="right">金额</th>
                <th>备注</th>
                <th>状态</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((t) => (
                <tr key={t.id}>
                  <td className="num" style={{ fontSize: 11 }}>
                    {t.created_at.replace("T", " ").slice(0, 16)}
                  </td>
                  <td className="right num">{usd(t.micro_usd)}</td>
                  <td>{t.note ?? ""}</td>
                  <td>{label(t.status)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
