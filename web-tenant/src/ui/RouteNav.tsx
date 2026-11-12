import { useLayoutEffect, useRef, type KeyboardEvent } from "react";

// The portal's copy of the Dashboard's RouteNav (P2c-R4), horizontal.

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
}

/**
 * 文字导航保留手动激活的 tab 键盘模型：方向键、Home/End 移焦，Enter/Space 打开。
 * docked 表示应用顶导航，窄屏仍在顶部独立横滚；其余用分段或下划线外观。
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
    const mark = lens.current;
    if (!nav || !mark) return;
    const place = () => {
      const tab = nav.querySelector<HTMLElement>('[aria-selected="true"]');
      if (!tab) {
        mark.style.opacity = "0";
        return;
      }
      if (docked) {
        // 只滚动导航本身，深链或键盘激活末项不会移动整张页面。
        const left = tab.offsetLeft;
        const right = left + tab.offsetWidth;
        if (left < nav.scrollLeft) nav.scrollLeft = left;
        else if (right > nav.scrollLeft + nav.clientWidth) nav.scrollLeft = right - nav.clientWidth;
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
      e.key === "ArrowRight" ? (at === last ? 0 : at + 1)
      : e.key === "ArrowLeft" ? (at === 0 ? last : at - 1)
      : e.key === "Home" ? 0
      : e.key === "End" ? last
      : null;
    if (next === null) return;
    e.preventDefault();
    tabs[next]?.focus();
  }

  const kind = docked ? "route route-topnav" : look === "underline" ? "route route-line" : "route route-seg";
  return (
    <nav
      ref={bar}
      className={className ? `${kind} ${className}` : kind}
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
          aria-current={docked && t.id === current ? "page" : undefined}
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
