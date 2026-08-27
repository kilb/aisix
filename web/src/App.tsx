import { useCallback, useEffect, useState } from "react";
import * as api from "./lib/api";
import { EXPECTED_API_CONTRACT } from "./lib/contract";
import { useDoc } from "./lib/useDoc";
import { useTheme } from "./lib/useTheme";
import { Gate } from "./Gate";
import { Overview } from "./tabs/Overview";
import { Usage } from "./tabs/Usage";
import { Providers } from "./tabs/Providers";
import { Models } from "./tabs/Models";
import { Keys } from "./tabs/Keys";
import { Limits } from "./tabs/Limits";
import { Resources } from "./tabs/Resources";
import { Logs } from "./tabs/Logs";
import { Users } from "./tabs/Users";
import { Raw } from "./tabs/Raw";

const TABS = [
  ["overview", "概览"],
  ["usage", "用量"],
  ["providers", "供应商"],
  ["models", "模型与定价"],
  ["keys", "调用方密钥"],
  ["limits", "限流与预算"],
  ["resources", "全部资源"],
  ["users", "门户用户"],
  ["logs", "调用日志"],
  ["raw", "配置原文"],
] as const;

type TabId = (typeof TABS)[number][0];

export function App() {
  // null = 还不知道有没有登录。不能默认 false：那会让每次刷新都闪一下
  // 登录框，而绝大多数刷新是已登录的。
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [apiContract, setApiContract] = useState<number | null>(null);
  const [tab, setTab] = useState<TabId>("overview");
  const [theme, toggleTheme] = useTheme();
  const skewed = apiContract !== null && apiContract !== EXPECTED_API_CONTRACT;
  // 偏移时连读都不发：界面对响应形状的假设已经不成立，读回来的东西怎么
  // 渲染都是猜。
  const doc = useDoc(authed === true && !skewed);

  useEffect(() => {
    void api.session().then((s) => {
      setAuthed(s.authed);
      setApiContract(s.apiContract);
    });
  }, []);

  const signOut = useCallback(async () => {
    await api.logout();
    setAuthed(false);
  }, []);

  if (authed === null || apiContract === null) return <div className="boot" />;

  // 偏移时停止一切操作，而不是显示一条横幅继续让人编辑。
  //
  // 具体后果不是「界面某处显示不对」：一个不带 `base_version` 的旧界面会
  // 落进后端「缺版本 = 不检查」的逃生口，于是丢失更新静默回来 —— 两个标签
  // 页各改一处，后保存的整份覆盖先保存的，被覆盖的可能是一次密钥吊销。
  if (skewed) {
    const stale = apiContract < EXPECTED_API_CONTRACT ? "后端" : "界面";
    return (
      <div className="centered">
        <div className="note crit" style={{ margin: "24px" }}>
          <strong>界面与后端的接口版本不一致，已停止一切操作。</strong>
          <p>
            界面期望契约 v{EXPECTED_API_CONTRACT}，后端报告 v{apiContract}
            ——{stale}是旧的那一侧，需要更新它。
          </p>
          <p className="hint">
            两者是分开部署的：界面是 <code>web/</code> 的构建产物（由 nginx
            托管），后端是 <code>aisix-console</code> 二进制。只更新了一侧就会
            出现这个状态。
          </p>
          <p className="hint">
            这里不允许继续编辑，是因为旧界面可能不带并发校验字段，那会让
            「两个标签页同时改配置、后保存的静默覆盖先保存的」重新发生。
          </p>
        </div>
      </div>
    );
  }

  if (!authed) return <Gate onIn={() => setAuthed(true)} theme={theme} onToggleTheme={toggleTheme} />;

  const status = doc.res?.model_status as { error?: string } | undefined;
  const healthy = !!status && !status.error;

  return (
    <div className="frame">
      {/* 账簿的页眉：册名在左，戳记和出口在右，底下一道重线。页签横排在
          重线下面 —— 单栏账页要的是列宽，不是左边那条竖栏。 */}
      <header className="book-head">
        <div className="book-mark">
          <h1>Gateway Meter Room</h1>
          <span className="book-sub">AISIX 网关控制台</span>
        </div>

        <div className="book-foli">
          <span className={`pill ${doc.res ? (healthy ? "ok" : "crit") : ""}`}>
            <span className="dot" />
            <span>{doc.res ? (healthy ? "网关在线" : "网关不可达") : "检查中"}</span>
          </span>
          <button
            className="ghost"
            onClick={toggleTheme}
            aria-label={theme === "dark" ? "切换到浅色" : "切换到深色"}
            title={theme === "dark" ? "切换到浅色" : "切换到深色"}
          >
            {theme === "dark" ? "浅色" : "深色"}
          </button>
          <button className="ghost" onClick={signOut}>
            登出
          </button>
        </div>
      </header>

      <div className="leafs">
      <nav className="tabs" role="tablist" aria-label="控制台分区">
        {TABS.map(([id, label]) => (
          <button
            key={id}
            role="tab"
            data-tab={id}
            aria-selected={tab === id}
            onClick={() => setTab(id)}
          >
            {label}
          </button>
        ))}
      </nav>

      <main>
        {/* 读取失败时这条横幅必须挡在所有编辑之前：在配置能被正常读取之前
            保存任何改动，都会用一份不完整的文档覆盖线上配置。 */}
        {doc.loadError && (
          <div className="note crit" style={{ marginTop: 0 }}>
            <strong>读取配置失败，已停止一切编辑操作。</strong> {doc.loadError}
          </div>
        )}

        <section id={`tab-${tab}`}>
          {tab === "overview" && <Overview doc={doc} onGoto={setTab} />}
          {tab === "usage" && <Usage doc={doc} />}
          {tab === "providers" && <Providers doc={doc} />}
          {tab === "models" && <Models doc={doc} />}
          {tab === "keys" && <Keys doc={doc} />}
          {tab === "limits" && <Limits doc={doc} />}
          {tab === "resources" && <Resources doc={doc} onGoto={setTab} />}
          {tab === "users" && <Users />}
          {tab === "logs" && <Logs doc={doc} />}
          {tab === "raw" && <Raw doc={doc} />}
        </section>
      </main>
      </div>
    </div>
  );
}

export type { TabId };
