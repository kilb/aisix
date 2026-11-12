import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Dialog } from "radix-ui";
import { Menu, X } from "lucide-react";
import { Button } from "./Button";
import "./Sidebar.css";

const DESKTOP_QUERY = "(min-width: 1100px)";

/** 桌面与抽屉共用同一份导航，避免重复的 tab id 与可访问入口。 */
export function SidebarShell({
  brand,
  label,
  context,
  footer,
  navigation,
  routeKey,
  children,
}: {
  brand: ReactNode | ((close: () => void) => ReactNode);
  label: string;
  context?: ReactNode;
  footer?: ReactNode | ((close: () => void) => ReactNode);
  navigation: (close: () => void) => ReactNode;
  routeKey?: string;
  children: ReactNode;
}) {
  const [desktop, setDesktop] = useState(() => window.matchMedia(DESKTOP_QUERY).matches);
  const [open, setOpen] = useState(false);
  const sidebar = useRef<HTMLElement>(null);
  const shell = useRef<HTMLDivElement>(null);
  const returnToMenu = useRef(false);
  const close = useCallback(() => setOpen(false), []);

  useEffect(() => {
    const query = window.matchMedia(DESKTOP_QUERY);
    const change = () => {
      if (!query.matches && sidebar.current?.contains(document.activeElement)) returnToMenu.current = true;
      setDesktop(query.matches);
    };
    change();
    query.addEventListener("change", change);
    return () => query.removeEventListener("change", change);
  }, []);

  // 外部深链与页内跳转同样收起抽屉，不改变页面自己的导航状态。
  useEffect(close, [routeKey, close]);

  useEffect(() => {
    if (!desktop && returnToMenu.current) {
      returnToMenu.current = false;
      shell.current?.querySelector<HTMLElement>(".shell-menu-trigger")?.focus({ preventScroll: true });
    }
    if (!desktop) return;
    const current = sidebar.current?.querySelector<HTMLElement>('[aria-selected="true"]');
    current?.scrollIntoView({ block: "nearest", inline: "nearest" });
    if (!open) return;
    close();
    current?.focus({ preventScroll: true });
  }, [desktop, open, close]);

  const panel = (
    <>
      <div className="shell-sidebar-head">
        <div className="shell-sidebar-brand">{typeof brand === "function" ? brand(close) : brand}</div>
        {!desktop && (
          <Dialog.Close asChild>
            <Button kind="quiet" className="shell-drawer-close" aria-label="关闭菜单">
              <X aria-hidden="true" />
            </Button>
          </Dialog.Close>
        )}
      </div>
      {context && <div className="shell-sidebar-context">{context}</div>}
      <div className="shell-sidebar-navigation">{navigation(close)}</div>
      {footer && <div className="shell-sidebar-footer">{typeof footer === "function" ? footer(close) : footer}</div>}
    </>
  );

  return (
    <Dialog.Root open={!desktop && open} onOpenChange={setOpen}>
      <div ref={shell} className="sidebar-shell">
        {desktop || !open ? (
          <aside ref={sidebar} className="shell-sidebar" aria-label={label} hidden={!desktop}>{panel}</aside>
        ) : (
          <Dialog.Portal>
            <Dialog.Overlay className="shell-drawer-backdrop" />
            <Dialog.Content className="shell-sidebar shell-drawer" aria-describedby={undefined}>
              <Dialog.Title className="vh">{label}</Dialog.Title>
              {panel}
            </Dialog.Content>
          </Dialog.Portal>
        )}
        <div className="shell-main">{children}</div>
      </div>
    </Dialog.Root>
  );
}

/** 由 Radix 记录触发按钮，关闭、Escape 和选择入口后均可返回焦点。 */
export function SidebarMenuButton() {
  return (
    <Dialog.Trigger asChild>
      <Button className="shell-menu-trigger" aria-label="打开菜单">
        <Menu aria-hidden="true" />
        菜单
      </Button>
    </Dialog.Trigger>
  );
}
