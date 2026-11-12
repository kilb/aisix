import { useEffect, useRef, type ComponentType, type KeyboardEvent } from "react";

export interface RouteItem<Id extends string> {
  id: Id;
  label: string;
  group?: string;
  /** A lucide icon drawn before the label. */
  icon?: ComponentType<{ className?: string; "aria-hidden"?: boolean }>;
}

/** 侧栏分区导航，保留键盘漫游与需要关注事项的文字说明。 */
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
  const tabs = () => [...(list.current?.querySelectorAll<HTMLAnchorElement>("a[href]") ?? [])];

  // 当前入口始终在侧栏自己的滚动区域中可见。
  useEffect(() => {
    list.current
      ?.querySelector<HTMLElement>('[aria-current="page"]')
      ?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [current]);

  function onKeyDown(e: KeyboardEvent<HTMLOListElement>) {
    const all = tabs();
    const at = all.indexOf(document.activeElement as HTMLAnchorElement);
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
    <nav className="shell-route-nav" aria-label={label}>
      <ol
        ref={list}
        aria-label={label}
        onKeyDown={onKeyDown}
        className="shell-route-list"
      >
        {items.map((t, index) => {
          const note = attention[t.id];
          const selected = current === t.id;
          const Icon = t.icon;
          return (
            <li key={t.id} role="presentation" className="shell-route-item">
              {t.group && items[index - 1]?.group !== t.group && <div className="shell-route-group">{t.group}</div>}
              <a
                href={`#${t.id}`}
                className="shell-route-stop"
                id={`stop-${t.id}`}
                data-tab={t.id}
                data-attention={note ? "" : undefined}
                aria-label={t.label}
                aria-current={selected ? "page" : undefined}
                aria-describedby={note ? `attention-${t.id}` : undefined}
                onClick={(event) => {
                  if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
                  event.preventDefault();
                  onSelect(t.id);
                }}
              >
                {Icon && <Icon className="shell-route-icon" aria-hidden />}
                <span className="stop-name">{t.label}</span>
                {note && <span aria-hidden="true" className="cp-attention">!</span>}
              </a>
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
