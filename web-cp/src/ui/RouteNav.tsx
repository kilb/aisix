import { useEffect, useRef, type ComponentType, type KeyboardEvent } from "react";
import { cn } from "../lib/cn";

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
  /** A lucide icon drawn before the label. */
  icon?: ComponentType<{ className?: string; "aria-hidden"?: boolean }>;
}

/**
 * The section navigation: the sidebar's list of sections from 1024px, and
 * under it a bar along the bottom of the screen that scrolls sideways. The
 * current section is the filled row.
 *
 * It is a `tablist` of `tab`s whose text is the section name and nothing
 * else. A section that needs attention carries a small red marker, drawn by
 * CSS, and says why through `aria-describedby` to a visually hidden note
 * outside the `tablist`. Arrow keys move along the list, Home and End to its
 * ends, and Enter or Space opens a section.
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

  // As a bottom bar the list scrolls sideways: keep the current section in view.
  useEffect(() => {
    if (!window.matchMedia("(max-width: 1023px)").matches) return;
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
      <ol
        ref={list}
        role="tablist"
        aria-label={label}
        onKeyDown={onKeyDown}
        className={cn(
          "flex flex-col gap-0.5",
          "max-lg:fixed max-lg:inset-x-0 max-lg:bottom-0 max-lg:z-40 max-lg:flex-row max-lg:gap-0 max-lg:overflow-x-auto",
          "max-lg:border-t max-lg:border-border max-lg:bg-card/95 max-lg:px-2 max-lg:pt-1.5 max-lg:pb-[calc(6px+env(safe-area-inset-bottom))] max-lg:backdrop-blur",
          "max-lg:[scrollbar-width:none]",
        )}
      >
        {items.map((t) => {
          const note = attention[t.id];
          const selected = current === t.id;
          const Glyph = t.icon;
          return (
            <li key={t.id} role="presentation" className="max-lg:flex-none">
              <button
                type="button"
                role="tab"
                className={cn(
                  "route-stop group relative flex w-full items-center gap-3 rounded-md px-3 text-left text-sm transition-colors",
                  "h-9 text-text-2 hover:bg-sidebar-accent/70 hover:text-foreground",
                  "aria-selected:bg-sidebar-accent aria-selected:font-medium aria-selected:text-foreground",
                  "max-lg:h-auto max-lg:min-w-16 max-lg:flex-col max-lg:gap-1 max-lg:px-2 max-lg:py-1.5 max-lg:text-[11px] max-lg:text-muted-foreground max-lg:aria-selected:bg-sidebar-accent max-lg:aria-selected:text-foreground",
                )}
                id={`stop-${t.id}`}
                data-tab={t.id}
                data-attention={note ? "" : undefined}
                aria-selected={selected}
                aria-controls={`tab-${t.id}`}
                aria-describedby={note ? `attention-${t.id}` : undefined}
                tabIndex={selected ? 0 : -1}
                onClick={() => onSelect(t.id)}
              >
                {Glyph && (
                  <Glyph
                    aria-hidden
                    className={cn(
                      "size-4 shrink-0 text-muted-foreground group-hover:text-foreground",
                      selected && "text-foreground",
                      "max-lg:size-5",
                    )}
                  />
                )}
                <span className="stop-name min-w-0 truncate">{t.label}</span>
                {note && (
                  <span
                    aria-hidden="true"
                    className="ml-auto size-2 shrink-0 rounded-full bg-danger max-lg:absolute max-lg:top-1 max-lg:right-3 max-lg:ml-0"
                  />
                )}
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
