import { useState } from "react";
import * as api from "./lib/api";
import type { Theme } from "./lib/useTheme";

/**
 * 登录闸。
 *
 * 控制台能改网关配置、能看到明文上游密钥，所以未登录不渲染任何数据 ——
 * 界面骨架也不渲染。
 *
 * 但这也是没登录的人看到的**全部**，所以它承担整个第一印象：铭牌、一排
 * 三块停在静止位的表头、以及这台网关的身份。表头空着而不是停在零 ——
 * 它们说明这台机器量的是什么，不报任何读数。
 */
export function Gate({
  onIn,
  theme,
  onToggleTheme,
}: {
  onIn: () => void;
  theme: Theme;
  onToggleTheme: () => void;
}) {
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
      <div className="gate-plate">
        <div className="gate-mark">
          <h1>Gateway Meter Room</h1>
          <span className="gate-sub">AISIX 网关控制台</span>
        </div>
        <button
          className="ghost"
          onClick={onToggleTheme}
          aria-label={theme === "dark" ? "切换到浅色" : "切换到深色"}
          title={theme === "dark" ? "切换到浅色" : "切换到深色"}
        >
          {theme === "dark" ? "浅色" : "深色"}
        </button>
      </div>

      {/* 三块停在静止位的表头，说明这台机器量的是什么。
          静止位是空的，不是零：印一个 0 出来就是在报一个没测到的数，旁边
          写「未连接」也抵不掉 —— 概览页刚为同一件事改过一遍。 */}
      <div className="gate-dials" aria-hidden="true">
        {["请求", "TOKEN", "花费"].map((l) => (
          <div key={l} className="gate-dial">
            <span className="gate-dial-lab">{l}</span>
            <span className="gate-dial-val" />
          </div>
        ))}
        <span className="gate-dial-note">未连接</span>
      </div>

      <form className="gate-form panel" onSubmit={submit}>
        {/* 单口令登录，但给密码管理器一个可锚定的用户名字段：否则浏览器会
            警告，而且凭据存不进去。 */}
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
        <button className="act" type="submit" disabled={busy}>
          {busy ? "校验中…" : "进入"}
        </button>
        {err && <div className="note crit">{err}</div>}
      </form>
    </div>
  );
}
