import { useEffect, useLayoutEffect, useRef, type KeyboardEvent } from "react";

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
}

/**
 * The section navigation: a list of sections on the glass rail down the
 * left edge, and under 960px a floating bar along the bottom that scrolls
 * sideways. The current section sits in the lens, a Signal-yellow capsule
 * that glides from the section left to the one opened.
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
  const lens = useRef<HTMLLIElement>(null);
  const tabs = () => [...(list.current?.querySelectorAll<HTMLButtonElement>('[role="tab"]') ?? [])];

  // The lens covers the current section's tab. It is placed before the first
  // paint with no transition, and glides only from then on; it is placed
  // again whenever the list changes size (a font arriving, the rail turning
  // into the bottom bar).
  useLayoutEffect(() => {
    const ol = list.current;
    const mark = lens.current;
    if (!ol || !mark) return;
    const place = () => {
      const tab = ol.querySelector<HTMLElement>('[aria-selected="true"]');
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
    resized.observe(ol);
    void document.fonts?.ready.then(place);
    return () => {
      cancelAnimationFrame(frame);
      resized.disconnect();
    };
  }, [current, items]);

  // As a bottom bar the list scrolls sideways: keep the current section in view.
  useEffect(() => {
    if (!window.matchMedia("(max-width: 959px)").matches) return;
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
    // once. No aria-orientation: the list is vertical on a wide screen and a
    // horizontal bar on a narrow one, and both arrow axes move along it.
    <nav className="route-nav">
      <ol ref={list} role="tablist" aria-label={label} onKeyDown={onKeyDown}>
        <li ref={lens} className="lens" role="presentation" aria-hidden="true" />
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
