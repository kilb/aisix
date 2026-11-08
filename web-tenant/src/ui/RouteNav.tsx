import type { CSSProperties, KeyboardEvent } from "react";

// The portal's copy of the Dashboard's RouteNav (P2c-R4), horizontal.

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
}

/**
 * Sections as stops on a route line: a `tablist` of `tab`s named exactly
 * by their labels, in order, joined by one Ink line. The current tab is a
 * larger Sign-yellow stop. A stop is drawn by CSS and carries no text, so
 * each tab's text is its label alone. Tab `<id>` is `stop-<id>` and
 * controls `tab-<id>`, the `tabpanel` the caller renders for the open
 * section, labelled by its tab.
 *
 * Keyboard: Tab reaches the current stop; the arrow keys, Home and End move
 * between stops, and Enter or Space opens one.
 *
 * `docked` is the portal's section bar: across the top, and on a phone a
 * bar along the bottom edge of the screen. Without it the line stays where
 * it is placed (the gate's 登录 / 注册组织).
 */
export function RouteNav<Id extends string>({
  label,
  items,
  current,
  onSelect,
  docked,
}: {
  label: string;
  items: readonly RouteItem<Id>[];
  current: Id;
  onSelect: (id: Id) => void;
  docked?: boolean;
}) {
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

  return (
    <nav
      className={docked ? "route docked" : "route"}
      role="tablist"
      aria-label={label}
      style={{ "--stops": items.length } as CSSProperties}
      onKeyDown={onKeyDown}
    >
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
