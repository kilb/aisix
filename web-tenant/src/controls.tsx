import { useState, type ReactNode } from "react";

/**
 * A destructive action in two steps, inline: `label`, then `确认<label>`
 * beside `取消`, with `warning` between them. Never `window.confirm`, which
 * leaves the page's own flow and is blocked in some embeddings.
 */
export function ConfirmButton({
  label,
  warning,
  disabled,
  onConfirm,
}: {
  label: string;
  warning?: string;
  disabled?: boolean;
  onConfirm: () => void;
}) {
  const [asking, setAsking] = useState(false);
  if (!asking) {
    return (
      <button className="ghost" disabled={disabled} onClick={() => setAsking(true)}>
        {label}
      </button>
    );
  }
  return (
    <span className="acts">
      {warning && <span className="hint warn-inline">{warning}</span>}
      <button
        className="ghost danger"
        disabled={disabled}
        onClick={() => {
          setAsking(false);
          onConfirm();
        }}
      >
        确认{label}
      </button>
      <button className="ghost" onClick={() => setAsking(false)}>
        取消
      </button>
    </span>
  );
}

/**
 * A secret shown this once (a key's plaintext, an invitation link), with
 * `复制`. It lives in the caller's state only, so it goes with `onDone`, a
 * reload or a change of page; nothing here stores, logs or routes it.
 */
export function ShownOnce({
  title,
  value,
  children,
  onDone,
}: {
  /** The notice's heading, which also names the region. */
  title: string;
  value: string;
  children?: ReactNode;
  onDone: () => void;
}) {
  const [copied, setCopied] = useState<boolean | null>(null);

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
    } catch {
      // No clipboard (an insecure context, a denied permission): the value
      // is still there to select by hand.
      setCopied(false);
    }
  }

  return (
    <div className="note warn minted" role="region" aria-label={title}>
      <strong>{title}</strong>
      <input readOnly value={value} aria-label={title} onFocus={(e) => e.currentTarget.select()} />
      {children}
      <span className="acts">
        <button className="ghost" onClick={() => void copy()}>
          复制
        </button>
        <button className="ghost" onClick={onDone}>
          我已保存
        </button>
        {copied !== null && (
          <span className="hint" role="status">
            {copied ? "已复制" : "无法写入剪贴板，请手动选中复制"}
          </span>
        )}
      </span>
    </div>
  );
}
