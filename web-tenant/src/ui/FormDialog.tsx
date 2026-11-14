import { useRef, type ReactNode, type RefObject } from "react";
import { Dialog } from "radix-ui";
import { X } from "lucide-react";
import { Button } from "./Button";
import "./form-dialog.css";

/** 创建操作保持独立焦点与滚动边界；失败保留输入，请求未决时禁止误关。 */
export function FormDialog({ open, onOpenChange, title, description, busy = false, returnFocusRef, children }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: string;
  busy?: boolean;
  returnFocusRef?: RefObject<HTMLElement | null>;
  children: ReactNode;
}) {
  const content = useRef<HTMLDivElement>(null);
  const returnFocus = useRef<HTMLElement | null>(null);
  const pending = () => busy || content.current?.querySelector(".btn.is-busy, [aria-busy=\"true\"]") != null;
  return <Dialog.Root open={open} onOpenChange={(next) => { if (!pending()) onOpenChange(next); }}>
    <Dialog.Portal>
      <Dialog.Overlay className="form-dialog-overlay" />
      <Dialog.Content ref={content} className="form-dialog" aria-busy={busy}
        onOpenAutoFocus={(event) => {
          returnFocus.current = returnFocusRef?.current ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null);
          event.preventDefault();
          const first = content.current?.querySelector<HTMLElement>('input:not([type="hidden"]):not(:disabled), select:not(:disabled), textarea:not(:disabled), [data-dialog-focus]');
          (first ?? content.current)?.focus();
        }}
        onCloseAutoFocus={(event) => { event.preventDefault(); returnFocus.current?.focus(); }}
        onEscapeKeyDown={(event) => { if (pending() || document.activeElement?.closest(".two-step.is-asking")) event.preventDefault(); }}
        onPointerDownOutside={(event) => event.preventDefault()}>
        <div className="form-dialog-heading">
          <div className="min-w-0">
            <Dialog.Title className="form-dialog-title">{title}</Dialog.Title>
            <Dialog.Description className={description ? "form-dialog-description" : "vh"}>{description ?? title}</Dialog.Description>
          </div>
          <Dialog.Close asChild><Button kind="quiet" disabled={busy} className="form-dialog-close" aria-label={`关闭${title}`}><X aria-hidden /></Button></Dialog.Close>
        </div>
        <div className="form-dialog-body">{children}</div>
      </Dialog.Content>
    </Dialog.Portal>
  </Dialog.Root>;
}
