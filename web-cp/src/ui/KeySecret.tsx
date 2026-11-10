// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import { useState } from "react";
import { Button } from "./Button";
import { CopyButton } from "./CopyButton";

/** A request for the plaintext that refused: the page shows its own text for it. */
class RevealRefused extends Error {}

/**
 * A key's plaintext, by its prefix: `显示` swaps the prefix for the value
 * the control plane hands out on demand, and `复制` fetches and copies it in
 * one click. The value lives in this component's state only, so a reload or
 * a change of page drops it; nothing here stores, logs or routes it.
 *
 * `reveal` is the request that answers the plaintext. `unavailable` says why
 * there is none — a key minted before the platform stored one — and the copy
 * button is then off, with that reason in its tooltip. A refused reveal goes
 * to `onError`, which the page shows; a clipboard that would not take the
 * value is said here, under the buttons.
 */
export function KeySecret({
  prefix,
  reveal,
  unavailable,
  onError,
  className,
}: {
  /** The key's stored prefix, shown until the plaintext is. */
  prefix: string;
  reveal: () => Promise<string>;
  /** Why the plaintext cannot be shown, or null when it can. */
  unavailable: string | null;
  /** A refused reveal, which the page shows. */
  onError: (e: unknown) => void;
  /** The page's own action group, so the buttons sit as its other rows' do. */
  className: string;
}) {
  const [plaintext, setPlaintext] = useState<string | null>(null);
  const [clipboard, setClipboard] = useState(false);

  /** The plaintext, kept here once fetched; a failure goes to `onError` and is not retried by itself. */
  async function load(): Promise<string> {
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
      <code aria-label="密钥明文">{plaintext ?? `${prefix}…`}</code>
      <span className={className}>
        <CopyButton
          text={load}
          size="sm"
          disabled={unavailable !== null}
          disabledReason={unavailable ?? undefined}
          onFailed={(cause) => {
            // A refused reveal has said so on the page already.
            if (!(cause instanceof RevealRefused)) setClipboard(true);
          }}
        />
        {unavailable === null && (
          <Button
            type="button"
            size="sm"
            onClick={() => {
              if (plaintext !== null) {
                setPlaintext(null);
                return;
              }
              load().catch(() => {});
            }}
          >
            {plaintext !== null ? "隐藏" : "显示"}
          </Button>
        )}
        {clipboard && (
          <span className="hint cell-note" role="status">
            无法写入剪贴板，请点“显示”后手动选中复制
          </span>
        )}
      </span>
    </>
  );
}
