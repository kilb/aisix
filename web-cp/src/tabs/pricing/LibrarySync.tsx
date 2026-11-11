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
import { droppedFieldText, flagText } from "../../lib/importReview";
import {
  byState,
  cardLines,
  longContextChange,
  longContextLines,
  longContextText,
  syncRequest,
  syncStateText,
} from "../../lib/library";
import { TIER_HINT } from "../../lib/rateUnits";
import { safeHttpUrl } from "../../lib/safeUrl";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useToast } from "../../ui/Toast";
import { cn } from "../../lib/cn";
import { Card, CardHeader } from "../../components/page";

/** The attestation's own words: the checkbox's text, as the import's confirmation gives it. */
const ATTESTATION = "已对照官方价格页核对";

/** The chosen candidate's value in a row's select: its entry's `provider/model`. */
function keyOf(entry: { provider: string; model: string }): string {
  return `${entry.provider}/${entry.model}`;
}

/** An ambiguous item's candidate whose entry is `key`, in this preview; nothing for any other item. */
function candidateOf(item: SyncItem, key: string | undefined): SyncCandidate | undefined {
  if (item.state !== "ambiguous" || key === undefined) return undefined;
  return item.candidates.find((c) => keyOf(c) === key);
}

/** An error as the page says it: the code's text, and each item the answer names. */
function failureText(e: unknown): string {
  if (!(e instanceof ApiError)) return errorText(e);
  const details = e.details.map((d) => d.message).filter(Boolean);
  return details.length > 0 ? `${messageFor(e)}：${details.join("；")}` : messageFor(e);
}

/** The lines of a card, one per tier and band, or a dash. */
export function CardLines({ card }: { card: unknown }) {
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

/**
 * What a sync from `entry` attests beside its card: how it was found, its
 * pricing page (linked only when it is an http(s) page), its flags, every
 * price it leaves off, and its long-context bands.
 */
function EntryMeta({ entry, how }: { entry: LibraryEntry; how: string }) {
  const href = safeHttpUrl(entry.pricing_url);
  return (
    <>
      <span className="cell-note">
        {how}
        {href && (
          <>
            {" · "}
            <a href={href} target="_blank" rel="noopener noreferrer">
              价格页面
            </a>
          </>
        )}
      </span>
      {entry.flags.length > 0 && <span className="cell-note library-flags">{entry.flags.map(flagText).join("、")}</span>}
      {entry.dropped?.length > 0 && <span className="cell-note">未计入：{entry.dropped.map(droppedFieldText).join("、")}</span>}
      <LongContext card={entry.card} />
    </>
  );
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
        {chosen && <EntryMeta entry={chosen} how="已选价格来源" />}
      </span>
    );
  }
  if (!item.entry) return <>—</>;
  return (
    <span className="library-entry">
      <span className="mono">{keyOf(item.entry)}</span>
      <EntryMeta
        entry={item.entry}
        how={SOMEONE_CHOSE[item.matched_by ?? ""] ?? (item.matched_by === "alias" ? "按 ID 或别名匹配" : "名称一致")}
      />
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
  /**
   * Each ambiguous model's chosen candidate, by its entry's `provider/model`
   * only: the candidate itself, and so the card sent, is always the one in
   * the preview on screen, however often it was read again since the choice.
   * Nothing is chosen by default.
   */
  const [chosenKeys, setChosenKeys] = useState<Record<string, string | undefined>>({});
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

  const items = preview ? byState(preview.items) : [];
  /** Each ambiguous model's chosen candidate in this preview, when it still has one by that key. */
  const chosen: Record<string, SyncCandidate | undefined> = Object.fromEntries(
    items.map((i) => [i.model_id, candidateOf(i, chosenKeys[i.model_id])]),
  );

  /** The state a row shows: an ambiguous one's, once a candidate is chosen. */
  const shown = (item: SyncItem): SyncItem => {
    const c = chosen[item.model_id];
    return c ? { ...item, state: c.state, entry: c } : item;
  };

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
      setChosenKeys({});
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
      setChosenKeys((c) => ({ ...c, [item.model_id]: undefined }));
      await load();
    } catch (e) {
      setFailure(failureText(e));
    }
  }

  const counts: [string, string][] = [
    ["将新增", "create"],
    ["将更新", "update"],
    ["一致", "same"],
    ["多个匹配", "ambiguous"],
    ["需手动录入", "flagged"],
    ["无匹配", "unmatched"],
  ];
  return (
    <Card className="mb-0" role="region" aria-label="同步基准价">
      <CardHeader
        as="h2"
        title="同步基准价"
        description={
          <>
            按模型名称在价格库中查找对应条目：先找名称完全一致的，找不到再按数据集的 ID 与别名查找（不区分大小写，可带或不带「供应商/」前缀）。有多个候选时，先取模型自己的上游供应商的那一条，再取与上游账号协议相同的那一条；仍有多个时由你选择价格来源。同步按价格库的标价写入基准价，立即生效。
            <span className="mt-1 block">{TIER_HINT}</span>
          </>
        }
        actions={
          <Button size="sm" type="button" icon="refresh" onClick={() => void load()}>
            重新预览
          </Button>
        }
      />
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
          <div className="library-counts grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-6" aria-label="同步概况">
            {counts.map(([label, state]) => (
              <div key={state} className="rounded-xl border border-border bg-muted/50 px-3 py-2">
                <div className="text-xs text-muted-foreground">{label}</div>
                <div className="text-lg font-semibold tabular-nums">{count(state)}</div>
              </div>
            ))}
          </div>
          {canEdit && (
            <div
              className="library-confirm mt-4 mb-0 flex flex-wrap items-center gap-x-4 gap-y-2 rounded-xl border border-border bg-muted/50 px-4 py-3"
              role="group"
              aria-label="确认同步"
            >
              <label className="check mb-0">
                <input type="checkbox" checked={attested} onChange={(e) => setAttested(e.target.checked)} />
                <span>{ATTESTATION}</span>
              </label>
              {writable > request.length && (
                <span className="hint text-[13px]">
                  共 {writable} 个模型待同步，一次最多同步 {preview.cap} 个；同步后再次确认剩下的。
                </span>
              )}
              <Button
                type="button"
                kind="primary"
                className="ml-auto"
                busy={syncing}
                busyLabel="同步中…"
                disabled={!attested || request.length === 0}
                onClick={() => void sync()}
              >
                同步 {request.length} 个模型
              </Button>
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
                <th>价格库价格（美元/百万 tokens）</th>
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
                    <td className="font-medium">
                      <span className="trunc" title={item.name}>
                        {item.name}
                      </span>
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
                        onChoose={(c) => {
                          setChosenKeys((all) => ({ ...all, [item.model_id]: c && keyOf(c) }));
                          // A tick is a claim about the sources on screen: another source is not checked.
                          setAttested(false);
                        }}
                      />
                      {item.state === "ambiguous" && item.binding && !chosen[item.model_id] && (
                        <span className="cell-note">
                          已选：{keyOf(item.binding)}
                          {item.binding.in_library ? "" : "（已不在价格库中）"}
                        </span>
                      )}
                    </td>
                    <td className="text-[13px]">{item.current ? <CardLines card={item.current.card} /> : "未定价"}</td>
                    <td className="text-[13px]">
                      {row.state === "ambiguous" || row.state === "unmatched" ? "—" : <CardLines card={row.entry?.card} />}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </DataTable>
        </>
      )}
    </Card>
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
  className,
}: {
  /** Spacing from what stands above it. */
  className?: string;
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

  /** The candidate chosen on this panel, for an ambiguous model, by its entry's key: resolved in the preview on screen. */
  const [chosenKey, setChosenKey] = useState<string | undefined>(undefined);
  const item = preview?.items[0] ?? null;
  const chosen = item ? candidateOf(item, chosenKey) : undefined;
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
      setChosenKey(undefined);
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
      setChosenKey(undefined);
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
    <section className={cn("library-one", className)} aria-label="从价格库同步">
      <h3 className="mb-3 text-base">从价格库同步</h3>
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
        <div className="grid gap-3">
          <div className="flex flex-wrap items-start gap-2">
            <span className="tag">{syncStateText(state ?? item.state)}</span>{" "}
            <EntryCell
              item={item}
              chosen={chosen}
              onChoose={(c) => {
                setChosenKey(c && keyOf(c));
                // A tick is a claim about the source on screen: another source is not checked.
                setAttested(false);
              }}
            />
          </div>
          {item.binding && (
            <p className="library-chosen">
              <Chosen item={item} onForget={() => void forget(item)} />
            </p>
          )}
          {item.state === "ambiguous" && !chosen && <p className="hint">价格库中有多个候选条目：请选择价格来源。</p>}
          {item.state === "unmatched" && <p className="hint">价格库中没有与模型名称对应的条目。</p>}
          {entry && item.state !== "flagged" && (
            <p className="library-card-block rounded-xl border border-border bg-muted/50 p-3 text-[13px]">
              价格库价格（美元/百万 tokens）：
              <CardLines card={entry.card} />
            </p>
          )}
          {entry && Object.keys(entry.card ?? {}).some((t) => t !== "default") && <p className="hint">{TIER_HINT}</p>}
          {warning && <p className="hint">{warning}</p>}
          {canEdit && writeable && (
            <div
              className="library-confirm m-0 flex flex-wrap items-center justify-between gap-3 border-t border-border pt-3"
              role="group"
              aria-label="确认从价格库同步"
            >
              <label className="check mb-0">
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
        </div>
      )}
    </section>
  );
}
