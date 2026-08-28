import { useCallback, useEffect, useState } from "react";
import * as api from "./lib/api";
import { usd } from "./lib/fmt";

/**
 * 自助密钥与每把密钥的额度。
 *
 * 两层闸：总额度由管理员设定，你自己决定怎么把它分到各把密钥上。分出去的总和
 * 不能超过总额度 —— 服务端会挡，这里也把剩余可分配额摆在明面上，免得让人填完
 * 才被拒。
 *
 * 不分配额度的密钥不是「没得花」，是「不单独设限」：它只受总额度约束。这两种
 * 状态在界面上必须分得开，否则用户会以为得给每把密钥都填个数才能用。
 *
 * 明文只在铸出来那一次出现。界面因此必须把它当成一次性的东西对待：显眼地给
 * 出来、说清不会再显示，而不是塞进列表里等用户以后回来复制。
 */
export function Keys({ onChanged }: { onChanged: () => void }) {
  const [list, setList] = useState<api.KeyList | null>(null);
  const [minted, setMinted] = useState<api.MintedKey | null>(null);
  const [label, setLabel] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  /** 正在编辑额度的那把密钥的全名，以及输入框里的 USD 文本。 */
  const [editing, setEditing] = useState<string | null>(null);
  const [quotaUsd, setQuotaUsd] = useState("");

  const load = useCallback(async () => {
    try {
      setList(await api.listKeys());
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

  async function saveQuota(fullName: string) {
    // 金额按 micro-USD 整数下发。浮点做钱会累积误差，而这个产品的花费到千分
    // 之一美分 —— 在这里就换算成整数。
    const micro = Math.round(Number(quotaUsd) * 1_000_000);
    if (!Number.isFinite(micro) || micro < 0) {
      setErr("额度必须是非负数");
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await api.setKeyQuota(fullName.split(" · ")[0] ?? fullName, micro);
      setEditing(null);
      setQuotaUsd("");
      await load();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "设置失败");
    } finally {
      setBusy(false);
    }
  }

  const rows = list?.keys ?? null;
  const granted = list?.granted_micro_usd ?? 0;
  const allocated = list?.allocated_micro_usd ?? 0;
  const free = granted - allocated;

  return (
    <section className="panel">
      <h2>API 密钥</h2>
      <p className="hint">
        可以创建任意多把。每把可以单独设额度，各把额度之和不会超过你的总额度；
        不设额度的密钥只受总额度约束。总额度为零时新密钥处于停用态，管理员设好
        额度后自动启用。
      </p>
      {/* 「额度」在这里是累计口径，跟总额度一致。不写明的话它读起来像每月刷新的
          预算，用户会以为花完了下个周期还能再花一遍。 */}
      <p className="hint">
        这里的额度都是<strong>累计</strong>口径：花掉的算数，不会按周期刷新。把某把
        密钥的额度调高，多出来的部分才是它还能花的。
      </p>

      {list && (
        <div className="row budget">
          <span>
            总额度 <strong className="num">{usd(granted)}</strong>
          </span>
          <span>
            已分配 <strong className="num">{usd(allocated)}</strong>
          </span>
          <span>
            可再分配{" "}
            <strong className="num" data-low={free <= 0 ? "yes" : undefined}>
              {usd(free)}
            </strong>
          </span>
        </div>
      )}

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
                <th className="right">额度</th>
                <th>状态</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rows.map((k) => (
                <tr key={k.name}>
                  <td>{k.name}</td>
                  <td className="num">{k.masked_hash}</td>
                  <td className="right">
                    {editing === k.name ? (
                      <span className="quota-edit">
                        <input
                          aria-label={`${k.name} 的额度 USD`}
                          inputMode="decimal"
                          value={quotaUsd}
                          placeholder="0 = 不设限"
                          onChange={(e) => setQuotaUsd(e.target.value)}
                        />
                        <button
                          className="ghost"
                          disabled={busy}
                          onClick={() => void saveQuota(k.name)}
                        >
                          保存
                        </button>
                        <button className="ghost" onClick={() => setEditing(null)}>
                          取消
                        </button>
                      </span>
                    ) : (
                      <button
                        className="ghost num"
                        onClick={() => {
                          setEditing(k.name);
                          setQuotaUsd(
                            k.quota_micro_usd > 0 ? String(k.quota_micro_usd / 1_000_000) : "",
                          );
                        }}
                      >
                        {k.quota_micro_usd > 0 ? usd(k.quota_micro_usd) : "不设限"}
                      </button>
                    )}
                  </td>
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
