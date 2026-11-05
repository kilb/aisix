import { useCallback, useEffect, useState } from "react";
import * as api from "./api";
import type { Json } from "./api";

export interface DocState {
  /** 资源快照（管理 API 只读视图）。 */
  res: Json | null;
  /** 可编辑的 resources.yaml 文档。读失败时为 null。 */
  doc: Json | null;
  /** 文档原文，供「配置原文」页。 */
  raw: string;
  /** 本次编辑所基于的磁盘版本。读失败时为空串。 */
  version: string;
  /** 读取失败的原因。非空时界面必须停止一切编辑。 */
  loadError: string | null;
  reload: () => Promise<void>;
  /**
   * 保存整份文档：写入 → 无论成败都以磁盘为准重载 → 返回结果。
   *
   * 重载放在返回之前，是因为失败时它就是回滚。顺序反了的话调用方拿到
   * 的 doc 还是被拒的那份，下一次保存会把它连同新改动再提交一遍。
   */
  save: (next: Json) => Promise<SaveOutcome>;
  saveRawText: (text: string) => Promise<SaveOutcome>;
}

export interface SaveOutcome {
  ok: boolean;
  message: string;
  /** 因版本冲突被拒。界面重载之后，用户手上那份改动已经不在编辑框里了。 */
  stale?: boolean;
  /** 文件写了（`ok` 为真，表单该清空），但网关没收到重载信号，还没生效。 */
  unapplied?: boolean;
}

function describe(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 照实说网关的状态：信号送到不等于它收下了。 */
function reloadMessage(r: api.SaveResult): string {
  const said = (() => {
    switch (r.reload) {
      case "applied":
        return "已保存，网关已重载。";
      case "signal_failed":
        return "已保存到文件，但没能通知网关重载，网关仍在用旧配置：执行 systemctl reload aisix（或重启网关）之后才生效。";
      default:
        return "已保存，已通知网关重载（未确认是否用上）。";
    }
  })();
  return r.warnings ? `${said}\n校验器提醒：${r.warnings}` : said;
}

export function useDoc(enabled: boolean): DocState {
  const [res, setRes] = useState<Json | null>(null);
  const [doc, setDoc] = useState<Json | null>(null);
  const [raw, setRaw] = useState("");
  const [version, setVersion] = useState("");
  const [loadError, setLoadError] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try {
      const [r, d] = await Promise.all([api.resources(), api.loadDoc()]);
      setRes(r);
      setDoc(d.doc);
      setRaw(d.raw);
      setVersion(d.version);
      setLoadError(null);
    } catch (e) {
      // 绝不退化成一份空文档：那样界面显示「还没有配置任何东西」，运维加
      // 一条再保存，这份只含一条的文档就覆盖了真实配置。版本也一并作废,
      // 否则读失败后的保存会带着过期版本，磁盘恰好没变时它就被接受了。
      setDoc(null);
      setVersion("");
      setLoadError(describe(e));
    }
  }, []);

  useEffect(() => {
    if (enabled) void reload();
  }, [enabled, reload]);

  const commit = useCallback(
    async (write: () => Promise<api.SaveResult>): Promise<SaveOutcome> => {
      let ok = false;
      let stale = false;
      let unapplied = false;
      let message: string;
      try {
        const saved = await write();
        message = reloadMessage(saved);
        unapplied = saved.reload === "signal_failed";
        ok = true;
      } catch (e) {
        message = describe(e);
        stale = e instanceof api.ApiError && e.stale;
      }
      // 成功是确认，失败是回滚 —— 两种情况都要以磁盘为准。
      await reload();
      return { ok, message, stale, unapplied };
    },
    [reload],
  );

  const save = useCallback(
    (next: Json) => commit(() => api.saveDoc({ _format_version: "1", ...next }, version)),
    [commit, version],
  );

  const saveRawText = useCallback(
    (text: string) => commit(() => api.saveRaw(text, version)),
    [commit, version],
  );

  return { res, doc, raw, version, loadError, reload, save, saveRawText };
}
