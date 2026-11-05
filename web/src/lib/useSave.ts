import { useCallback, useState } from "react";
import type { Note } from "../components/Msg";
import type { DocState } from "./useDoc";
import type { Json } from "./api";

/**
 * 面板级的保存动作：改一份文档副本 → 提交 → 把结果显示在本面板上。
 *
 * `mutate` 收到的是**副本**，不是活状态。直接改活状态再保存的话，一次被拒
 * 的保存会把那条改动留在内存里，下一次保存把它连同新改动再提交一遍 ——
 * 于是同一条被写进去两次。
 */
export function useSave(doc: DocState) {
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);

  const run = useCallback(
    async (mutate: (draft: Json) => void): Promise<boolean> => {
      if (!doc.doc) {
        setNote({
          text: "配置未能读取，拒绝保存：保存会用一份不完整的文档覆盖线上配置。",
          kind: "crit",
        });
        return false;
      }
      setBusy(true);
      const draft = structuredClone(doc.doc);
      mutate(draft);
      const { ok, message } = await doc.save(draft);
      setNote({ text: message, kind: ok ? "ok" : "crit" });
      setBusy(false);
      return ok;
    },
    [doc],
  );

  return { note, setNote, busy, run };
}
