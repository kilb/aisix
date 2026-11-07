import { useEffect, useRef, type KeyboardEvent } from "react";

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
}

/**
 * The section navigation as a route line, one stop per section: vertical
 * down the left edge, with names beside the stops from 1200px and on hover
 * or focus below that; a bar along the bottom under 720px. The current
 * stop is the larger Sign-yellow one.
 *
 * It is a `tablist` of `tab`s whose text is the section name and nothing
 * else. A section that needs attention carries a small Stop-red marker,
 * drawn by CSS, and says why through `aria-describedby` to a visually
 * hidden note outside the `tablist`. Arrow keys move along the line, Home
 * and End to its ends, and Enter or Space opens a stop.
 */
export function RouteNav<Id extends string>({
  items,
  current,
  attention,
  label,
  onSelect,
}: {
  items: readonly RouteItem<Id>[];
  current: Id;
  /** Per section, what needs attention there, in words; absent when nothing does. */
  attention: Partial<Record<Id, string>>;
  label: string;
  onSelect: (id: Id) => void;
}) {
  const list = useRef<HTMLOListElement>(null);
  const tabs = () => [...(list.current?.querySelectorAll<HTMLButtonElement>('[role="tab"]') ?? [])];

  // On a phone the line is a bar that scrolls sideways: keep the current stop in view.
  useEffect(() => {
    if (!window.matchMedia("(max-width: 719px)").matches) return;
    list.current
      ?.querySelector<HTMLElement>('[aria-selected="true"]')
      ?.scrollIntoView({ block: "nearest", inline: "center" });
  }, [current]);

  function onKeyDown(e: KeyboardEvent<HTMLOListElement>) {
    const all = tabs();
    const at = all.indexOf(document.activeElement as HTMLButtonElement);
    if (at < 0) return;
    const to =
      e.key === "ArrowDown" || e.key === "ArrowRight"
        ? (at + 1) % all.length
        : e.key === "ArrowUp" || e.key === "ArrowLeft"
          ? (at - 1 + all.length) % all.length
          : e.key === "Home"
            ? 0
            : e.key === "End"
              ? all.length - 1
              : null;
    if (to === null) return;
    e.preventDefault();
    all[to]?.focus();
  }

  return (
    // The list carries the name, not the landmark too, so it is announced
    // once. No aria-orientation: the line is vertical on a wide screen and a
    // horizontal bar on a phone, and both arrow axes move along it.
    <nav className="route-nav">
      <ol ref={list} role="tablist" aria-label={label} onKeyDown={onKeyDown}>
        {items.map((t) => {
          const note = attention[t.id];
          return (
            <li key={t.id} role="presentation">
              <button
                type="button"
                role="tab"
                className="route-stop"
                id={`stop-${t.id}`}
                data-tab={t.id}
                data-attention={note ? "" : undefined}
                aria-selected={current === t.id}
                aria-controls={`tab-${t.id}`}
                aria-describedby={note ? `attention-${t.id}` : undefined}
                tabIndex={current === t.id ? 0 : -1}
                onClick={() => onSelect(t.id)}
              >
                <span className="stop-box" aria-hidden="true">
                  <span className="stop" />
                </span>
                <span className="stop-name">{t.label}</span>
              </button>
            </li>
          );
        })}
      </ol>
      {items.map((t) =>
        attention[t.id] ? (
          <span key={t.id} id={`attention-${t.id}`} className="vh">
            {attention[t.id]}
          </span>
        ) : null,
      )}
    </nav>
  );
}
