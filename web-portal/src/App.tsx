import { useCallback, useEffect, useState } from "react";
import * as api from "./lib/api";
import { Gate } from "./Gate";
import { Account } from "./Account";

/**
 * 门户外壳。
 *
 * 这里**没有**配置编辑、没有资源列表、没有密钥管理 —— 门户与管理控制台是两个
 * 进程、两套会话（设计文档 §5.1）。角色判断错一次就是全量泄漏；进程分开后，
 * 这份构建产物里根本不存在那些代码路径。
 */
export function App() {
  const [sess, setSess] = useState<api.Session | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setSess(await api.session());
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "读取会话失败");
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  if (err) {
    return (
      <div className="boot">
        <div className="note crit">{err}</div>
      </div>
    );
  }
  if (!sess) return <div className="boot">载入中…</div>;
  if (!sess.authed) return <Gate onIn={refresh} />;
  return <Account sess={sess} onOut={refresh} />;
}
