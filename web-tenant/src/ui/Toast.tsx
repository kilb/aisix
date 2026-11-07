import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";

// The portal's copy of the Dashboard's Toast (P2c-R4).

const SHOWN_MS = 2400;
const LEAVE_MS = 200;

const ToastContext = createContext<(text: string) => void>(() => {});

interface Item {
  id: number;
  text: string;
  leaving: boolean;
}

/**
 * Toasts: bottom left, worded with the action's own verb (复制 → 已复制),
 * gone on their own after a moment. A toast only repeats what the page
 * already says in place, in the status text beside the control that acted;
 * so the layer is hidden from assistive technology, which would otherwise
 * hear the same words twice, and no message lives in a toast alone.
 */
export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<Item[]>([]);
  const seq = useRef(0);
  const timers = useRef<number[]>([]);

  useEffect(() => () => timers.current.forEach((t) => window.clearTimeout(t)), []);

  const show = useCallback((text: string) => {
    const id = ++seq.current;
    setItems((xs) => [...xs.slice(-2), { id, text, leaving: false }]);
    timers.current.push(
      window.setTimeout(() => setItems((xs) => xs.map((x) => (x.id === id ? { ...x, leaving: true } : x))), SHOWN_MS),
      window.setTimeout(() => setItems((xs) => xs.filter((x) => x.id !== id)), SHOWN_MS + LEAVE_MS),
    );
  }, []);

  return (
    <ToastContext.Provider value={show}>
      {children}
      <div className="toasts" aria-hidden="true">
        {items.map((t) => (
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
