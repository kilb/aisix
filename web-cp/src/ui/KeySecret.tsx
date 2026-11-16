// 共享组件在双前端逐字节保持一致，合同见同目录 CONTRACT.md。
import { useState } from "react";
import { CopyButton } from "./CopyButton";

/** 获取明文已被拒绝，调用页负责显示错误。 */
class RevealRefused extends Error {}

/** 始终只显示遮罩与复制图标；明文仅在组件内存与剪贴板通道中流转。 */
export function KeySecret({
  prefix,
  reveal,
  unavailable,
  onError,
  className,
}: {
  prefix: string;
  reveal: () => Promise<string>;
  unavailable: string | null;
  onError: (e: unknown) => void;
  className: string;
}) {
  const [plaintext, setPlaintext] = useState<string | null>(null);
  const [clipboard, setClipboard] = useState(false);

  async function load(): Promise<string> {
    setClipboard(false);
    if (plaintext !== null) return plaintext;
    try {
      const value = await reveal();
      setPlaintext(value);
      return value;
    } catch (e) {
      onError(e);
      throw new RevealRefused();
    }
  }

  return (
    <>
      <code aria-label="密钥前缀">{prefix}…</code>
      <span className={className}>
        <CopyButton
          text={load}
          size="sm"
          label="复制密钥"
          disabled={unavailable !== null}
          disabledReason={unavailable ?? undefined}
          onFailed={(cause) => {
            if (!(cause instanceof RevealRefused)) setClipboard(true);
          }}
        />
        {clipboard && plaintext !== null && (
          <span className="hint cell-note" role="status">
            无法写入剪贴板，请允许剪贴板权限后重试。
          </span>
        )}
      </span>
    </>
  );
}
