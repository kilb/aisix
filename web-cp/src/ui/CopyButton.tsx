import { useEffect, useRef, useState } from "react";
import { Button } from "./Button";
import { Icon } from "./Icon";
import { useToast } from "./Toast";

const DONE_MS = 1_500;

/**
 * Copies `text` to the clipboard. On success the button itself says 已复制
 * with a check for 1.5s and a toast confirms it; a refusal goes to
 * `onFailed`, which the caller shows beside the text to copy by hand.
 */
export function CopyButton({
  text,
  label = "复制",
  size,
  kind = "secondary",
  onFailed,
}: {
  text: string;
  label?: string;
  size?: "sm";
  kind?: "primary" | "secondary";
  onFailed: () => void;
}) {
  const [done, setDone] = useState(false);
  const toast = useToast();
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      onFailed();
      return;
    }
    setDone(true);
    toast("已复制");
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setDone(false), DONE_MS);
  }

  return (
    <Button type="button" kind={kind} size={size} onClick={() => void copy()}>
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
