import { UserRoundPlus, X } from "lucide-react";
import { Dialog } from "radix-ui";
import { useRef, useState } from "react";
import { CreateMember } from "./CreateMember";
import type { Member, MemberRole } from "./lib/api";
import { Button } from "./ui/Button";
import "./CreateMember.css";

/** 创建表单独立挂载；请求未决时保留输入、阻止重复提交与误关。 */
export function CreateMemberDialog({ roles, labels, onInactive, onCreated }: {
  roles: MemberRole[];
  labels: Record<MemberRole, string>;
  onInactive: () => void;
  onCreated: (member: Member) => void;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const content = useRef<HTMLDivElement>(null);

  function changeOpen(next: boolean) {
    if (!next && pending.current) return;
    setOpen(next);
  }

  return (
    <Dialog.Root open={open} onOpenChange={changeOpen}>
      <Dialog.Trigger asChild>
        <Button ref={trigger} kind="primary"><UserRoundPlus aria-hidden />创建账号</Button>
      </Dialog.Trigger>
      <Dialog.Portal>
        <Dialog.Overlay className="create-member-overlay" />
        <Dialog.Content
          ref={content}
          className="create-member-dialog"
          onOpenAutoFocus={(event) => {
            event.preventDefault();
            content.current?.querySelector<HTMLInputElement>('input[type="email"]')?.focus();
          }}
          onCloseAutoFocus={(event) => {
            event.preventDefault();
            trigger.current?.focus();
          }}
          onEscapeKeyDown={(event) => { if (pending.current) event.preventDefault(); }}
          onPointerDownOutside={(event) => event.preventDefault()}
        >
          <div className="create-member-dialog-heading">
            <div className="min-w-0">
              <p className="mb-3 font-mono text-meta text-muted">组织成员</p>
              <Dialog.Title className="text-title font-medium">创建账号</Dialog.Title>
            </div>
            <Button type="button" kind="quiet" className="size-11 p-0" disabled={busy} aria-label="关闭创建账号" onClick={() => changeOpen(false)}>
              <X aria-hidden />
            </Button>
          </div>
          <Dialog.Description className="mt-4 text-body text-ink-soft">
            直接为新成员设置登录账号。新成员可使用邮箱和初始密码登录本组织；已有账号的密码不会被修改。
          </Dialog.Description>
          <CreateMember
            roles={roles}
            labels={labels}
            onInactive={onInactive}
            onBusyChange={(next) => { pending.current = next; setBusy(next); }}
            onCancel={() => changeOpen(false)}
            onCreated={(member) => {
              onCreated(member);
              setOpen(false);
            }}
          />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
