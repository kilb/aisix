import { useEffect, useRef, useState } from "react";
import { Button } from "./Button";
import { Icon } from "./Icon";
import { useToast } from "./Toast";

const DONE_MS = 1_500;

/**
 * Copies `text` to the clipboard. On success the button itself says 已复制
 * with a check for 1.5s and a toast confirms it; a refusal goes to
 * `onFailed` with its cause, which the caller shows beside the text to copy
 * by hand.
 */
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
      onClick={() => void copy()}
    >
      {done ? (
        <>
          <Icon name="check" />
          已复制
        </>
      ) : (
        <>
          <Icon name="copy" />
          {label}
        </>
      )}
    </Button>
  );
}
