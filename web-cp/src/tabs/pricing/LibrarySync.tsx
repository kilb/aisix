import { useCallback, useEffect, useRef, useState } from "react";
import {
  ApiError,
  library,
  type BaselineRevision,
  type LibraryEntry,
  type SyncCandidate,
  type SyncItem,
  type SyncState,
  type SyncPreview,
} from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { flagText } from "../../lib/importReview";
import {
  byState,
  cardLines,
  longContextChange,
  longContextLines,
  longContextText,
  syncRequest,
  syncStateText,
} from "../../lib/library";
import { safeHttpUrl } from "../../lib/safeUrl";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useToast } from "../../ui/Toast";
import "./PriceLibrary.css";

/** The attestation's own words: the checkbox's text, as the import's confirmation gives it. */
const ATTESTATION = "已对照官方价格页核对";

/** The chosen candidate's value in a row's select: its entry's `provider/model`. */
function keyOf(entry: { provider: string; model: string }): string {
  return `${entry.provider}/${entry.model}`;
}

/** An error as the page says it: the code's text, and each item the answer names. */
function failureText(e: unknown): string {
  if (!(e instanceof ApiError)) return errorText(e);
  const details = e.details.map((d) => d.message).filter(Boolean);
  return details.length > 0 ? `${messageFor(e)}：${details.join("；")}` : messageFor(e);
}

/** The lines of a card, one per tier and band, or a dash. */
function CardLines({ card }: { card: unknown }) {
  const lines = cardLines((card ?? null) as Parameters<typeof cardLines>[0]);
  if (lines.length === 0) return <>—</>;
  return (
    <span className="library-card">
      {lines.map((line) => (
        <span key={line} className="tabular">
          {line}
        </span>
      ))}
    </span>
  );
}

/** 「含长上下文价」 and its bands, when the card carries one; nothing otherwise. */
function LongContext({ card }: { card: unknown }) {
  const lines = longContextLines((card ?? null) as Parameters<typeof longContextLines>[0]);
  if (lines.length === 0) return null;
  return (
    <span className="library-long-context">
      <span className="tag">含长上下文价</span>
      {lines.map((line) => (
        <span key={`${line.tier}/${line.above_input_tokens}`} className="cell-note tabular">
          {longContextText(line)}
        </span>
      ))}
    </span>
  );
}

/** One candidate of an ambiguous model, in the row's select: `provider · name — $in/$out 每百万`. */
function candidateLabel(c: LibraryEntry): string {
  const rates = c.card ? ` — $${c.card.default?.base.input}/${c.card.default?.base.output} 每百万` : " — 无可同步价格";
  return `${c.provider} · ${c.model}${rates}`;
}

/** The entry a model matched, or, for an ambiguous one, the candidate chosen. */
function EntryCell({
  item,
  chosen,
  onChoose,
}: {
  item: SyncItem;
  chosen: SyncCandidate | undefined;
  onChoose: (c: SyncCandidate | undefined) => void;
}) {
  if (item.state === "ambiguous") {
    return (
      <span className="library-entry">
        <select
          aria-label={`选择价格来源 ${item.name}`}
          value={chosen ? keyOf(chosen) : ""}
          onChange={(e) => onChoose(item.candidates.find((c) => keyOf(c) === e.target.value))}
        >
          <option value="">选择价格来源</option>
          {item.candidates.map((c) => (
            <option key={keyOf(c)} value={keyOf(c)}>
              {candidateLabel(c)}
            </option>
          ))}
        </select>
        {chosen && <LongContext card={chosen.card} />}
      </span>
    );
  }
  if (!item.entry) return <>—</>;
  const href = safeHttpUrl(item.entry.pricing_url);
  return (
    <span className="library-entry">
      <span className="mono">{keyOf(item.entry)}</span>
      <span className="cell-note">
        {SOMEONE_CHOSE[item.matched_by ?? ""] ?? (item.matched_by === "alias" ? "按 ID 或别名匹配" : "名称一致")}
        {href && (
          <>
            {" · "}
            <a href={href} target="_blank" rel="noopener noreferrer">
              价格页面
            </a>
          </>
        )}
      </span>
      {item.entry.flags.length > 0 && (
        <span className="cell-note library-flags">{item.entry.flags.map(flagText).join("、")}</span>
      )}
      <LongContext card={item.entry.card} />
    </span>
  );
}

/** How a matched entry was found, when it was not its own name alone. */
const SOMEONE_CHOSE: Record<string, string> = {
  adapter: "按上游协议匹配",
  chosen: "已选",
};

/** 「已选：provider · name」, with the way to choose again. */
function Chosen({ item, onForget }: { item: SyncItem; onForget: () => void }) {
  const binding = item.binding;
  if (!binding) return null;
  return (
    <span className="library-chosen">
      {binding.in_library
        ? `已选：${keyOf(binding)}`
        : `${keyOf(binding)} 已不在价格库中，改为按名称匹配`}
      <Button type="button" size="sm" kind="quiet" onClick={onForget}>
        重新选择
      </Button>
    </span>
  );
}

/**
 * The sync of every priceable model's baseline from the library: a preview
 * of what it would do to each, then one attested confirmation that writes
 * the 将新增 and 将更新 ones, at most the control plane's cap at a time.
 *
 * An ambiguous model is never guessed at: the operator picks one of its
 * candidates, the row then shows what syncing from it would do, and the
 * choice is written with the sync so later ones need it again only when the
 * entry leaves the library. 重新选择 forgets it.
 *
 * The confirmation sends exactly the rows on screen, each with the entry and
 * the card shown, so the control plane writes those and nothing else: a model
 * that appeared, or a price that moved, since the preview is refused, and the
 * page reads the preview again.
 */
export function LibrarySync({ canEdit, generation }: { canEdit: boolean; generation: number }) {
  const toast = useToast();
  const [preview, setPreview] = useState<SyncPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attested, setAttested] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  /** Each ambiguous model's candidate, as chosen on this page; nothing is chosen by default. */
  const [chosen, setChosen] = useState<Record<string, SyncCandidate | undefined>>({});
  /** The newest preview asked for: an answer to an older one is dropped. */
  const latest = useRef(0);

  const load = useCallback(async () => {
    const n = ++latest.current;
    try {
      const next = await library.preview();
      if (n !== latest.current) return;
      setPreview(next);
      setError(null);
    } catch (e) {
      if (n !== latest.current) return;
      setError(errorText(e));
    }
    // A tick is a claim about the rows that were on screen when it was given.
    setAttested(false);
  }, []);

  useEffect(() => {
    void load();
  }, [load, generation]);

  /** The state a row shows: an ambiguous one's, once a candidate is chosen. */
  const shown = (item: SyncItem): SyncItem => {
    const c = item.state === "ambiguous" ? chosen[item.model_id] : undefined;
    return c ? { ...item, state: c.state, entry: c } : item;
  };

  const items = preview ? byState(preview.items) : [];
  const request = preview ? syncRequest(items, preview.cap, chosen) : [];
  const writable = items.filter((i) => {
    const s = shown(i).state;
    return s === "create" || s === "update";
  }).length;
  const count = (state: string) => items.filter((i) => shown(i).state === state).length;

  async function sync() {
    if (!preview?.library || syncing || !attested || request.length === 0) return;
    setSyncing(true);
    setFailure(null);
    try {
      const done = await library.sync({ library_id: preview.library.id, attested: true, items: request });
      toast(`已同步 ${done.revisions.length} 个模型`);
      setChosen({});
      await load();
    } catch (e) {
      setFailure(failureText(e));
      // A moved preview is read again, so what is on screen is what holds now.
      if (e instanceof ApiError && e.code === "library_stale") await load();
    } finally {
      setSyncing(false);
    }
  }

  async function forget(item: SyncItem) {
    setFailure(null);
    try {
      await library.unbind(item.model_id);
      setChosen((c) => ({ ...c, [item.model_id]: undefined }));
      await load();
    } catch (e) {
      setFailure(failureText(e));
    }
  }

  return (
    <div className="panel" role="region" aria-label="同步基准价">
      <h2>
        同步基准价
        <Button size="sm" type="button" icon="refresh" onClick={() => void load()}>
          重新预览
        </Button>
      </h2>
      <p className="hint">
        按模型名称在价格库中查找对应条目：先找名称完全一致的，找不到再按数据集的 ID 与别名查找（不区分大小写，可带或不带「供应商/」前缀）。有多个候选时，先取模型自己的上游供应商的那一条，再取与上游账号协议相同的那一条；仍有多个时由你选择价格来源。同步按价格库的标价写入基准价，立即生效。
      </p>
      {error && (
        <div className="note crit" role="alert">
          {error}
        </div>
      )}
      {preview === null ? (
        !error && <Loading />
      ) : !preview.library ? (
        <EmptyState text="还没有价格库，无法预览同步。" />
      ) : items.length === 0 ? (
        <EmptyState text="还没有可定价的模型。" />
      ) : (
        <>
          <p className="library-counts" aria-label="同步概况">
            将新增 {count("create")} · 将更新 {count("update")} · 一致 {count("same")} · 多个匹配 {count("ambiguous")} ·
            需手动录入 {count("flagged")} · 无匹配 {count("unmatched")}
          </p>
          {canEdit && (
            <div className="library-confirm" role="group" aria-label="确认同步">
              <label className="check">
                <input type="checkbox" checked={attested} onChange={(e) => setAttested(e.target.checked)} />
                <span>{ATTESTATION}</span>
              </label>
              <Button
                type="button"
                kind="primary"
                busy={syncing}
                busyLabel="同步中…"
                disabled={!attested || request.length === 0}
                onClick={() => void sync()}
              >
                同步 {request.length} 个模型
              </Button>
              {writable > request.length && (
                <span className="hint">
                  共 {writable} 个模型待同步，一次最多同步 {preview.cap} 个；同步后再次确认剩下的。
                </span>
              )}
            </div>
          )}
          {failure && (
            <div className="note crit" role="alert">
              同步失败：{failure}
            </div>
          )}
          <DataTable label="同步预览">
            <thead>
              <tr>
                <th>模型</th>
                <th>状态</th>
                <th>价格库条目</th>
                <th>现行基准价</th>
                <th>价格库价格（美元/百万）</th>
              </tr>
            </thead>
            <tbody>
              {items.map((item) => {
                const row = shown(item);
                const warning =
                  row.state === "ambiguous" || row.state === "unmatched"
                    ? null
                    : longContextChange(row.entry?.card ?? null, item.current?.card ?? null);
                return (
                  <tr key={item.model_id} data-model={item.name} data-state={item.state} data-shown={row.state}>
                    <td className="trunc" title={item.name}>
                      {item.name}
                      {item.state !== "ambiguous" && <Chosen item={item} onForget={() => void forget(item)} />}
                    </td>
                    <td>
                      <span className="tag">{syncStateText(row.state)}</span>
                      {warning && <span className="cell-note library-flags">{warning}</span>}
                    </td>
                    <td>
                      <EntryCell
                        item={item}
                        chosen={chosen[item.model_id]}
                        onChoose={(c) => setChosen((all) => ({ ...all, [item.model_id]: c }))}
                      />
                      {item.state === "ambiguous" && item.binding && !chosen[item.model_id] && (
                        <span className="cell-note">
                          已选：{keyOf(item.binding)}
                          {item.binding.in_library ? "" : "（已不在价格库中）"}
                        </span>
                      )}
                    </td>
                    <td>{item.current ? <CardLines card={item.current.card} /> : "未定价"}</td>
                    <td>
                      {row.state === "ambiguous" || row.state === "unmatched" ? "—" : <CardLines card={row.entry?.card} />}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </DataTable>
        </>
      )}
    </div>
  );
}

/**
 * 从价格库同步 for one model: what the library says of it, and, when it would
 * create or change the baseline, the same attested confirmation as the
 * bulk sync, for this model alone.
 */
export function LibrarySyncOne({
  modelId,
  canEdit,
  current,
  onSynced,
}: {
  modelId: string;
  canEdit: boolean;
  /** The baseline in force as the page shows it, so the preview is read again when it changes. */
  current: BaselineRevision | null;
  onSynced: () => Promise<void>;
}) {
  const toast = useToast();
  const [preview, setPreview] = useState<SyncPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attested, setAttested] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  /** The newest preview asked for: an answer to an older one is dropped. */
  const latest = useRef(0);

  const load = useCallback(async () => {
    const n = ++latest.current;
    try {
      const next = await library.preview(modelId);
      if (n !== latest.current) return;
      setPreview(next);
      setError(null);
    } catch (e) {
      if (n !== latest.current) return;
      setError(errorText(e));
    }
    setAttested(false);
  }, [modelId]);

  useEffect(() => {
    void load();
  }, [load, current?.revision_id]);

  /** The candidate chosen on this panel, for an ambiguous model. */
  const [chosen, setChosen] = useState<SyncCandidate | undefined>(undefined);
  const item = preview?.items[0] ?? null;
  const entry = item?.state === "ambiguous" && chosen ? chosen : (item?.entry ?? null);
  const request = item ? syncRequest([item], 1, chosen ? { [item.model_id]: chosen } : {}) : [];
  /** The state shown: the chosen candidate's, for an ambiguous model. */
  const state: SyncState | undefined = item?.state === "ambiguous" && chosen ? chosen.state : item?.state;
  const writeable = state === "create" || state === "update";
  const warning =
    entry && state !== "flagged" ? longContextChange(entry.card, current?.card ?? null) : null;

  async function forget(item: SyncItem) {
    setFailure(null);
    try {
      await library.unbind(item.model_id);
      setChosen(undefined);
      await load();
    } catch (e) {
      setFailure(failureText(e));
    }
  }

  async function sync() {
    if (!preview?.library || syncing || !attested || request.length === 0) return;
    setSyncing(true);
    setFailure(null);
    try {
      await library.sync({ library_id: preview.library.id, attested: true, items: request });
      toast("已从价格库同步");
      setChosen(undefined);
      await onSynced();
      await load();
    } catch (e) {
      setFailure(failureText(e));
      if (e instanceof ApiError && e.code === "library_stale") await load();
    } finally {
      setSyncing(false);
    }
  }

  return (
    <section className="baseline-now library-one" aria-label="从价格库同步">
      <h3>从价格库同步</h3>
      {error && (
        <div className="note crit" role="alert">
          {error}
        </div>
      )}
      {preview === null ? (
        !error && <p className="hint">正在读取价格库…</p>
      ) : !preview.library ? (
        <p className="hint">还没有价格库：在「价格 → 价格库」更新，或在「导入审核」上传数据集文件。</p>
      ) : !item ? null : (
        <>
          <p>
            <span className="tag">{syncStateText(state ?? item.state)}</span> <EntryCell item={item} chosen={chosen} onChoose={setChosen} />
          </p>
          {item.binding && (
            <p className="library-chosen">
              <Chosen item={item} onForget={() => void forget(item)} />
            </p>
          )}
          {item.state === "ambiguous" && !chosen && <p className="hint">价格库中有多个候选条目：请选择价格来源。</p>}
          {item.state === "unmatched" && <p className="hint">价格库中没有与模型名称对应的条目。</p>}
          {entry && item.state !== "flagged" && (
            <p className="library-card-block">
              价格库价格（美元/百万）：
              <CardLines card={entry.card} />
            </p>
          )}
          {warning && <p className="hint">{warning}</p>}
          {canEdit && writeable && (
            <div className="library-confirm" role="group" aria-label="确认从价格库同步">
              <label className="check">
                <input type="checkbox" checked={attested} onChange={(e) => setAttested(e.target.checked)} />
                <span>{ATTESTATION}</span>
              </label>
              <Button
                type="button"
                kind="primary"
                busy={syncing}
                busyLabel="同步中…"
                disabled={!attested}
                onClick={() => void sync()}
              >
                从价格库同步
              </Button>
            </div>
          )}
          {failure && (
            <div className="note crit" role="alert">
              同步失败：{failure}
            </div>
          )}
        </>
      )}
    </section>
  );
}
