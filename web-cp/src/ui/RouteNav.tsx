import { useEffect, useRef, type ComponentType, type KeyboardEvent } from "react";

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
  /** A lucide icon drawn before the label. */
  icon?: ComponentType<{ className?: string; "aria-hidden"?: boolean }>;
}

/** 顶部文字分区导航，保留键盘漫游与需要关注事项的文字说明。 */
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

  // 当前入口始终在导航自己的横向滚动区域中可见。
  useEffect(() => {
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
    <nav className="cp-route-nav">
      <ol
        ref={list}
        role="tablist"
        aria-label={label}
        aria-orientation="horizontal"
        onKeyDown={onKeyDown}
        className="cp-route-list"
      >
        {items.map((t) => {
          const note = attention[t.id];
          const selected = current === t.id;
          return (
            <li key={t.id} role="presentation">
              <button
                type="button"
                role="tab"
                className="route-stop cp-route-stop"
                id={`stop-${t.id}`}
                data-tab={t.id}
                data-attention={note ? "" : undefined}
                aria-selected={selected}
                aria-current={selected ? "page" : undefined}
                aria-controls={`tab-${t.id}`}
                aria-describedby={note ? `attention-${t.id}` : undefined}
                tabIndex={selected ? 0 : -1}
                onClick={() => onSelect(t.id)}
              >
                <span className="stop-name">{t.label}</span>
                {note && <span aria-hidden="true" className="cp-attention">!</span>}
              </button>
            </li>
          );
        })}
      </ol>
      {items.map((t) => attention[t.id] ? (
        <span key={t.id} id={`attention-${t.id}`} className="vh">{attention[t.id]}</span>
      ) : null)}
    </nav>
  );
}
