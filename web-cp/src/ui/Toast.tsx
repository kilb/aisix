import { createContext, useCallback, useContext, useRef, useState, type ReactNode } from "react";

const SHOWN_MS = 3_000;
const LEAVE_MS = 200;

type Toast = { id: number; text: string; leaving: boolean };

const ToastContext = createContext<(text: string) => void>(() => undefined);

/**
 * Toasts: bottom left, worded with the action's own verb (复制 → 已复制),
 * gone after three seconds. They slide up 8px and fade in over 160ms and
 * fade out over 200ms. The stack is a polite live region with no role, so
 * it never competes with a page's own `status` or `alert`.
 */
export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const next = useRef(0);
  const show = useCallback((text: string) => {
    const id = next.current++;
    setToasts((list) => [...list, { id, text, leaving: false }]);
    window.setTimeout(() => {
      setToasts((list) => list.map((t) => (t.id === id ? { ...t, leaving: true } : t)));
      window.setTimeout(() => setToasts((list) => list.filter((t) => t.id !== id)), LEAVE_MS);
    }, SHOWN_MS);
  }, []);
  return (
    <ToastContext.Provider value={show}>
      {children}
      <div className="toasts" aria-live="polite">
        {toasts.map((t) => (
          <div key={t.id} className={t.leaving ? "toast is-leaving" : "toast"}>
            {t.text}
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

/** Shows a toast. */
export function useToast(): (text: string) => void {
  return useContext(ToastContext);
}
