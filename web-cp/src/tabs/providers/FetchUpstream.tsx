import { useMemo, useState } from "react";
import { ApiError, channelModels, upstreamModels, type ChannelModel, type PublicModel } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { MAX_BULK_MODELS, appliedText, selectable, stateOf, stateText } from "../../lib/upstreamBulk";
import { Button } from "../../ui/Button";
import { DataTable } from "../../ui/DataTable";
import { why } from "../../ui/UpstreamModelPicker";

/** The most ids listed at once: a list of thousands is narrowed by searching, not by scrolling. */
const MAX_SHOWN = 500;

/**
 * 获取上游模型: the account's own model list as a searchable multi-select,
 * applied in one step.
 *
 * Each id says what mapping it would do — reuse the public model of the same
 * name, create one, or nothing because this account already maps it — read
 * from the section's own rows and public models, so it is what the control
 * plane will decide. Only an id that would add a row can be ticked. `全选`
 * ticks every selectable id the search shows. The apply is one request, all
 * or nothing: a refusal names each model it refused, and nothing is written.
 */
export function FetchUpstream({
  accountId,
  rows,
  publics,
  disabled,
  onApplied,
}: {
  accountId: string;
  /** The account's checklist: every live public model, with this account's row. */
  rows: ChannelModel[];
  /** Every public model, live and retired. */
  publics: PublicModel[];
  disabled: boolean;
  /** Reloads the section once an apply has written. */
  onApplied: () => Promise<void>;
}) {
  const [phase, setPhase] = useState<"idle" | "loading" | "listed" | "failed">("idle");
  const [ids, setIds] = useState<string[]>([]);
  const [listNote, setListNote] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [chosen, setChosen] = useState<ReadonlySet<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [applied, setApplied] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<{ message: string; lines: string[] } | null>(null);

  const liveNames = useMemo(() => new Set(publics.filter((m) => m.retired_at === null).map((m) => m.name)), [publics]);
  const retiredNames = useMemo(() => new Set(publics.filter((m) => m.retired_at !== null).map((m) => m.name)), [publics]);
  const states = useMemo(
    () => new Map(ids.map((id) => [id, stateOf(id, rows, liveNames, retiredNames)] as const)),
    [ids, rows, liveNames, retiredNames],
  );

  async function fetchList() {
    setPhase("loading");
    setFailure(null);
    setApplied(null);
    setRefusal(null);
    try {
      const answer = await upstreamModels.list(accountId);
      setIds(answer.items.map((m) => m.id));
      setListNote(answer.warnings.length > 0 ? answer.warnings.join("；") : null);
      setChosen(new Set());
      setPhase("listed");
    } catch (e) {
      setFailure(why(e));
      setPhase("failed");
    }
  }

  const filter = query.trim().toLowerCase();
  const matches = filter === "" ? ids : ids.filter((id) => id.toLowerCase().includes(filter));
  const shown = matches.slice(0, MAX_SHOWN);
  const shownSelectable = shown.filter((id) => selectable(states.get(id)!));
  const allShownChosen = shownSelectable.length > 0 && shownSelectable.every((id) => chosen.has(id));
  // A tick survives a refresh only while its id is still selectable.
  const picked = ids.filter((id) => chosen.has(id) && selectable(states.get(id)!));
  const overCap = picked.length > MAX_BULK_MODELS;

  function toggle(id: string, on: boolean) {
    setChosen((prev) => {
      const next = new Set(prev);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  }

  function toggleShown(on: boolean) {
    setChosen((prev) => {
      const next = new Set(prev);
      for (const id of shownSelectable) {
        if (on) next.add(id);
        else next.delete(id);
      }
      return next;
    });
  }

  async function apply() {
    // The selection is fixed here: a tick during the request must not change what it wrote.
    const batch = picked;
    setBusy(true);
    setApplied(null);
    setRefusal(null);
    try {
      const items = await channelModels.bulk(accountId, batch);
      setApplied(appliedText(items));
      setChosen(new Set());
      await onApplied();
    } catch (e) {
      if (e instanceof ApiError) {
        setRefusal({ message: messageFor(e), lines: e.details.map((d) => d.message) });
      } else {
        setRefusal({ message: errorText(e), lines: [] });
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    // A group, not a region: the section around it is the region named 模型.
    <div className="ch-fetch" role="group" aria-label="获取上游模型">
      <p className="hint">
        从该账号的上游读取模型列表，勾选后一次写入映射：已有同名公共模型的直接复用，没有的以上游模型名称新建；该账号已映射的模型保持不变。
      </p>
      <p>
        <Button
          type="button"
          icon="refresh"
          busy={phase === "loading"}
          busyLabel="获取中…"
          disabled={disabled || busy}
          onClick={() => void fetchList()}
        >
          获取上游模型
        </Button>
      </p>
      {phase === "failed" && failure !== null && (
        <div className="note crit" role="alert">
          无法获取该上游的模型列表：{failure}
        </div>
      )}
      {applied && (
        <div className="note ok" role="status">
          {applied}
        </div>
      )}
      {refusal && (
        <div className="note crit" role="alert">
          {refusal.message}
          {refusal.lines.length > 0 && (
            <ul>
              {refusal.lines.map((line, i) => (
                <li key={i}>{line}</li>
              ))}
            </ul>
          )}
        </div>
      )}
      {phase === "listed" && (
        <>
          {listNote && <div className="note warn">{listNote}</div>}
          {ids.length === 0 ? (
            <p className="hint">该上游没有返回任何模型名称。</p>
          ) : (
            <>
              <div className="ch-fetch-bar">
                <label className="f ch-filter">
                  <span className="f-label">搜索上游模型</span>
                  <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="按上游模型名称筛选" />
                </label>
                <label className="ch-tick">
                  <input
                    type="checkbox"
                    checked={allShownChosen}
                    disabled={disabled || busy || shownSelectable.length === 0}
                    onChange={(e) => toggleShown(e.target.checked)}
                  />
                  全选筛选结果（{shownSelectable.length} 个可选）
                </label>
              </div>
              <DataTable label="上游模型列表" scroll>
                <thead>
                  <tr>
                    <th>选择</th>
                    <th>上游模型</th>
                    <th>映射</th>
                  </tr>
                </thead>
                <tbody>
                  {shown.map((id) => {
                    const state = states.get(id)!;
                    return (
                      <tr key={id}>
                        <td>
                          <input
                            type="checkbox"
                            aria-label={`选择 ${id}`}
                            checked={chosen.has(id) && selectable(state)}
                            disabled={disabled || busy || !selectable(state)}
                            onChange={(e) => toggle(id, e.target.checked)}
                          />
                        </td>
                        <td>
                          <span className="mono">{id}</span>
                        </td>
                        <td className={selectable(state) ? undefined : "ch-fetch-muted"}>{stateText(state)}</td>
                      </tr>
                    );
                  })}
                  {shown.length === 0 && (
                    <tr>
                      <td colSpan={3}>没有匹配的上游模型。</td>
                    </tr>
                  )}
                </tbody>
              </DataTable>
              {matches.length > shown.length && (
                <p className="hint">
                  共 {matches.length} 个，只列出前 {MAX_SHOWN} 个；继续输入可缩小范围。
                </p>
              )}
              {overCap && (
                <div className="note warn" role="alert">
                  一次最多映射 {MAX_BULK_MODELS} 个上游模型，已选 {picked.length} 个，请减少选择后再写入。
                </div>
              )}
              <p className="ch-save">
                <Button
                  type="button"
                  busy={busy}
                  busyLabel="写入中…"
                  disabled={disabled || picked.length === 0 || overCap}
                  disabledReason={
                    picked.length === 0 ? "先勾选要映射的上游模型" : overCap ? `一次最多 ${MAX_BULK_MODELS} 个` : undefined
                  }
                  onClick={() => void apply()}
                >
                  映射所选模型
                </Button>
                <span className="hint">已选 {picked.length} 个</span>
              </p>
            </>
          )}
        </>
      )}
    </div>
  );
}
