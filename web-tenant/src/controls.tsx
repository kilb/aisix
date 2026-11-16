import { useEffect, useState, type ReactNode } from "react";
import { Button } from "./ui/Button";
import { useToast } from "./ui/Toast";

/** How long the copy button reads 已复制. */
const COPIED_MS = 1500;

/**
 * A secret shown this once (a key's plaintext, an invitation link), with
 * `复制`. It lives in the caller's state only, so it goes with `onDone`, a
 * reload or a change of page; nothing here stores, logs or routes it.
 *
 * A copy is confirmed three ways: the button itself reads 已复制 with a
 * check for a moment, a toast says it, and the status text beside the
 * buttons (which assistive technology reads) says it too, out of sight once
 * it worked. A failed copy says so on screen, since the value then has to
 * be selected by hand.
 */
export function ShownOnce({
  title,
  value,
  masked,
  children,
  onDone,
}: {
  /** The notice's heading, which also names the region. */
  title: string;
  value: string;
  masked: boolean;
  children?: ReactNode;
  onDone: () => void;
}) {
  const [copied, setCopied] = useState<boolean | null>(null);
  const [flash, setFlash] = useState(false);
  const toast = useToast();

  useEffect(() => {
    if (!flash) return;
    const t = window.setTimeout(() => setFlash(false), COPIED_MS);
    return () => window.clearTimeout(t);
  }, [flash]);

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      setFlash(true);
      // The status beside the buttons says it to assistive technology already.
      toast("已复制", { announce: false });
    } catch {
      // 复制失败不解除密钥遮罩；邀请链接保留手动复制入口。
      setCopied(false);
    }
  }

  return (
    <div className="secret tenant-shown-once" role="region" aria-label={title}>
      <strong>{title}</strong>
      {masked ? <code className="secret-mask" aria-label="密钥已隐藏，请使用复制按钮获取">••••••••••••••••</code> : <input readOnly value={value} aria-label={title} onFocus={(e) => e.currentTarget.select()} />}
      {children}
      <span className="secret-acts">
        <Button size="sm" icon={flash ? "check" : "copy"} aria-label={flash ? "已复制" : "复制"} title={flash ? "已复制" : "复制"} onClick={() => void copy()} />
        <Button size="sm" onClick={onDone}>
          我已保存
        </Button>
        {copied !== null && (
          <span className={copied ? "vh" : "field-problem"} role="status">
            {copied ? "已复制" : masked ? "无法写入剪贴板，请允许剪贴板权限后重试" : "无法写入剪贴板，请手动选中复制"}
          </span>
        )}
      </span>
    </div>
  );
}
