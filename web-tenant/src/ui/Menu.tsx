import type { ReactNode } from "react";
import { DropdownMenu } from "radix-ui";
import { Ellipsis } from "lucide-react";
import { cn } from "../lib/cn";
import "./dropdown.css";

export interface MenuItem {
  label: string;
  onSelect: () => void;
  disabled?: boolean;
  danger?: boolean;
}

export interface MenuProps {
  label: string;
  items: readonly MenuItem[];
  disabled?: boolean;
  align?: "start" | "end";
  className?: string;
  children?: ReactNode;
}

/** 行操作使用独立弹层，菜单关闭后归还焦点；fieldset 禁用同样阻止打开。 */
export function Menu({ label, items, disabled, align = "end", className, children }: MenuProps) {
  return <DropdownMenu.Root modal={false}>
    <DropdownMenu.Trigger onPointerDown={event => { if (event.currentTarget.matches(":disabled")) event.preventDefault(); }} className={cn("ui-menu-trigger", className)} disabled={disabled} aria-label={label} title={label}>
      {children ?? <Ellipsis aria-hidden="true" />}
    </DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content className="ui-dropdown ui-menu-content" sideOffset={4} align={align} collisionPadding={12} loop>
        {items.map(item => <DropdownMenu.Item key={item.label} className="ui-dropdown-item" data-danger={item.danger || undefined} disabled={item.disabled} onSelect={item.onSelect} textValue={item.label}>{item.label}</DropdownMenu.Item>)}
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>;
}
