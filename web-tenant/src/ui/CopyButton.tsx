import { useEffect, useRef, useState } from "react";
import { Button } from "./Button";
import { useToast } from "./Toast";

const DONE_MS = 1_500;

/** 复制仅使用图标；读屏名称、提示和成功反馈保持可达。 */
export function CopyButton({
  text,
  label = "复制",
  size,
  kind = "secondary",
  disabled = false,
  disabledReason,
  onFailed,
}: {
  /**
   * The value to copy, or a request that answers it: a key's plaintext is
   * fetched only when it is copied, and a request that refuses copies
   * nothing.
   */
  text: string | (() => Promise<string>);
  label?: string;
  size?: "sm";
  kind?: "primary" | "secondary";
  /** Off, with `disabledReason` saying why: the button keeps its place in a row. */
  disabled?: boolean;
  disabledReason?: string;
  /** What went wrong: a refused request, or a clipboard the browser would not write. */
  onFailed: (cause: unknown) => void;
}) {
  const [done, setDone] = useState(false);
  const [fetching, setFetching] = useState(false);
  const toast = useToast();
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  async function copy() {
    setDone(false);
    window.clearTimeout(timer.current);
    setFetching(true);
    let value: string;
    try {
      value = typeof text === "string" ? text : await text();
    } catch (cause) {
      setFetching(false);
      onFailed(cause);
      return;
    }
    try {
      await navigator.clipboard.writeText(value);
    } catch (cause) {
      setFetching(false);
      onFailed(cause);
      return;
    }
    setFetching(false);
    setDone(true);
    toast("已复制");
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setDone(false), DONE_MS);
  }

  return (
    <Button
      type="button"
      kind={kind}
      size={size}
      disabled={disabled}
      disabledReason={disabledReason}
      busy={fetching}
      busyLabel={label}
      icon={done ? "check" : "copy"}
      iconOnly
      aria-label={done ? "已复制" : label}
      title={done ? "已复制" : label}
      onClick={() => void copy()}
    />
  );
}
