import { useLayoutEffect, useRef, type KeyboardEvent } from "react";

// The portal's copy of the Dashboard's RouteNav (P2c-R4), horizontal.

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
}

/**
 * Sections as tabs: a `tablist` of `tab`s named exactly by their labels, in
 * order. The open tab is marked by the lens, drawn outside the tabs and
 * gliding from the tab left to the one opened, so each tab's text is its
 * label alone. Tab `<id>` is `stop-<id>` and controls `tab-<id>`, the
 * `tabpanel` the caller renders for the open section, labelled by its tab.
 *
 * Keyboard: Tab reaches the current stop; the arrow keys, Home and End move
 * between stops, and Enter or Space opens one.
 *
 * Looks (ui.css): `segment`, the default, is shadcn's Tabs, a muted track
 * with the open tab a white segment (the gate's 登录 / 注册组织);
 * `underline` is Vercel's tabs, quiet text with a 2px rule under the open
 * one (a card's own tabs). `docked` is the portal's section bar: underline
 * tabs across the page under the header, and on a phone a bar along the
 * bottom edge of the screen.
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
  }, [current, items]);

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

  const kind = docked ? "route docked" : look === "underline" ? "route route-line" : "route route-seg";
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
