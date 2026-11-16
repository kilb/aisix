import type { ReactNode, RefObject } from "react";
import { Dialog } from "radix-ui";
import { X } from "lucide-react";
import type { FormulaCall } from "./lib/formulaApi";
import { fmtBeijing } from "./lib/time";
import { Button } from "./ui/Button";
import "./CallLogDrawer.css";

/** 详情使用独立滚动区，复用 Radix 的焦点约束、Escape 与遮罩关闭。 */
export function CallLogDrawer({ call, onOpenChange, returnFocusRef, children }: {
  call: FormulaCall | null;
  onOpenChange: (open: boolean) => void;
  returnFocusRef: RefObject<HTMLButtonElement | null>;
  children: ReactNode;
}) {
  return <Dialog.Root open={call !== null} onOpenChange={onOpenChange}>
    <Dialog.Portal>
      <Dialog.Overlay className="tenant-call-overlay" />
      <Dialog.Content className="tenant-call-drawer" onCloseAutoFocus={event => {
        event.preventDefault();
        if (returnFocusRef.current?.isConnected) returnFocusRef.current.focus();
      }}>
        <header className="tenant-call-drawer-header">
          <div>
            <Dialog.Title className="tenant-call-drawer-title">调用详情</Dialog.Title>
            <Dialog.Description className="tenant-call-drawer-description">{call ? `${call.model.name || "未知模型"} · ${fmtBeijing(call.occurred_at)}` : "调用记录的性能、用量与费用详情"}</Dialog.Description>
          </div>
          <Dialog.Close asChild><Button kind="quiet" iconOnly aria-label="关闭调用详情" title="关闭调用详情"><X aria-hidden /></Button></Dialog.Close>
        </header>
        <div className="tenant-call-drawer-body">{children}</div>
      </Dialog.Content>
    </Dialog.Portal>
  </Dialog.Root>;
}
