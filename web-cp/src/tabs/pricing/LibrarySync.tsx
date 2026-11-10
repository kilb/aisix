import { useCallback, useEffect, useState } from "react";
import { ApiError, library, type BaselineRevision, type SyncItem, type SyncPreview } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { flagText } from "../../lib/importReview";
import { byState, cardLines, syncRequest, syncStateText } from "../../lib/library";
import { safeHttpUrl } from "../../lib/safeUrl";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useToast } from "../../ui/Toast";
import "./PriceLibrary.css";

/** The attestation's own words: the checkbox's text, as the import's confirmation gives it. */
const ATTESTATION = "已对照官方价格页核对";

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

/** The library entry a model matched, with its provider's pricing page; or every candidate of an ambiguous one. */
function EntryCell({ item }: { item: SyncItem }) {
  if (item.state === "ambiguous") {
    return (
      <span className="cell-note">
        {item.candidates.map((c) => `${c.provider}/${c.model}`).join("、")}
      </span>
    );
  }
  if (!item.entry) return <>—</>;
  const href = safeHttpUrl(item.entry.pricing_url);
  return (
    <span className="library-entry">
      <span className="mono">
        {item.entry.provider}/{item.entry.model}
      </span>
      <span className="cell-note">
        {item.matched_by === "alias" ? "按 ID 或别名匹配" : "名称一致"}
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
    </span>
  );
}

/**
 * The sync of every priceable model's baseline from the library: a preview
 * of what it would do to each, then one attested confirmation that writes
 * the 将新增 and 将更新 ones, at most the control plane's cap at a time.
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

  const load = useCallback(async () => {
    try {
      setPreview(await library.preview());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
    // A tick is a claim about the rows that were on screen when it was given.
    setAttested(false);
  }, []);

  useEffect(() => {
    void load();
  }, [load, generation]);

  const items = preview ? byState(preview.items) : [];
  const request = preview ? syncRequest(items, preview.cap) : [];
  const writable = items.filter((i) => i.state === "create" || i.state === "update").length;
  const count = (state: string) => items.filter((i) => i.state === state).length;

  async function sync() {
    if (!preview?.library || syncing || !attested || request.length === 0) return;
    setSyncing(true);
    setFailure(null);
    try {
      const done = await library.sync({ library_id: preview.library.id, attested: true, items: request });
      toast(`已同步 ${done.revisions.length} 个模型`);
      await load();
    } catch (e) {
      setFailure(failureText(e));
      // A moved preview is read again, so what is on screen is what holds now.
      if (e instanceof ApiError && e.code === "library_stale") await load();
    } finally {
      setSyncing(false);
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
        按模型名称在价格库中查找对应条目：先找名称完全一致的，找不到再按数据集的 ID 与别名查找（不区分大小写，可带或不带「供应商/」前缀）。有多个候选时，只取模型自己的上游供应商的那一条；仍有多个则不会自动选择。同步按价格库的标价写入基准价，立即生效。
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
              {items.map((item) => (
                <tr key={item.model_id} data-state={item.state}>
                  <td className="trunc" title={item.name}>
                    {item.name}
                  </td>
                  <td>
                    <span className="tag">{syncStateText(item.state)}</span>
                  </td>
                  <td>
                    <EntryCell item={item} />
                  </td>
                  <td>{item.current ? <CardLines card={item.current.card} /> : "未定价"}</td>
                  <td>
                    {item.state === "ambiguous" || item.state === "unmatched" ? "—" : <CardLines card={item.entry?.card} />}
                  </td>
                </tr>
              ))}
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

  const load = useCallback(async () => {
    try {
      setPreview(await library.preview(modelId));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
    setAttested(false);
  }, [modelId]);

  useEffect(() => {
    void load();
  }, [load, current?.revision_id]);

  const item = preview?.items[0] ?? null;
  const request = item ? syncRequest([item], 1) : [];

  async function sync() {
    if (!preview?.library || syncing || !attested || request.length === 0) return;
    setSyncing(true);
    setFailure(null);
    try {
      await library.sync({ library_id: preview.library.id, attested: true, items: request });
      toast("已从价格库同步");
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
            <span className="tag">{syncStateText(item.state)}</span> <EntryCell item={item} />
          </p>
          {item.state === "ambiguous" && (
            <p className="hint">价格库中有多个候选条目，不会自动选择；请核对后手动录入。</p>
          )}
          {item.state === "unmatched" && <p className="hint">价格库中没有与模型名称对应的条目。</p>}
          {item.state === "flagged" && (
            <p className="hint">该条目含无法自动换算的价格，不能直接同步；请对照价格页面手动录入。</p>
          )}
          {item.entry && item.state !== "flagged" && (
            <p className="library-card-block">
              价格库价格（美元/百万）：
              <CardLines card={item.entry.card} />
            </p>
          )}
          {canEdit && (item.state === "create" || item.state === "update") && (
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
