import { useEffect, useRef, type ReactNode } from "react";
import { Button } from "./Button";

/**
 * A detail that opens beside its list, keeping the list visible: a
 * `dialog` (not modal) that slides in from the right over 160ms. It sits
 * beside the list from 1400px and stacks under it below that; the list's
 * container is `.with-side` with `is-open` while it shows. Escape or the
 * close button closes it. It takes focus when it opens, which also brings
 * it into view where it stacks, and gives it back to what opened it when
 * it closes.
 */
export function SidePanel({
  title,
  label,
  extra,
  onClose,
  children,
}: {
  title: ReactNode;
  /** The dialog's accessible name. */
  label: string;
  /** Beside the title: a lamp, a tag. */
  extra?: ReactNode;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLElement>(null);
  useEffect(() => {
    const panel = ref.current;
    if (!panel) return;
    // What opened the panel gets the focus back when it closes.
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    panel.focus({ preventScroll: true });
    // Stacked under the list, it may open below the fold: bring its top into view.
    if (panel.getBoundingClientRect().top > window.innerHeight) panel.scrollIntoView({ block: "start" });
    return () => {
      // Only when the focus went with the panel (关闭, Escape): opening
      // another tenant's detail has put it on that tenant's name already.
      const lost = document.activeElement === null || document.activeElement === document.body;
      if (lost && opener?.isConnected) opener.focus({ preventScroll: true });
    };
  }, []);
  return (
    <section
      ref={ref}
      className="panel side-panel"
      role="dialog"
      aria-label={label}
      tabIndex={-1}
      onKeyDown={(e) => {
        // Not from a field: Escape there must not throw away what is typed.
        const t = e.target;
        const typing = t instanceof HTMLInputElement || t instanceof HTMLSelectElement || t instanceof HTMLTextAreaElement;
        if (e.key === "Escape" && !typing && !e.defaultPrevented) onClose();
      }}
    >
      <div className="side-head">
        <h2 title={typeof title === "string" ? title : undefined}>{title}</h2>
        {extra}
        <Button type="button" kind="quiet" size="sm" icon="close" className="side-close" aria-label="关闭" title="关闭" onClick={onClose} />
      </div>
      {children}
    </section>
  );
}
