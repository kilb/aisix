export interface PublicModelWriteGate {
  busy: boolean;
  begin: () => boolean;
  end: () => void;
}

/** ref 同步准入，state 回调只更新界面；重绘前的第二入口也不能覆盖已发写入。 */
export function createPublicModelWriteGate(active: { current: boolean }, changed: (busy: boolean) => void): Omit<PublicModelWriteGate, "busy"> {
  return {
    begin: () => {
      if (active.current) return false;
      active.current = true;
      changed(true);
      return true;
    },
    end: () => {
      active.current = false;
      changed(false);
    },
  };
}

/** 写入及读回共用同一占用周期，拒绝、回调抛错或提前返回都释放 gate。 */
export async function runPublicModelWrite(gate: Pick<PublicModelWriteGate, "begin" | "end">, write: () => Promise<void>): Promise<boolean> {
  if (!gate.begin()) return false;
  try {
    await write();
    return true;
  } finally {
    gate.end();
  }
}
