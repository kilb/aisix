import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Dialog } from "radix-ui";
import { Menu, PanelLeftClose, PanelLeftOpen, X } from "lucide-react";
import { Button } from "./Button";
import "./Sidebar.css";

const DESKTOP_QUERY = "(min-width: 1024px)";

/** 桌面与抽屉共用同一份导航，避免重复的导航 id 与可访问入口。 */
export function SidebarShell({
  brand,
  label,
  context,
  footer,
  navigation,
  routeKey,
  storageKey = `aisix.sidebar.collapsed:${window.location.pathname}`,
  children,
}: {
  brand: ReactNode | ((close: () => void) => ReactNode);
  label: string;
  context?: ReactNode;
  footer?: ReactNode | ((close: () => void) => ReactNode);
  navigation: (close: () => void) => ReactNode;
  routeKey?: string;
  storageKey?: string;
  children: ReactNode;
}) {
  const [desktop, setDesktop] = useState(() => window.matchMedia(DESKTOP_QUERY).matches);
  const [open, setOpen] = useState(false);
  const [collapsed, setCollapsed] = useState(() => {
    try {
      return window.localStorage.getItem(storageKey) === "1";
    } catch {
      return false;
    }
  });
  const sidebar = useRef<HTMLElement>(null);
  const shell = useRef<HTMLDivElement>(null);
  const returnToMenu = useRef(false);
  const close = useCallback(() => setOpen(false), []);
  const toggleCollapsed = () => setCollapsed((current) => {
    try {
      window.localStorage.setItem(storageKey, current ? "0" : "1");
    } catch {
      // 存储不可用时仍可在当前页面收起导航。
    }
    return !current;
  });

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
    const current = sidebar.current?.querySelector<HTMLElement>('[aria-current="page"]');
    current?.scrollIntoView({ block: "nearest", inline: "nearest" });
    if (!open) return;
    close();
    current?.focus({ preventScroll: true });
  }, [desktop, open, close]);

  const panel = (
    <>
      <div className="shell-sidebar-head">
        <div className="shell-sidebar-brand">{typeof brand === "function" ? brand(close) : brand}</div>
        {desktop && <Button kind="quiet" size="sm" className="shell-collapse-toggle"
          aria-label={collapsed ? "展开侧边栏" : "收起侧边栏"} aria-expanded={!collapsed}
          onClick={toggleCollapsed}>
          {collapsed ? <PanelLeftOpen aria-hidden="true" /> : <PanelLeftClose aria-hidden="true" />}
        </Button>}
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
      <div ref={shell} className="sidebar-shell" data-collapsed={desktop && collapsed ? "true" : undefined}>
        <a className="shell-skip-link" href="#workspace-main" onClick={(event) => {
          event.preventDefault();
          const main = shell.current?.querySelector<HTMLElement>("#workspace-main");
          main?.focus({ preventScroll: true });
          main?.scrollIntoView({ block: "start" });
        }}>跳到主要内容</a>
        {desktop || !open ? (
          <aside ref={sidebar} className="shell-sidebar" aria-label={label} hidden={!desktop}>{panel}</aside>
        ) : (
          <Dialog.Portal>
            <Dialog.Overlay className="shell-drawer-backdrop" />
            <Dialog.Content className="shell-sidebar shell-drawer" aria-modal="true" aria-describedby={undefined}>
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
