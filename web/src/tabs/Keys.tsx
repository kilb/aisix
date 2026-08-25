import { useState } from "react";
import * as api from "../lib/api";
import { Msg } from "../components/Msg";
import { fmtUsd, intOrNull, modelsOrAll } from "../lib/fmt";
import { useSave } from "../lib/useSave";
import type { DocState } from "../lib/useDoc";

type Rec = Record<string, unknown>;

function fmtRl(rl: unknown): string {
  if (!rl || typeof rl !== "object") return "不限";
  const parts = Object.entries(rl as Rec)
    .filter(([, v]) => v != null)
    .map(([k, v]) => `${k}=${String(v)}`);
  return parts.length ? parts.join(" ") : "不限";
}

/**
 * 找出挂在某个 key 上的花费策略。
 *
 * 文件模式下 `scope_ref` 存的是 key 的 `display_name` 而不是 UUID —— 上游
 * 的 desugar 会把名字解析成派生 id。
 */
function spendPolicyFor(policies: Rec[], keyName: unknown): Rec | undefined {
  return policies.find(
    (p) =>
      p.scope === "api_key" && p.scope_ref === keyName && p.max_spend_micro_usd != null,
  );
}

export function Keys({ doc }: { doc: DocState }) {
  const { note, setNote, busy, run } = useSave(doc);
  /** 明文单独存：它必须在保存成败之前就呈现，且不能被后续提示冲掉。 */
  const [plaintext, setPlaintext] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [models, setModels] = useState("*");
  const [rpm, setRpm] = useState("");
  const [tpm, setTpm] = useState("");
  const [spend, setSpend] = useState("");
  const [window_, setWindow] = useState("day");

  const keys = (doc.doc?.api_keys as Rec[] | undefined) ?? [];
  const policies = (doc.doc?.rate_limit_policies as Rec[] | undefined) ?? [];

  async function mint() {
    setPlaintext(null);
    const n = name.trim();
    if (!n) {
      setNote({ text: "名称不能为空", kind: "crit" });
      return;
    }
    if (keys.some((k) => k.display_name === n)) {
      setNote({ text: `已存在同名密钥「${n}」`, kind: "crit" });
      return;
    }

    const usd = parseFloat(spend);
    // micro-USD 是整数计数器，低于一个单位的上限四舍五入后是 0 —— 那等于
    // 禁止这把密钥花任何钱，而用户以为自己设了一个很小的额度。
    if (!Number.isNaN(usd) && usd > 0 && Math.round(usd * 1e6) < 1) {
      setNote({
        text: `花费上限 $${usd} 小于最小可表示单位（0.000001 美元），四舍五入后会变成 0——那等于禁止这把密钥花任何钱。请填更大的值或留空。`,
        kind: "crit",
      });
      return;
    }

    let minted: { plaintext: string; key_hash: string };
    try {
      minted = await api.mintKey();
    } catch (e) {
      setNote({ text: e instanceof Error ? e.message : "生成失败", kind: "crit" });
      return;
    }

    // 明文先落到界面上，再去保存。反过来的话，只要保存返回非 2xx（哪怕配置
    // 其实已经写进去了，例如只是 SIGHUP 没送达），明文就永久丢失 —— 而它
    // 已经在网关配置里生效，等于一把没人持有的活密钥。
    setPlaintext(minted.plaintext);

    const item: Rec = {
      display_name: n,
      key_hash: minted.key_hash,
      allowed_models: modelsOrAll(models),
    };
    // 只接受纯整数字面量：`parseInt("1e3")` 是 1 而不是 1000，运维填科学
    // 计数法会得到一个紧 1000 倍的限制且毫无提示。
    const rl: Rec = {};
    const r = intOrNull(rpm);
    const t = intOrNull(tpm);
    if (r) rl.rpm = r;
    if (t) rl.tpm = t;
    if (Object.keys(rl).length) item.rate_limit = rl;

    const ok = await run((d) => {
      d.api_keys = [...((d.api_keys as unknown[] | undefined) ?? []), item];
      // 花费上限是独立的策略资源，不是 key 上的内联字段 —— 网关按
      // scope/scope_ref 把它绑到这个 key 上。
      if (!Number.isNaN(usd) && usd > 0) {
        d.rate_limit_policies = [
          ...((d.rate_limit_policies as unknown[] | undefined) ?? []),
          {
            name: `${n}-spend`,
            scope: "api_key",
            scope_ref: n,
            window: window_,
            // 界面用美元，落盘用 micro-USD 整数：计数器上不能出现浮点。
            max_spend_micro_usd: Math.round(usd * 1e6),
          },
        ];
      }
    });
    if (ok) {
      setName("");
      setSpend("");
      setRpm("");
      setTpm("");
    }
  }

  /**
   * 删 key 必须连同引用它的策略一起删。
   *
   * `scope_ref` 指向不存在的 key 会让整份配置校验失败，网关从此再也重载
   * 不了 —— 留下孤儿策略比留下密钥更危险。
   */
  async function delKey(i: number) {
    const k = keys[i];
    if (!k) return;
    const orphans = policies.filter(
      (p) => p.scope === "api_key" && p.scope_ref === k.display_name,
    );
    const extra = orphans.length
      ? `\n\n同时会删除引用它的 ${orphans.length} 条策略——留着会让配置校验失败。`
      : "";
    if (!confirm(`确认删除密钥「${String(k.display_name)}」？会立即重写配置并重载网关。${extra}`)) {
      return;
    }
    await run((d) => {
      const list = [...((d.api_keys as Rec[] | undefined) ?? [])];
      list.splice(i, 1);
      d.api_keys = list;
      if (orphans.length) {
        d.rate_limit_policies = ((d.rate_limit_policies as Rec[] | undefined) ?? []).filter(
          (p) => !(p.scope === "api_key" && p.scope_ref === k.display_name),
        );
      }
    });
  }

  async function delPolicy(i: number) {
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
        <h2>调用方密钥</h2>
        <p className="hint">配置里只存 sha256。明文在生成时显示一次，之后无法找回。</p>
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>名称</th>
                <th>可用模型</th>
                <th>限速</th>
                <th>花费上限</th>
                <th>散列</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {keys.length === 0 ? (
                <tr>
                  <td colSpan={6} style={{ color: "var(--ink-3)" }}>
                    还没有调用方密钥。
                  </td>
                </tr>
              ) : (
                keys.map((k, i) => {
                  const sp = spendPolicyFor(policies, k.display_name);
                  const allowed = (k.allowed_models as string[] | undefined) ?? ["*"];
                  return (
                    <tr key={`${String(k.display_name)}-${i}`}>
                      <td>
                        <strong>{String(k.display_name ?? "（未命名）")}</strong>
                      </td>
                      <td>{allowed.join(", ")}</td>
                      <td className="num" style={{ fontSize: 12 }}>
                        {fmtRl(k.rate_limit)}
                      </td>
                      <td className="num" style={{ fontSize: 12 }}>
                        {sp ? (
                          `${fmtUsd(sp.max_spend_micro_usd)} / ${String(sp.window ?? "day")}`
                        ) : (
                          <span style={{ color: "var(--ink-3)" }}>不限</span>
                        )}
                      </td>
                      <td className="num" style={{ fontSize: 11, color: "var(--ink-3)" }}>
                        {String(k.key_hash ?? "").slice(0, 16)}…
                      </td>
                      <td className="right">
                        <button className="ghost" disabled={busy} onClick={() => void delKey(i)}>
                          删除
                        </button>
                      </td>
                    </tr>
                  );
                })
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="panel">
        <h2>生成新密钥</h2>
        <div className="grid g2">
          <label className="f">
            <span>名称</span>
            <input
              type="text"
              placeholder="my-app"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <label className="f">
            <span>可用模型（逗号分隔，* 为全部）</span>
            <input type="text" value={models} onChange={(e) => setModels(e.target.value)} />
          </label>
        </div>

        <h3>限速</h3>
        <p className="hint">
          按请求数与 token 数限速，超限返回 429 <code>rate_limit_exceeded</code>。
        </p>
        <div className="grid g2">
          <label className="f">
            <span>每分钟请求上限（留空不限）</span>
            <input
              type="number"
              min="1"
              placeholder="60"
              value={rpm}
              onChange={(e) => setRpm(e.target.value)}
            />
          </label>
          <label className="f">
            <span>每分钟 token 上限（留空不限）</span>
            <input
              type="number"
              min="1"
              placeholder="100000"
              value={tpm}
              onChange={(e) => setTpm(e.target.value)}
            />
          </label>
        </div>

        <h3>花费上限</h3>
        <p className="hint">
          按金额的上限，网关本地执行。超限返回 429 <code>billing_error</code>——和限速的
          429 是<strong>不同的分类</strong>，客户端据此区分「钱用完了」和「发得太快了」。
        </p>
        <div className="grid g2">
          <label className="f">
            <span>花费上限 USD（留空不限）</span>
            <input
              type="number"
              step="0.01"
              min="0"
              placeholder="5.00"
              value={spend}
              onChange={(e) => setSpend(e.target.value)}
            />
          </label>
          <label className="f">
            <span>结算窗口</span>
            <select value={window_} onChange={(e) => setWindow(e.target.value)}>
              <option value="day">每日（UTC 零点归零）</option>
              <option value="hour">每小时</option>
              <option value="minute">每分钟</option>
            </select>
          </label>
        </div>
        <div className="note">
          花费按模型定价算出，所以<strong>未定价的模型对上限没有贡献</strong>——
          它的请求会被放行并计入未定价指标。给模型填上价格，这个上限才对它生效。
        </div>

        <button className="act" disabled={busy} onClick={() => void mint()}>
          {busy ? "保存中…" : "生成并重载网关"}
        </button>

        {/* 明文的位置在提示之上，且不随提示更新而消失。 */}
        {plaintext && (
          <div className="note">
            <strong>密钥明文——只显示这一次，请立即保存：</strong>
            <pre style={{ marginTop: 8 }}>{plaintext}</pre>
          </div>
        )}
        <Msg note={note} />
        {plaintext && note?.kind === "crit" && (
          <div className="note crit">
            保存未成功。如果这把密钥其实已经写进配置，请用上面的明文；否则请重新生成。
          </div>
        )}
      </div>

      <div className="panel">
        <h2>花费上限策略</h2>
        <p className="hint">
          上面每设一个花费上限就会生成一条策略资源。这里按原样列出全部策略，
          包括不是从本页创建的（例如按 team 作用域的）。
        </p>
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>名称</th>
                <th>作用域</th>
                <th>对象</th>
                <th>窗口</th>
                <th className="right">上限</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {policies.length === 0 ? (
                <tr>
                  <td colSpan={6} style={{ color: "var(--ink-3)" }}>
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
                      {p.max_spend_micro_usd != null ? (
                        `$${(Number(p.max_spend_micro_usd) / 1e6).toFixed(2)}`
                      ) : (
                        <span style={{ color: "var(--ink-3)" }}>非花费策略</span>
                      )}
                    </td>
                    <td className="right">
                      <button className="ghost" disabled={busy} onClick={() => void delPolicy(i)}>
                        删除
                      </button>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </div>
    </>
  );
}
