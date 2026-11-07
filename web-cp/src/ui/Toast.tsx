// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";

const SHOWN_MS = 3_000;
const LEAVE_MS = 200;
/** At most this many toasts at once; the oldest goes first. */
const MAX = 3;

export interface ToastOptions {
  /**
   * Whether assistive technology hears the toast (the default). Pass
   * `false` only when the page already says the same words in a live
   * status of its own, so they are not heard twice.
   */
  announce?: boolean;
}

type Show = (text: string, options?: ToastOptions) => void;

interface Item {
  id: number;
  text: string;
  announce: boolean;
  leaving: boolean;
}

const ToastContext = createContext<Show>(() => undefined);

/**
 * Toasts: bottom left, worded with the action's own verb (保存 → 已保存),
 * gone after three seconds. They slide up 8px and fade in over 160ms and
 * fade out over 200ms. The stack is a polite live region with no role, so
 * it never competes with a page's own `status` or `alert`.
 */
export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<Item[]>([]);
  const seq = useRef(0);
  const timers = useRef<number[]>([]);

  useEffect(() => () => timers.current.forEach((t) => window.clearTimeout(t)), []);

  const show = useCallback<Show>((text, options) => {
    const id = ++seq.current;
    const announce = options?.announce ?? true;
    setItems((xs) => [...xs.slice(-(MAX - 1)), { id, text, announce, leaving: false }]);
    timers.current.push(
      window.setTimeout(() => setItems((xs) => xs.map((x) => (x.id === id ? { ...x, leaving: true } : x))), SHOWN_MS),
      window.setTimeout(() => setItems((xs) => xs.filter((x) => x.id !== id)), SHOWN_MS + LEAVE_MS),
    );
  }, []);

  return (
    <ToastContext.Provider value={show}>
      {children}
      <div className="toasts" aria-live="polite">
        {items.map((t) => (
          <div
            key={t.id}
            className={t.leaving ? "toast is-leaving" : "toast"}
            aria-hidden={t.announce ? undefined : true}
          >
            {t.text}
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

/** Shows a toast: `toast(text)`, or `toast(text, { announce: false })`. */
export function useToast(): Show {
  return useContext(ToastContext);
}
