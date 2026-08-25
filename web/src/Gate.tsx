import { useState } from "react";
import * as api from "./lib/api";

/**
 * 登录闸。控制台能改网关配置、能看到明文上游密钥，所以未登录不渲染任何
 * 数据 —— 界面骨架也不渲染。
 */
export function Gate({ onIn }: { onIn: () => void }) {
  const [pw, setPw] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    setBusy(true);
    try {
      await api.login(pw);
      setPw("");
      onIn();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "登录失败");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div id="gate">
      <div className="box panel">
        <h1>Gateway Meter Room</h1>
        <p>AISIX 网关控制台</p>
        <form onSubmit={submit}>
          {/* 单口令登录，但给密码管理器一个可锚定的用户名字段：否则浏览器
              会警告，而且凭据存不进去。 */}
          <input
            type="text"
            name="username"
            value="aisix-console"
            autoComplete="username"
            readOnly
            hidden
            aria-hidden="true"
            tabIndex={-1}
          />
          <label className="f">
            <span>口令</span>
            <input
              type="password"
              autoComplete="current-password"
              required
              autoFocus
              value={pw}
              onChange={(e) => setPw(e.target.value)}
            />
          </label>
          <button className="act" style={{ width: "100%" }} type="submit" disabled={busy}>
            {busy ? "校验中…" : "进入"}
          </button>
        </form>
        {err && <div className="note crit">{err}</div>}
      </div>
    </div>
  );
}
