import { useLayoutEffect, useRef, type KeyboardEvent } from "react";
import { cn } from "../lib/cn";

// 应用侧栏与卡内横向分组共用既有导航接口。

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
  group?: string;
}

/**
 * 文字导航保留手动激活的 tab 键盘模型：方向键、Home/End 移焦，Enter/Space 打开。
 * docked 表示侧栏或抽屉内的应用导航；其余保留分段或下划线外观。
 * tab 的 id、controls 和可访问名称沿用既有页面合同。
 */
export function RouteNav<Id extends string>({
  label,
  items,
  current,
  onSelect,
  docked,
  look = "segment",
  className,
}: {
  label: string;
  items: readonly RouteItem<Id>[];
  current: Id;
  onSelect: (id: Id) => void;
  docked?: boolean;
  look?: "segment" | "underline";
  className?: string;
}) {
  const bar = useRef<HTMLElement>(null);
  const lens = useRef<HTMLSpanElement>(null);

  // The lens covers the current tab. It is placed before the first paint
  // with no transition, and glides only from then on; it is placed again
  // whenever the bar changes size (a font arriving, the phone layout).
  useLayoutEffect(() => {
    const nav = bar.current;
    if (!nav) return;
    if (docked) {
      nav.querySelector<HTMLElement>('[aria-selected="true"]')?.scrollIntoView({ block: "nearest", inline: "nearest" });
      return;
    }
    const mark = lens.current;
    if (!mark) return;
    const place = () => {
      const tab = nav.querySelector<HTMLElement>('[aria-selected="true"]');
      if (!tab) {
        mark.style.opacity = "0";
        return;
      }
      mark.style.opacity = "";
      mark.style.width = `${tab.offsetWidth}px`;
      mark.style.height = `${tab.offsetHeight}px`;
      mark.style.transform = `translate(${tab.offsetLeft}px, ${tab.offsetTop}px)`;
    };
    place();
    const frame = requestAnimationFrame(() => mark.setAttribute("data-ready", ""));
    const resized = new ResizeObserver(place);
    resized.observe(nav);
    void document.fonts?.ready.then(place);
    return () => {
      cancelAnimationFrame(frame);
      resized.disconnect();
    };
  }, [current, items, docked]);

  function onKeyDown(e: KeyboardEvent<HTMLElement>) {
    const tabs = [...e.currentTarget.querySelectorAll<HTMLButtonElement>('[role="tab"]')];
    const at = tabs.findIndex((t) => t === document.activeElement);
    if (at < 0) return;
    const last = tabs.length - 1;
    const next =
      e.key === "ArrowRight" || (docked && e.key === "ArrowDown") ? (at === last ? 0 : at + 1)
      : e.key === "ArrowLeft" || (docked && e.key === "ArrowUp") ? (at === 0 ? last : at - 1)
      : e.key === "Home" ? 0
      : e.key === "End" ? last
      : null;
    if (next === null) return;
    e.preventDefault();
    tabs[next]?.focus();
  }

  if (docked) {
    return (
      <nav ref={bar} className={cn("shell-route-nav", className)} role="tablist" aria-label={label} aria-orientation="vertical" onKeyDown={onKeyDown}>
        <ol className="shell-route-list" role="presentation">
          {items.map((t, index) => (
            <li key={t.id} className="shell-route-item" role="presentation">
              {t.group && items[index - 1]?.group !== t.group && <div className="shell-route-group">{t.group}</div>}
              <button
                type="button"
                role="tab"
                className="shell-route-stop"
                id={`stop-${t.id}`}
                data-tab={t.id}
                aria-selected={t.id === current}
                aria-current={t.id === current ? "page" : undefined}
                aria-controls={`tab-${t.id}`}
                tabIndex={t.id === current || (index === 0 && !items.some((item) => item.id === current)) ? 0 : -1}
                onClick={() => onSelect(t.id)}
              >
                <span className="stop-name">{t.label}</span>
              </button>
            </li>
          ))}
        </ol>
      </nav>
    );
  }

  const kind = look === "underline" ? "route route-line" : "route route-seg";
  return (
    <nav
      ref={bar}
      className={cn(kind, className)}
      role="tablist"
      aria-label={label}
      onKeyDown={onKeyDown}
    >
      <span ref={lens} className="lens" aria-hidden="true" />
      {items.map((t) => (
        <button
          key={t.id}
          type="button"
          role="tab"
          id={`stop-${t.id}`}
          data-tab={t.id}
          aria-selected={t.id === current}
          aria-controls={`tab-${t.id}`}
          tabIndex={t.id === current ? 0 : -1}
          onClick={() => onSelect(t.id)}
        >
          <span className="stop" aria-hidden="true" />
          <span className="name">{t.label}</span>
        </button>
      ))}
    </nav>
  );
}
