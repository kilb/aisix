import { useState } from "react";
import * as api from "./lib/api";

/**
 * 注册与登录。
 *
 * 两件事共用一张表单：门户面向陌生人，把「我是新用户」和「我已有账号」做成
 * 两个页面只会让人多走一步。
 */
export function Gate({ onIn }: { onIn: () => void }) {
  const [mode, setMode] = useState<"login" | "register">("login");
  const [email, setEmail] = useState("");
  const [pw, setPw] = useState("");
  const [msg, setMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setMsg(null);
    setBusy(true);
    try {
      if (mode === "register") {
        await api.register(email, pw);
      }
      await api.login(email, pw);
      onIn();
    } catch (e) {
      setMsg(e instanceof Error ? e.message : "失败");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div id="gate">
      <header className="plate">
        <h1>AISIX 用量门户</h1>
        <span className="sub">API 额度与用量</span>
      </header>

      <div className="panel">
        <div className="seg">
          <button
            type="button"
            aria-selected={mode === "login"}
            onClick={() => setMode("login")}
          >
            已有账号
          </button>
          <button
            type="button"
            aria-selected={mode === "register"}
            onClick={() => setMode("register")}
          >
            新用户
          </button>
        </div>

        <form onSubmit={submit}>
          <label className="f">
            <span>邮箱</span>
            <input
              type="email"
              autoComplete="username"
              required
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
          </label>
          <label className="f">
            <span>口令</span>
            <input
              type="password"
              autoComplete={mode === "register" ? "new-password" : "current-password"}
              required
              value={pw}
              onChange={(e) => setPw(e.target.value)}
            />
            {mode === "register" && <span className="hint">至少 12 个字符</span>}
          </label>
          <button className="act" type="submit" disabled={busy}>
            {busy ? "处理中…" : mode === "login" ? "登录" : "注册并登录"}
          </button>
        </form>

        {msg && <div className="note crit">{msg}</div>}
      </div>

      {/* 这段原本写的是「等管理员把密钥绑到你的用户 ID」—— 那是一期的形态。
          用户能自助建密钥之后，照着做只会白等。 */}
      <p className="foot">
        注册后由管理员分配额度，有额度才能调用。密钥你自己创建，用量与花费也在{""}
        这里看。
      </p>
    </div>
  );
}
