import { useState } from "react";
import { Msg } from "../components/Msg";
import { useSave } from "../lib/useSave";
import type { DocState } from "../lib/useDoc";

const PROVIDERS = ["openai", "anthropic", "gemini", "deepseek", "bedrock"] as const;

/** 只显示密钥的两端。`${VAR}` 形式的环境变量引用原样显示 —— 它不是秘密。 */
function maskKey(k: unknown): string {
  const s = String(k ?? "");
  if (!s) return "—";
  if (s.startsWith("${")) return s;
  return s.length <= 10 ? "••••" : `${s.slice(0, 5)}…${s.slice(-4)}`;
}

export function Providers({ doc }: { doc: DocState }) {
  const { note, setNote, busy, run } = useSave(doc);
  const [name, setName] = useState("");
  const [provider, setProvider] = useState<string>("openai");
  const [key, setKey] = useState("");
  const [base, setBase] = useState("");

  const pks = (doc.doc?.provider_keys as Record<string, unknown>[] | undefined) ?? [];

  async function add() {
    if (!name.trim() || !key.trim()) {
      setNote({ text: "名称和密钥都不能为空", kind: "crit" });
      return;
    }
    const item: Record<string, unknown> = {
      display_name: name.trim(),
      api_key: key.trim(),
      provider,
    };
    if (base.trim()) item.api_base = base.trim();
    const ok = await run((d) => {
      const list = (d.provider_keys as unknown[] | undefined) ?? [];
      d.provider_keys = [...list, item];
    });
    if (ok) {
      setName("");
      setKey("");
      setBase("");
    }
  }

  async function del(i: number) {
    if (!confirm("确认删除？会立即重写配置并重载网关。")) return;
    await run((d) => {
      const list = [...((d.provider_keys as unknown[] | undefined) ?? [])];
      list.splice(i, 1);
      d.provider_keys = list;
    });
  }

  return (
    <>
      <div className="panel">
        <h2>上游供应商</h2>
        <p className="hint">
          保存会重写声明式配置并向网关发 SIGHUP。写盘前先跑网关自带的{" "}
          <code>aisix validate</code>，校验不过则不改动任何东西。
        </p>
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>名称</th>
                <th>供应商</th>
                <th>密钥</th>
                <th>API 基址</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {pks.length === 0 ? (
                <tr>
                  <td colSpan={5} style={{ color: "var(--muted)" }}>
                    还没有配置供应商。
                  </td>
                </tr>
              ) : (
                pks.map((p, i) => (
                  <tr key={`${String(p.display_name)}-${i}`}>
                    <td>
                      <strong>{String(p.display_name ?? "")}</strong>
                    </td>
                    <td>{String(p.provider ?? "openai")}</td>
                    <td className="num" style={{ fontSize: 12 }}>
                      {maskKey(p.api_key)}
                    </td>
                    <td className="num" style={{ fontSize: 12 }}>
                      {String(p.api_base ?? "（默认）")}
                    </td>
                    <td className="r">
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
      </div>

      <div className="panel">
        <h2>新增供应商</h2>
        <div className="grid g2">
          <label className="f">
            <span>名称</span>
            <input
              type="text"
              placeholder="openai-main"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          <label className="f">
            <span>供应商</span>
            <select value={provider} onChange={(e) => setProvider(e.target.value)}>
              {PROVIDERS.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))}
            </select>
          </label>
          <label className="f">
            <span>API 密钥</span>
            <input
              type="text"
              placeholder="sk-..."
              value={key}
              onChange={(e) => setKey(e.target.value)}
            />
          </label>
          <label className="f">
            <span>API 基址（留空用官方）</span>
            <input
              type="text"
              placeholder="https://api.openai.com/v1"
              value={base}
              onChange={(e) => setBase(e.target.value)}
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
