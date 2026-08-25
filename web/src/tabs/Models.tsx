import { useMemo, useState } from "react";
import * as api from "../lib/api";
import { Msg } from "../components/Msg";
import { useSave } from "../lib/useSave";
import type { DocState } from "../lib/useDoc";

/**
 * 参考折扣：上游对缓存 token 的计价相对于输入价的倍率。
 *
 * 这些是各家公开文档写明的倍率，填价时用来一键换算，**不是网关行为** ——
 * 网关只按 `cost` 里填的数字算钱。
 */
const CACHE_FACTORS: Record<string, { read: number; write: number; note: string }> = {
  anthropic: { read: 0.1, write: 1.25, note: "读 0.1×、5 分钟写 1.25×（1 小时写为 2×）" },
  openai: { read: 0.5, write: 1.0, note: "缓存输入约为输入价的一半" },
  deepseek: { read: 0.1, write: 1.0, note: "命中缓存约为未命中的十分之一" },
  gemini: { read: 0.25, write: 1.0, note: "缓存读取按折扣计，另有存储费" },
};

function trimNum(n: number): string {
  return n.toFixed(8).replace(/0+$/, "").replace(/\.$/, "");
}

function parseOrNull(s: string): number | null {
  const v = parseFloat(s);
  return Number.isNaN(v) ? null : v;
}

function Price({ v }: { v: unknown }) {
  if (v == null) return <span style={{ color: "var(--muted)" }}>未设</span>;
  return <>${Number(v).toFixed(5)}</>;
}

export function Models({ doc }: { doc: DocState }) {
  const { note, setNote, busy, run } = useSave(doc);
  const models = (doc.doc?.models as Record<string, unknown>[] | undefined) ?? [];
  const pks = (doc.doc?.provider_keys as Record<string, unknown>[] | undefined) ?? [];

  const [name, setName] = useState("");
  const [pk, setPk] = useState("");
  const [provider, setProvider] = useState("openai");
  const [upstream, setUpstream] = useState("");
  const [priceIn, setPriceIn] = useState("");
  const [priceOut, setPriceOut] = useState("");
  const [cacheR, setCacheR] = useState("");
  const [cacheW, setCacheW] = useState("");
  const [mul, setMul] = useState("1");
  /**
   * 缓存两格是否还归自动换算所有。
   *
   * 旧界面把它放在模块作用域，于是手改过一次之后，本次会话里后续新增的每个
   * 模型都静默失去换算 —— 用户填了输入价，缓存两栏一直空着，缓存 token 全
   * 按输入价计费，界面上看不出异常。这里它是组件状态，随表单一起存在。
   */
  const [factorsOwned, setFactorsOwned] = useState(true);

  const factor = CACHE_FACTORS[provider] ?? { read: 1, write: 1, note: "" };
  const mulNum = useMemo(() => {
    const m = parseFloat(mul);
    return Number.isNaN(m) || m < 0 ? 1 : m;
  }, [mul]);

  /** 输入价一变就按倍率重算缓存两项 —— 但只在它们还归自动换算时。 */
  function onInputPrice(v: string) {
    setPriceIn(v);
    const base = parseFloat(v);
    if (!Number.isNaN(base) && base >= 0 && factorsOwned) {
      setCacheR(trimNum(base * factor.read * mulNum));
      setCacheW(trimNum(base * factor.write * mulNum));
    }
  }

  function onMul(v: string) {
    setMul(v);
    const m = parseFloat(v);
    const base = parseFloat(priceIn);
    const eff = Number.isNaN(m) || m < 0 ? 1 : m;
    if (!Number.isNaN(base) && base >= 0 && factorsOwned) {
      setCacheR(trimNum(base * factor.read * eff));
      setCacheW(trimNum(base * factor.write * eff));
    }
  }

  async function add() {
    if (!name.trim() || !upstream.trim()) {
      setNote({ text: "模型名和上游模型名都不能为空", kind: "crit" });
      return;
    }
    const i = parseOrNull(priceIn);
    const o = parseOrNull(priceOut);
    const cr = parseOrNull(cacheR);
    const cw = parseOrNull(cacheW);

    // 只填一半的定价此前被整块丢掉：模型于是变成未定价，而未定价的模型
    // 不计入任何花费上限 —— 用户填了价、表单也接受了，得到的却是一个静默
    // 豁免于自己预算的模型。宁可拦下来让他填全。
    if ([i, o, cr, cw].some((v) => v != null) && (i == null || o == null)) {
      setNote({
        text:
          "填了定价就必须同时给出输入价和输出价——只给一半的话网关会把这个模型当作未定价，" +
          "它的花费不计入任何预算上限。",
        kind: "crit",
      });
      return;
    }

    const item: Record<string, unknown> = {
      display_name: name.trim(),
      provider,
      model_name: upstream.trim(),
      provider_key: pk || String(pks[0]?.display_name ?? ""),
    };
    if (i != null && o != null) {
      const cost: Record<string, number> = { input_per_1k: i, output_per_1k: o };
      if (cr != null) cost.cached_input_per_1k = cr;
      if (cw != null) cost.cache_write_per_1k = cw;
      item.cost = cost;
    }
    const ok = await run((d) => {
      d.models = [...((d.models as unknown[] | undefined) ?? []), item];
    });
    if (ok) {
      setName("");
      setUpstream("");
      setPriceIn("");
      setPriceOut("");
      setCacheR("");
      setCacheW("");
      setFactorsOwned(true);
    }
  }

  async function del(i: number) {
    if (!confirm("确认删除？会立即重写配置并重载网关。")) return;
    await run((d) => {
      const list = [...((d.models as unknown[] | undefined) ?? [])];
      list.splice(i, 1);
      d.models = list;
    });
  }

  return (
    <>
      <div className="panel">
        <h2>模型与定价</h2>
        <p className="hint">
          这里的价就是网关折算花费用的数字（USD / 千 token）。没填价的模型，花费统计恒为
          0 —— 不是没花钱，是没有价可算。
        </p>
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>模型</th>
                <th>供应商</th>
                <th>上游名</th>
                <th className="r">输入</th>
                <th className="r">输出</th>
                <th className="r">缓存读</th>
                <th className="r">缓存写</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {models.length === 0 ? (
                <tr>
                  <td colSpan={8} style={{ color: "var(--muted)" }}>
                    还没有配置模型。
                  </td>
                </tr>
              ) : (
                models.map((m, i) => {
                  const c = m.cost as Record<string, unknown> | undefined;
                  return (
                    <tr key={`${String(m.display_name)}-${i}`}>
                      <td>
                        <strong>{String(m.display_name ?? "")}</strong>
                      </td>
                      <td>{String(m.provider ?? "")}</td>
                      <td className="num" style={{ fontSize: 12 }}>
                        {String(m.model_name ?? "")}
                      </td>
                      <td className="r num">
                        <Price v={c?.input_per_1k} />
                      </td>
                      <td className="r num">
                        <Price v={c?.output_per_1k} />
                      </td>
                      <td className="r num">
                        <Price v={c?.cached_input_per_1k} />
                      </td>
                      <td className="r num">
                        <Price v={c?.cache_write_per_1k} />
                      </td>
                      <td className="r">
                        <button className="ghost" disabled={busy} onClick={() => void del(i)}>
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
        <div className="note">
          缓存读/写未设时，网关按<strong>输入价</strong>计缓存 token。
          上游对缓存的计价是打折（读）和加价（写）的，所以不填这两项，缓存流量的花费统计会偏。
        </div>
      </div>

      <SyncPanel doc={doc} pks={pks} />

      <div className="panel">
        <h2>新增模型</h2>
        {pks.length === 0 && (
          <div className="note warn">先添加一个供应商，模型需要绑定到它。</div>
        )}
        <div className="grid g2">
          <label className="f">
            <span>对外模型名</span>
            <input
              type="text"
              placeholder="gpt-4o-mini"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <label className="f">
            <span>绑定供应商</span>
            <select value={pk} onChange={(e) => setPk(e.target.value)}>
              {pks.map((p) => (
                <option key={String(p.display_name)} value={String(p.display_name)}>
                  {String(p.display_name)}
                </option>
              ))}
            </select>
          </label>
          <label className="f">
            <span>供应商类型</span>
            <select value={provider} onChange={(e) => setProvider(e.target.value)}>
              {Object.keys(CACHE_FACTORS).map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
          </label>
          <label className="f">
            <span>上游模型名</span>
            <input
              type="text"
              placeholder="gpt-4o-mini"
              value={upstream}
              onChange={(e) => setUpstream(e.target.value)}
            />
          </label>
        </div>

        <h3 style={{ fontSize: 13, margin: "18px 0 10px" }}>定价</h3>
        <div className="grid g2">
          <label className="f">
            <span>输入 USD / 1k</span>
            <input
              type="number"
              step="0.00001"
              min="0"
              placeholder="0.00015"
              value={priceIn}
              onChange={(e) => onInputPrice(e.target.value)}
            />
          </label>
          <label className="f">
            <span>输出 USD / 1k</span>
            <input
              type="number"
              step="0.00001"
              min="0"
              placeholder="0.0006"
              value={priceOut}
              onChange={(e) => setPriceOut(e.target.value)}
            />
          </label>
          <label className="f">
            <span>缓存读 USD / 1k</span>
            <input
              type="number"
              step="0.000001"
              min="0"
              value={cacheR}
              onChange={(e) => {
                setFactorsOwned(false);
                setCacheR(e.target.value);
              }}
            />
          </label>
          <label className="f">
            <span>缓存写 USD / 1k</span>
            <input
              type="number"
              step="0.000001"
              min="0"
              value={cacheW}
              onChange={(e) => {
                setFactorsOwned(false);
                setCacheW(e.target.value);
              }}
            />
          </label>
        </div>
        <div className="note">
          <strong>{provider}</strong> 的公开缓存倍率：{factor.note}
          。填好输入价后自动换算；系数 {mulNum} 会同时乘到四个价上。
          这些倍率是上游的计价规则，不是网关行为。
        </div>
        <label className="f" style={{ maxWidth: 320 }}>
          <span>加价 / 折扣系数（对全部四个价生效）</span>
          <input
            type="number"
            step="0.01"
            min="0"
            value={mul}
            onChange={(e) => onMul(e.target.value)}
          />
        </label>
        <button className="act" disabled={busy || pks.length === 0} onClick={() => void add()}>
          {busy ? "保存中…" : "保存并重载网关"}
        </button>
        <Msg note={note} />
      </div>
    </>
  );
}

/** 从上游拉模型清单，勾选接入或改用通配符一次接全部。 */
function SyncPanel({
  doc,
  pks,
}: {
  doc: DocState;
  pks: Record<string, unknown>[];
}) {
  const { note, setNote, busy, run } = useSave(doc);
  const [syncPk, setSyncPk] = useState("");
  const [list, setList] = useState<{ provider: string; models: string[] } | null>(null);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const have = new Set(
    ((doc.doc?.models as Record<string, unknown>[] | undefined) ?? []).map((m) =>
      String(m.model_name ?? ""),
    ),
  );
  const effectivePk = syncPk || String(pks[0]?.display_name ?? "");
  const providerOf = (n: string) =>
    String(pks.find((p) => String(p.display_name) === n)?.provider ?? "openai");

  async function pull() {
    setErr(null);
    setList(null);
    setPicked(new Set());
    setLoading(true);
    try {
      const models = await api.upstreamModels(effectivePk);
      setList({ provider: providerOf(effectivePk), models });
    } catch (e) {
      setErr(e instanceof Error ? e.message : "拉取失败");
    } finally {
      setLoading(false);
    }
  }

  async function addPicked() {
    if (picked.size === 0) {
      setNote({ text: "没有勾选任何模型", kind: "crit" });
      return;
    }
    const provider = list?.provider ?? "openai";
    const ok = await run((d) => {
      const list0 = [...((d.models as unknown[] | undefined) ?? [])];
      for (const id of picked) {
        list0.push({
          display_name: id,
          provider,
          model_name: id,
          provider_key: effectivePk,
        });
      }
      d.models = list0;
    });
    if (ok) setPicked(new Set());
  }

  async function addWildcard() {
    const provider = list?.provider ?? "openai";
    const alias = `${provider}/*`;
    const existing = (doc.doc?.models as Record<string, unknown>[] | undefined) ?? [];
    if (existing.some((m) => m.display_name === alias)) {
      setNote({ text: "该通配符行已存在", kind: "crit" });
      return;
    }
    await run((d) => {
      d.models = [
        ...((d.models as unknown[] | undefined) ?? []),
        { display_name: alias, provider, model_name: "*", provider_key: effectivePk },
      ];
    });
  }

  return (
    <div className="panel">
      <h2>从上游同步模型</h2>
      <p className="hint">网关不知道上游有哪些模型 —— 它只按配置转发。这里直接问上游要清单。</p>
      {pks.length === 0 ? (
        <div className="note warn">先添加一个供应商。</div>
      ) : (
        <>
          <div className="grid g2">
            <label className="f">
              <span>供应商</span>
              <select value={syncPk} onChange={(e) => setSyncPk(e.target.value)}>
                {pks.map((p) => (
                  <option key={String(p.display_name)} value={String(p.display_name)}>
                    {String(p.display_name)}
                  </option>
                ))}
              </select>
            </label>
          </div>
          <button className="act" disabled={loading} onClick={() => void pull()}>
            {loading ? "拉取中…" : "拉取清单"}
          </button>
        </>
      )}

      <div style={{ marginTop: 14 }}>
        {err && <div className="note crit">{err}</div>}
        {list && (
          <>
            <p className="hint">
              上游报告 {list.models.length} 个模型。勾选要接入的，或用下方的通配符一次性全接。
            </p>
            <div className="scroll" style={{ maxHeight: 320 }}>
              <table>
                <thead>
                  <tr>
                    <th style={{ width: 36 }} />
                    <th>上游模型 id</th>
                    <th>状态</th>
                  </tr>
                </thead>
                <tbody>
                  {list.models.map((id) => (
                    <tr key={id}>
                      <td>
                        <input
                          type="checkbox"
                          disabled={have.has(id)}
                          checked={picked.has(id)}
                          onChange={(e) => {
                            const next = new Set(picked);
                            if (e.target.checked) next.add(id);
                            else next.delete(id);
                            setPicked(next);
                          }}
                        />
                      </td>
                      <td className="num" style={{ fontSize: 12 }}>
                        {id}
                      </td>
                      <td>
                        {have.has(id) ? (
                          <span style={{ color: "var(--ok)" }}>已接入</span>
                        ) : (
                          <span style={{ color: "var(--muted)" }}>未接入</span>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div style={{ marginTop: 12, display: "flex", gap: 8, flexWrap: "wrap" }}>
              <button className="act" disabled={busy} onClick={() => void addPicked()}>
                接入勾选的
              </button>
              <button className="ghost" disabled={busy} onClick={() => void addWildcard()}>
                改用通配符一次接全部
              </button>
            </div>
            <div className="note">
              通配符行 <code>{list.provider}/*</code> 一行服务上游<strong>所有</strong>模型，
              <code>cost</code>、限流、护栏都从这一行继承。代价：通配符行
              <strong>不会出现在 <code>/v1/models</code> 列表里</strong>，客户端发现不到具体型号。
            </div>
          </>
        )}
        <Msg note={note} />
      </div>
    </div>
  );
}
