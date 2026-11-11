import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import {
  ApiError,
  baselines,
  imports,
  resources,
  type BaselineRow,
  type Detail,
  type ImportBatch,
  type Proposal,
  type RateCard,
  type ResourceView,
} from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { generatedDirect } from "../../lib/generatedDirect";
import {
  COMPARED_DIMENSIONS,
  datasetNotes,
  droppedFieldText,
  flagText,
  flagsBlock,
  statusText,
  unshownParts,
} from "../../lib/importReview";
import { fmtUtc } from "../../lib/money";
import { usdRateText } from "../../lib/rateUnits";
import { safeHttpUrl } from "../../lib/safeUrl";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { SidePanel } from "../../ui/SidePanel";
import { useToast } from "../../ui/Toast";
import { usdPerMillion } from "./Baselines";
import { Card, CardHeader, PageHeader } from "../../components/page";

/**
 * The credit the dataset's authors ask for (its MIT licence, `LICENSE` beside
 * the committed snapshot): the one place its name appears on this page.
 */
export const CREDIT =
  "价格数据来自公开数据集 pydantic/genai-prices（MIT 许可），属第三方数据；确认前请对照官方价格页核对。";

/** The most the upload may carry: the control plane's own limit, checked here first. */
const MAX_BYTES = 8 << 20;

/** A proposal's `default` tier rates, when its card has both. */
function headline(card: RateCard | null): { input: string; output: string } | null {
  const base = card?.default?.base;
  if (!base || base.input === undefined || base.output === undefined) return null;
  return { input: base.input, output: base.output };
}

/**
 * Whether a proposal can be confirmed as it stands: pending, held back by no
 * blocking flag, and with a card fully shown. A price the card cannot meter
 * is dropped from it rather than blocking it, and is listed in `dropped`.
 */
function confirmable(p: Proposal): boolean {
  return (
    p.status === "pending" &&
    !flagsBlock(p.flags) &&
    p.card !== null &&
    unshownParts(p.card).length === 0
  );
}

/**
 * Whether the selection may confirm it: confirmable, and matched to a model by
 * the upload. One with no suggested model needs its own panel, where the
 * model is picked.
 */
function bulkable(p: Proposal): boolean {
  return confirmable(p) && p.suggested_model_id !== null;
}

/**
 * When a proposal starts if confirmed now, stamped at send: its own date, or
 * now for one in effect at upload and for a dated one whose date has passed,
 * since a price never starts in the past. `immediate` while the stamp is now,
 * so a start the clock overtook between stamp and check is stamped again.
 */
function startOf(p: Proposal): EffectiveResolver {
  const own = p.effective_from === null ? null : Date.parse(p.effective_from);
  return {
    immediate: own === null || own <= Date.now(),
    stamp: () => new Date(own === null ? Date.now() : Math.max(own, Date.now())).toISOString(),
  };
}

/** One card's rate of `dimension`, as the card wrote it, or null. */
function rate(card: RateCard | null, dimension: string): string | null {
  return card?.default?.base?.[dimension] ?? null;
}

/** The model document fields this page reads, to name the models a proposal can price. */
interface ModelDoc {
  display_name?: string;
}

/** 价格 · 导入审核: upload the public price dataset, review what it proposes, and decide each. */
export function PricingImport({ canEdit, onGoto }: TabProps) {
  const toast = useToast();
  const [batches, setBatches] = useState<ImportBatch[] | null>(null);
  const [batchId, setBatchId] = useState<string | null>(null);
  const [all, setAll] = useState(false);
  const [proposals, setProposals] = useState<Proposal[] | null>(null);
  const [models, setModels] = useState<ResourceView<ModelDoc>[]>([]);
  const [updates, setUpdates] = useState<BaselineRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  /** The dataset's own notes for each entry, read from the file uploaded in this session. */
  const [comments, setComments] = useState<Record<string, string>>({});
  /**
   * The batch whose file those notes came from. One file's notes are a claim
   * about that file's prices, so they are shown only while its batch is on
   * screen: uploading B and stepping back to A must not put B's notes beside
   * A's proposals. Held with its batch and compared on every read, the way the
   * attestation is, rather than cleared by each handler that changes one.
   */
  const [commentsFor, setCommentsFor] = useState<string | null>(null);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  /**
   * The batch the attestation was ticked under, not a bare boolean: a tick is
   * a claim about the pages of the batch on screen when it was given, so it
   * may not survive that batch leaving the selector. Held with its batch and
   * compared on every read below, rather than cleared by every handler that
   * could change one.
   */
  const [attestedFor, setAttestedFor] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [results, setResults] = useState<string[]>([]);
  const [view, setView] = useState<"proposals" | "updates">("proposals");

  const loadBatches = useCallback(async () => {
    try {
      const items = await imports.list();
      setBatches(items);
      setError(null);
      // The open batch is what an operator reviews; the newest, if none is open.
      setBatchId((id) => id ?? items.find((b) => b.status === "open")?.id ?? items[0]?.id ?? null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  /**
   * Reads a batch's proposals. A `quiet` read (after a decision) keeps the
   * list on screen meanwhile, so an open proposal's panel stays mounted and
   * can say what happened; switching batches shows the loading state.
   */
  const loadProposals = useCallback(async (id: string, every: boolean, quiet = false) => {
    try {
      if (!quiet) setProposals(null);
      const detail = await imports.detail(id, every);
      setProposals(detail.proposals);
      setError(null);
    } catch (e) {
      // The list is not emptied: "nothing to review" and "the read failed" are
      // different answers, and the second one is already the alert above.
      setError(errorText(e));
    }
  }, []);

  /** The catalogue's outstanding updates, and the models a proposal may name. */
  const loadSide = useCallback(async () => {
    baselines.list().then(
      (rows) => setUpdates(rows.filter((r) => r.catalogue_update)),
      () => undefined,
    );
    resources.list<ModelDoc>("models").then(setModels, () => undefined);
  }, []);

  useEffect(() => {
    void loadBatches();
    void loadSide();
  }, [loadBatches, loadSide]);

  useEffect(() => {
    if (batchId) void loadProposals(batchId, all);
  }, [batchId, all, loadProposals]);

  const detail = proposals?.find((p) => p.id === selected) ?? null;
  const batch = batches?.find((b) => b.id === batchId) ?? null;

  const shown = useMemo(() => {
    const text = filter.trim().toLowerCase();
    if (!text) return proposals ?? [];
    return (proposals ?? []).filter((p) => `${p.provider}/${p.model}`.toLowerCase().includes(text));
  }, [proposals, filter]);

  /** A decision was made: the list and the catalogue's updates are read again. */
  const afterDecision = useCallback(async () => {
    if (batchId) await loadProposals(batchId, all, true);
    await loadSide();
  }, [batchId, all, loadProposals, loadSide]);

  // The selection is of the whole batch, not of what the search shows: a
  // proposal picked and then filtered out of view is still confirmed.
  const chosen = (proposals ?? []).filter((p) => picked.has(p.id) && bulkable(p));
  const pages = [...new Set(chosen.flatMap((p) => (p.pricing_url ? [p.pricing_url] : [])))];
  /**
   * The attestation this page holds for the batch on screen. A tick given while
   * another batch was selected is not one: `attestedFor` is read here, at the
   * one place both the checkbox and the button read it, so no caller can hold a
   * stale one by remembering `true`.
   */
  const attested = attestedFor !== null && attestedFor === batchId;
  /** The notes this page may show: its own file's, and no other batch's. */
  const notes = commentsFor !== null && commentsFor === batchId ? comments : {};

  /** Switching what is reviewed drops every claim about the last one. */
  function switchBatch(next: string | null) {
    setBatchId(next);
    setAttestedFor(null);
    setPicked(new Set());
    setResults([]);
  }

  async function confirmSelection() {
    // The guard, not the button's `disabled`: a tick given under another batch
    // must not authorize this one's prices, however the confirm was invoked.
    if (!batchId || !attested || running) return;
    setRunning(true);
    setResults([]);
    const done: string[] = [];
    for (const p of chosen) {
      const entry = `${p.provider}/${p.model}`;
      try {
        await sendAt(startOf(p), (effective_from) =>
          imports.confirm(batchId, p.id, { model_id: p.suggested_model_id ?? "", effective_from, attested: true }),
        );
        done.push(`已确认 ${entry}`);
      } catch (e) {
        // A lost answer is not a refusal: the row is read again below, and its
        // state — not this line — is what the operator should act on.
        const lost = e instanceof ApiError && (e.code === "network" || e.code === "bad_response");
        done.push(
          `${entry}：${
            e instanceof ApiError && e.code === "proposal_state"
              ? messageFor(e)
              : lost
                ? `${messageFor(e)}；是否已写入未知，请看该行刷新后的状态`
                : errorText(e)
          }`,
        );
      }
    }
    setResults(done);
    setPicked(new Set());
    setAttestedFor(null);
    setRunning(false);
    toast(`确认了 ${done.filter((d) => d.startsWith("已确认")).length} 条`);
    await afterDecision();
  }

  return (
    <>
      <PageHeader
        title="导入审核"
        description={CREDIT}
        actions={
          <Button type="button" icon="refresh" onClick={() => void loadBatches()}>
            刷新
          </Button>
        }
      />
      <div className={detail ? "with-side is-open gap-y-4" : "with-side"}>
        <div className="side-main min-w-0">
          <ImportUpload
            canEdit={canEdit}
            onUploaded={async (batch, fileNotes) => {
              setFilter("");
              switchBatch(batch.id);
              // Installed with the batch, not before it: a note is a claim about
              // the file this batch came from, so it is held with that batch's id.
              setComments(fileNotes);
              setCommentsFor(batch.id);
              await loadBatches();
              await loadProposals(batch.id, all);
            }}
          />

          <Card className="mb-0">
            <div className="sections-switch -mx-6 -mt-2 px-4" role="group" aria-label="审核内容">
              <button type="button" aria-pressed={view === "proposals"} onClick={() => setView("proposals")}>
                数据集提议
              </button>
              <button type="button" aria-pressed={view === "updates"} onClick={() => setView("updates")}>
                官方目录已更新（{updates.length}）
              </button>
            </div>
            {error && (
              <div className="note crit" role="alert">
                {error}
              </div>
            )}
            {view === "updates" ? (
              <CatalogueUpdates canEdit={canEdit} rows={updates} onSaved={afterDecision} />
            ) : batches === null ? (
              !error && <Loading />
            ) : batches.length === 0 ? (
              <EmptyState text="还没有上传过价格数据集。选择数据集文件并填写版本后上传。" mark="route" />
            ) : (
              <>
                <div className="import-controls mt-0 grid gap-x-4 gap-y-3 md:grid-cols-[minmax(0,1.6fr)_minmax(0,1fr)] xl:grid-cols-[minmax(0,1.6fr)_minmax(0,1fr)_auto] xl:items-end [&_.field]:mb-0">
                  <Field label="上传批次">
                    {(aria) => (
                      <select {...aria} value={batchId ?? ""} onChange={(e) => switchBatch(e.target.value)}>
                        {batches.map((b) => (
                          <option key={b.id} value={b.id}>
                            {b.dataset_ref} · {fmtUtc(b.created_at)} · {b.status === "open" ? "待审" : "已被取代"} ·{" "}
                            {b.proposals} 条提议（{b.flagged} 条带标记）
                          </option>
                        ))}
                      </select>
                    )}
                  </Field>
                  <Field label="筛选模型">
                    {(aria) => (
                      <input {...aria} autoComplete="off" placeholder="按提供方或模型名筛选" value={filter} onChange={(e) => setFilter(e.target.value)} />
                    )}
                  </Field>
                  <label className="check mb-0 xl:h-9">
                    <input
                      type="checkbox"
                      checked={all}
                      onChange={(e) => {
                        // The rows this brings into view were not on screen when
                        // any tick was given, so the claim does not carry to them.
                        setAll(e.target.checked);
                        setAttestedFor(null);
                        setPicked(new Set());
                        setResults([]);
                      }}
                    />
                    <span>显示与当前基准价相同的提议</span>
                  </label>
                </div>

                {batch && (
                  <p className="hint tabular mt-3 text-[13px]">
                    本次上传 {batch.proposals} 条提议，其中 {batch.flagged} 条带标记；由 {batch.created_by.email} 上传于{" "}
                    {fmtUtc(batch.created_at)}。
                  </p>
                )}

                {canEdit && (proposals ?? []).some(bulkable) && (
                  <div
                    className="import-bulk mt-4 mb-0 flex flex-wrap items-center gap-x-4 gap-y-2 rounded-xl border border-border bg-muted/50 px-4 py-3"
                    role="group"
                    aria-label="批量确认"
                  >
                    <label className="check">
                      <input
                        type="checkbox"
                        checked={attested}
                        onChange={(e) => setAttestedFor(e.target.checked ? batchId : null)}
                      />
                      <span>已对照官方价格页核对</span>
                    </label>
                    {pages.length > 0 && (
                      <span className="cell-note">
                        核对页面：
                        {pages.map((url, i) => {
                          const href = safeHttpUrl(url);
                          return (
                            <span key={url}>
                              {i > 0 && "、"}
                              {href ? (
                                <a href={href} target="_blank" rel="noopener noreferrer">
                                  {url}
                                </a>
                              ) : (
                                <span className="trunc" title={url}>
                                  {url}
                                </span>
                              )}
                            </span>
                          );
                        })}
                      </span>
                    )}
                    <Button
                      type="button"
                      kind="primary"
                      className="ml-auto"
                      disabled={!attested || chosen.length === 0}
                      busy={running}
                      busyLabel="确认中…"
                      onClick={() => void confirmSelection()}
                    >
                      确认所选（{chosen.length}）
                    </Button>
                  </div>
                )}
                {results.length > 0 && (
                  <div className="note" role="status" aria-label="确认结果">
                    <ul className="import-results">
                      {results.map((r, i) => (
                        <li key={i}>{r}</li>
                      ))}
                    </ul>
                  </div>
                )}

                {proposals === null ? (
                  error ? null : <Loading />
                ) : shown.length === 0 ? (
                  <EmptyState text="这一批没有待处理的提议。换一个批次，或勾选「显示与当前基准价相同的提议」。" />
                ) : (
                  <DataTable label="提议" scroll>
                    <thead>
                      <tr>
                        <th className="import-pick" aria-label="选择" />
                        <th>模型</th>
                        <th>生效时间（UTC）</th>
                        <th className="num">输入</th>
                        <th className="num">输出</th>
                        <th>标记</th>
                        <th>状态</th>
                      </tr>
                    </thead>
                    <tbody>
                      {shown.map((p) => {
                        const entry = `${p.provider}/${p.model}`;
                        const rates = headline(p.card);
                        return (
                          <tr key={p.id} data-status={p.status}>
                            <td className="import-pick">
                              {canEdit && bulkable(p) && (
                                <input
                                  type="checkbox"
                                  aria-label={`选择 ${entry}`}
                                  checked={picked.has(p.id)}
                                  onChange={(e) =>
                                    setPicked((old) => {
                                      const next = new Set(old);
                                      if (e.target.checked) next.add(p.id);
                                      else next.delete(p.id);
                                      return next;
                                    })
                                  }
                                />
                              )}
                            </td>
                            <td>
                              {/* A long entry wraps rather than widening the list past its card. */}
                              <button
                                type="button"
                                className="link-button whitespace-normal [overflow-wrap:anywhere]"
                                title={entry}
                                aria-expanded={selected === p.id}
                                onClick={() => setSelected(p.id)}
                              >
                                {entry}
                              </button>
                              <span className="cell-note">
                                <span className="tag">第三方数据</span>
                                {notes[entry] && (
                                  <>
                                    {" "}
                                    <span className="trunc" title={notes[entry]}>
                                      {notes[entry]}
                                    </span>
                                  </>
                                )}
                              </span>
                            </td>
                            <td className="tabular">{p.effective_from ? fmtUtc(p.effective_from) : "立即生效"}</td>
                            <td className="num tabular">{rates ? usdPerMillion(rates.input) : "—"}</td>
                            <td className="num tabular">{rates ? usdPerMillion(rates.output) : "—"}</td>
                            <td className="import-flags text-[13px]">
                              <div className="max-w-56 whitespace-normal">{p.flags.length > 0 ? p.flags.map(flagText).join("、") : "—"}</div>
                            </td>
                            <td>
                              {statusText(p.status)}
                              {p.reject_reason && <span className="cell-note trunc" title={p.reject_reason}>{p.reject_reason}</span>}
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </DataTable>
                )}
              </>
            )}
          </Card>
        </div>
        {detail && batchId && (
          <ProposalPanel
            key={detail.id}
            batchId={batchId}
            proposal={detail}
            comment={notes[`${detail.provider}/${detail.model}`] ?? null}
            models={models}
            canEdit={canEdit}
            onClose={() => setSelected(null)}
            onDecided={afterDecision}
            onManual={() => onGoto("pricing", "baselines")}
          />
        )}
      </div>
    </>
  );
}

/** The upload: the file, read by the browser, and the revision it is known by. */
function ImportUpload({
  canEdit,
  onUploaded,
}: {
  canEdit: boolean;
  onUploaded: (batch: ImportBatch, notes: Record<string, string>) => Promise<void>;
}) {
  const toast = useToast();
  const [ref, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [file, setFile] = useState<File | null>(null);
  const [revision, setRevision] = useState("");
  const [uploading, setUploading] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);

  if (!canEdit) return null;

  async function upload(e: React.FormEvent) {
    e.preventDefault();
    if (uploading) return;
    setFailure(null);
    const refused: Detail[] = [];
    if (!file) refused.push({ path: "/file", message: "请选择数据集文件" });
    const ref = revision.trim();
    if (!/^[A-Za-z0-9._/-]{1,100}$/.test(ref)) {
      refused.push({ path: "/dataset_ref", message: "数据集版本只能包含字母、数字和 . _ / -，最长 100 个字符" });
    }
    // The control plane refuses a body over its limit with 413; the browser
    // knows the size before sending, so a large file never leaves the page.
    if (file && file.size > MAX_BYTES) {
      refused.push({ path: "/file", message: "数据集文件不能超过 8 MiB" });
    }
    if (refused.length > 0) {
      setDetails(refused);
      focusRefusal();
      return;
    }
    setDetails([]);
    setUploading(true);
    try {
      const text = await file!.text();
      const batch = await imports.upload(ref, text);
      // The dataset's own notes, keyed by the entry they belong to: they are
      // not stored with a proposal, so this session's file is where they come
      // from. Installed only once the batch exists — a file the control plane
      // refused must not leave its notes beside an earlier file's proposals —
      // and replacing the last file's rather than merging with them, since the
      // label says which dataset they come from.
      toast("已上传");
      setFile(null);
      setRevision("");
      await onUploaded(batch, datasetNotes(text));
    } catch (err) {
      if (err instanceof ApiError && err.code === "payload_too_large") {
        setFailure("数据集文件过大，控制面拒绝接收（上限 8 MiB）");
      } else if (err instanceof ApiError && err.status === 422 && err.details.length > 0) {
        setDetails(err.details);
        focusRefusal();
      } else {
        setFailure(err instanceof ApiError ? messageFor(err) : errorText(err));
      }
    } finally {
      setUploading(false);
    }
  }

  return (
    <form ref={ref} className="panel import-upload" aria-label="上传价格数据集" onSubmit={(e) => void upload(e)} noValidate>
      <CardHeader
        title="上传价格数据集"
        description="选择数据集文件并填写它的版本。上传后，它给出的每条价格都出现在下面的提议里，逐条审核。"
      />
      <div className="@container">
        <div className="grid items-start gap-4 @2xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] [&_.field]:mb-0">
          <Field label="数据集文件" error={problemAt(details, (p) => p === "/file")}>
            {(aria) => (
              <input
                {...aria}
                type="file"
                accept="application/json,.json"
                onChange={(e) => setFile(e.target.files?.[0] ?? null)}
              />
            )}
          </Field>
          <Field
            label="数据集版本"
            hint="文件对应的版本：仓库的 tag、提交或文件名"
            error={problemAt(details, (p) => p === "/dataset_ref")}
          >
            {(aria) => (
              <input
                {...aria}
                autoComplete="off"
                placeholder="genai-prices-v2-7de757d"
                value={revision}
                onChange={(e) => setRevision(e.target.value)}
              />
            )}
          </Field>
          <div className="flex justify-end @2xl:pt-[25px]">
            <Button type="submit" kind="primary" busy={uploading} busyLabel="上传中…">
              上传
            </Button>
          </div>
        </div>
      </div>
      {failure && (
        <div className="note crit mb-0" role="alert">
          {failure}
        </div>
      )}
    </form>
  );
}

/** One model's proposal, with everything a decision needs. */
function ProposalPanel({
  batchId,
  proposal,
  comment,
  models,
  canEdit,
  onClose,
  onDecided,
  onManual,
}: {
  batchId: string;
  proposal: Proposal;
  comment: string | null;
  models: ResourceView<ModelDoc>[];
  canEdit: boolean;
  onClose: () => void;
  onDecided: () => Promise<void>;
  onManual: () => void;
}) {
  const entry = `${proposal.provider}/${proposal.model}`;
  // A decision someone else made first (`409 proposal_state`): said here, on
  // the panel, which outlives the form that met it once the row is read again.
  const [stale, setStale] = useState<string | null>(null);
  const decided = async (notice?: string) => {
    setStale(notice ?? null);
    await onDecided();
  };
  /** What the comparison cannot show, so a refusal can say which part. */
  const unshown = unshownParts(proposal.card);
  /** The dataset fields this card leaves off; an older row has none. */
  const dropped = proposal.dropped ?? [];
  return (
    <SidePanel title={entry} label={`${entry} 的提议`} onClose={onClose}>
      {stale && (
        <div className="note crit" role="alert">
          {stale}
        </div>
      )}
      <p className="import-source mt-4 flex flex-wrap items-center gap-2">
        <span className="tag">第三方数据</span>{" "}
        <span className="cell-note">
          数据集{" "}
          {proposal.pricing_url && safeHttpUrl(proposal.pricing_url) ? (
            <a href={safeHttpUrl(proposal.pricing_url)!} target="_blank" rel="noopener noreferrer">
              价格页面
            </a>
          ) : proposal.pricing_url ? (
            <span className="trunc" title={proposal.pricing_url}>
              {proposal.pricing_url}
            </span>
          ) : (
            "未提供价格页面"
          )}
          {" · "}
          <span className="tabular">核对日期 {proposal.prices_checked ?? "未提供"}</span>
        </span>
      </p>
      {proposal.flags.length > 0 && (
        <div className="note warn" role="status">
          {flagsBlock(proposal.flags)
            ? `这项价格带有标记，不能直接确认：${proposal.flags.map(flagText).join("、")}。`
            : `这项价格含无法计价的收费项：${proposal.flags.map(flagText).join("、")}。`}
        </div>
      )}
      {dropped.length > 0 && (
        <div className="note" role="status">
          未计入：{dropped.map(droppedFieldText).join("、")}
        </div>
      )}
      {comment && (
        <div className="note">
          <strong>数据集备注</strong>
          <p>{comment}</p>
        </div>
      )}
      <PriceComparison proposal={proposal} />
      {proposal.status !== "pending" ? (
        <div className="note" role="status">
          {statusText(proposal.status)}
          {proposal.reject_reason && <>：{proposal.reject_reason}</>}
          {proposal.decided_at && <span className="cell-note tabular">（{fmtUtc(proposal.decided_at)}）</span>}
        </div>
      ) : !canEdit ? null : confirmable(proposal) ? (
        <ConfirmForm batchId={batchId} proposal={proposal} models={models} onDecided={decided} />
      ) : (
        <>
          {/* A card whose whole bill the comparison cannot show is refused,
              not confirmed on a fraction of it: the operator is told what is
              off screen so they can price it by hand from the source page.
              A dropped price never lands here: it is not on the card. */}
          {!flagsBlock(proposal.flags) && unshown.length > 0 && (
            <div className="note warn" role="status">
              这项价格含价格对照无法展示的部分：{unshown.join("、")}。确认后会按这些价格计费，因此不能在这里确认，请到「基准价」手动录入。
            </div>
          )}
          <p className="actions justify-end">
            <Button type="button" onClick={onManual}>
              去「基准价」手动录入
            </Button>
          </p>
        </>
      )}
      {proposal.status === "pending" && canEdit && (
        <RejectForm batchId={batchId} proposal={proposal} onDecided={decided} />
      )}
    </SidePanel>
  );
}

/** One comparison cell: the card's own rate text, per million tokens or, for a rate per call, per thousand calls. */
function shownRate(card: RateCard | null, dimension: string): string {
  const r = rate(card, dimension);
  return r === null ? "—" : usdRateText(dimension, r);
}

/** The dataset's card against the model's baseline in force and the catalogue's entry. */
function PriceComparison({ proposal }: { proposal: Proposal }) {
  const dataset = proposal.card;
  const current = (proposal.current_baseline?.card ?? null) as RateCard | null;
  const catalogue = proposal.catalogue_entry?.card ?? null;
  const rows = COMPARED_DIMENSIONS.filter(
    ([d]) => rate(dataset, d) !== null || rate(current, d) !== null || rate(catalogue, d) !== null,
  );
  if (rows.length === 0) return null;
  return (
    <section className="import-compare mt-5" aria-label="价格对照">
      <h3 className="text-sm">价格对照（token 按每百万 tokens，联网搜索按每千次）</h3>
      <DataTable label="价格对照" compact>
        <thead>
          <tr>
            <th>价格项</th>
            <th className="num">数据集</th>
            <th className="num">现行基准价</th>
            <th className="num">官方目录</th>
          </tr>
        </thead>
        <tbody>
          {rows.map(([dimension, label]) => (
            <tr key={dimension}>
              <td>{label}</td>
              <td className="num tabular">{shownRate(dataset, dimension)}</td>
              <td className="num tabular">{shownRate(current, dimension)}</td>
              <td className="num tabular">{shownRate(catalogue, dimension)}</td>
            </tr>
          ))}
        </tbody>
      </DataTable>
      {proposal.catalogue_entry && (
        <p className="hint">
          官方目录条目 {proposal.catalogue_entry.key}，抄录于 {proposal.catalogue_entry.as_of}
        </p>
      )}
    </section>
  );
}

/** Confirms one proposal: the model it prices, the attestation, and its effective time. */
function ConfirmForm({
  batchId,
  proposal,
  models,
  onDecided,
}: {
  batchId: string;
  proposal: Proposal;
  models: ResourceView<ModelDoc>[];
  /** After a decision; `notice` when it was someone else's, made first. */
  onDecided: (notice?: string) => Promise<void>;
}) {
  const toast = useToast();
  const [formRef, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [model, setModel] = useState(proposal.suggested_model_id ?? "");
  const [checked, setChecked] = useState(false);
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });

  /** The proposal's own date, when it has one: nothing earlier may be confirmed. */
  const dated = proposal.effective_from !== null;
  /**
   * Whether the operator chooses the start: always for a proposal in effect
   * at upload, and for a dated one once its date has passed, since a price
   * cannot start in the past and its own date can no longer be sent. A dated
   * one refused at `/effective_from` (its date passed while the form was
   * open) offers the choice from then on.
   */
  const [choosing, setChoosing] = useState(() => !dated || Date.parse(proposal.effective_from!) <= Date.now());
  /** Whether the chosen start is stamped at submit (立即生效) rather than typed. */
  const [stamped, setStamped] = useState(true);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (saving || !checked) return;
    setFailure(null);
    if (!model) {
      setDetails([{ path: "/model_id", message: "请选择这项价格对应的模型" }]);
      focusRefusal();
      return;
    }
    setDetails([]);
    setSaving(true);
    try {
      const send = (effective_from: string) =>
        imports.confirm(batchId, proposal.id, { model_id: model, effective_from, attested: true });
      const answer = choosing ? await sendAt(resolver.current, send) : await send(proposal.effective_from!);
      if (answer === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC 计" }]);
        focusRefusal();
        return;
      }
      toast("已确认");
      await onDecided();
    } catch (err) {
      if (err instanceof ApiError && err.code === "proposal_state") {
        // Somebody decided it first: the row is stale, so read it again.
        await onDecided(messageFor(err));
      } else if (err instanceof ApiError && err.status === 422 && err.details.length > 0) {
        setDetails(err.details);
        if (err.details.some((d) => d.path === "/effective_from")) setChoosing(true);
        focusRefusal();
      } else if (err instanceof ApiError && (err.code === "network" || err.code === "bad_response")) {
        // The answer was lost, so whether this confirmation was written is
        // unknown. Reading the proposal again is how to find out: a second
        // attempt is safe, but one that lands on a proposal already decided
        // answers `proposal_state` and reads as someone else's decision.
        setFailure(`${messageFor(err)}。这次确认是否已写入无法确定：刷新这一行即可看到实际状态。`);
      } else {
        setFailure(err instanceof ApiError ? messageFor(err) : errorText(err));
      }
    } finally {
      setSaving(false);
    }
  }

  const stray = details.filter(
    (d) => !(d.path === "/model_id" || d.path === "/effective_from" || d.path === "/attested" || d.path === "/flags"),
  );
  const startRefused = problemAt(details, (p) => p === "/effective_from");

  return (
    <form ref={formRef} aria-label="确认提议" className="import-decide mt-6 border-t border-border pt-5" onSubmit={(e) => void submit(e)} noValidate>
      <h3 className="mb-3 text-base">确认</h3>
      <Field label="模型" hint="确认后，这项价格成为该模型的基准价" error={problemAt(details, (p) => p === "/model_id")}>
        {(aria) => (
          <select {...aria} value={model} onChange={(e) => setModel(e.target.value)}>
            <option value="">请选择</option>
            {/* A channel's generated direct document is priced through its public model. */}
            {models
              .filter((m) => !generatedDirect(m))
              .map((m) => (
                <option key={m.id} value={m.id}>
                  {m.doc.display_name || m.id}
                </option>
              ))}
          </select>
        )}
      </Field>
      <label className="check">
        <input type="checkbox" checked={checked} onChange={(e) => setChecked(e.target.checked)} />
        <span>已对照官方价格页核对</span>
      </label>
      {proposal.pricing_url && (
        <p className="hint">
          核对页面：{" "}
          {safeHttpUrl(proposal.pricing_url) ? (
            <a href={safeHttpUrl(proposal.pricing_url)!} target="_blank" rel="noopener noreferrer">
              {proposal.pricing_url}
            </a>
          ) : (
            <span className="trunc" title={proposal.pricing_url}>
              {proposal.pricing_url}
            </span>
          )}
        </p>
      )}
      {choosing ? (
        <>
          {dated && (
            <p className="hint tabular">
              该提议的生效时间 {fmtUtc(proposal.effective_from!)} 已过；价格只能从现在或将来开始生效，请选择生效时间。
            </p>
          )}
          <EffectiveFrom
            onResolve={(r) => {
              resolver.current = r;
              setStamped(r.immediate);
            }}
            error={startRefused}
          />
        </>
      ) : (
        <p className="hint tabular">生效时间：{fmtUtc(proposal.effective_from!)}（该提议自己的生效时间）</p>
      )}
      {/* The time field shows a refusal of a typed time itself; a start the form stamped has no field to show it in. */}
      {startRefused && !(choosing && !stamped) && (
        <div className="note crit" role="alert">
          {startRefused}
        </div>
      )}
      {problemAt(details, (p) => p === "/attested") && (
        <div className="note crit" role="alert">
          {problemAt(details, (p) => p === "/attested")}
        </div>
      )}
      {problemAt(details, (p) => p === "/flags") && (
        <div className="note crit" role="alert">
          {problemAt(details, (p) => p === "/flags")}
        </div>
      )}
      {stray.length > 0 && (
        <div className="note crit" role="alert">
          {stray.map((d) => d.message).join("；")}
        </div>
      )}
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      <div className="mt-4 flex justify-end">
        <Button type="submit" kind="primary" disabled={!checked} busy={saving} busyLabel="确认中…">
          确认
        </Button>
      </div>
    </form>
  );
}

/** Rejects one proposal: a reason is required, and nothing but the decision is written. */
function RejectForm({
  batchId,
  proposal,
  onDecided,
}: {
  batchId: string;
  proposal: Proposal;
  /** After a decision; `notice` when it was someone else's, made first. */
  onDecided: (notice?: string) => Promise<void>;
}) {
  const toast = useToast();
  const [ref, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [reason, setReason] = useState("");
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setFailure(null);
    if (reason.trim() === "") {
      setDetails([{ path: "/reason", message: "请填写拒绝原因" }]);
      focusRefusal();
      return;
    }
    setDetails([]);
    setSaving(true);
    try {
      await imports.reject(batchId, proposal.id, reason.trim());
      toast("已驳回");
      await onDecided();
    } catch (err) {
      if (err instanceof ApiError && err.code === "proposal_state") {
        await onDecided(messageFor(err));
      } else if (err instanceof ApiError && err.status === 422 && err.details.length > 0) {
        setDetails(err.details);
        focusRefusal();
      } else {
        setFailure(err instanceof ApiError ? messageFor(err) : errorText(err));
      }
    } finally {
      setSaving(false);
    }
  }

  return (
    <form ref={ref} aria-label="驳回提议" className="import-decide mt-6 border-t border-border pt-5" onSubmit={(e) => void submit(e)} noValidate>
      <h3 className="mb-3 text-base">驳回</h3>
      <Field label="拒绝原因" hint="最多 500 字，记入这条提议" error={problemAt(details, (p) => p === "/reason")}>
        {(aria) => (
          <textarea {...aria} rows={2} autoComplete="off" value={reason} onChange={(e) => setReason(e.target.value)} />
        )}
      </Field>
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      <div className="mt-4 flex justify-end">
        <Button type="submit" busy={saving} busyLabel="驳回中…">
          驳回
        </Button>
      </div>
    </form>
  );
}

/** Models whose baseline the built-in catalogue has moved past, each confirmable from its entry. */
function CatalogueUpdates({
  canEdit,
  rows,
  onSaved,
}: {
  canEdit: boolean;
  rows: BaselineRow[];
  onSaved: () => Promise<void>;
}) {
  const toast = useToast();
  const [saving, setSaving] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  async function confirm(row: BaselineRow) {
    const update = row.catalogue_update;
    if (!update || saving) return;
    setSaving(row.model_id);
    setFailure(null);
    try {
      const resolver: EffectiveResolver = { immediate: true, stamp: () => new Date().toISOString() };
      await sendAt(resolver, (effective_from) =>
        baselines.create({ model_id: row.model_id, effective_from, catalogue_key: update.entry_key }),
      );
      toast("已按官方目录更新");
      await onSaved();
    } catch (err) {
      setFailure(`${row.name}：${errorText(err)}`);
    } finally {
      setSaving(null);
    }
  }

  if (rows.length === 0) {
    return <EmptyState text="没有需要确认的官方目录更新：来自官方目录的基准价都与目录一致。" />;
  }

  return (
    <section className="import-updates" aria-label="官方目录已更新">
      <p className="hint">这些模型的基准价来自官方目录，而目录现在对同一条目给出了不同的价格：确认后按目录更新，立即生效。</p>
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      <DataTable label="官方目录已更新" compact>
        <thead>
          <tr>
            <th>模型</th>
            <th>目录条目</th>
            <th>目录版本</th>
            <th>输入</th>
            <th>输出</th>
            {canEdit && <th />}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => {
            const update = row.catalogue_update!;
            const first = update.entries[0] ?? null;
            const rates = headline(first?.card ?? null);
            return (
              <tr key={row.model_id}>
                <td>{row.name}</td>
                <td className="tabular">{update.entry_key}</td>
                <td className="tabular">{update.catalogue_version}</td>
                <td className="num tabular">{rates ? usdPerMillion(rates.input) : "—"}</td>
                <td className="num tabular">{rates ? usdPerMillion(rates.output) : "—"}</td>
                {canEdit && (
                  <td className="right">
                    <Button
                      size="sm"
                      type="button"
                      busy={saving === row.model_id}
                      busyLabel="确认中…"
                      onClick={() => void confirm(row)}
                    >
                      按目录确认
                    </Button>
                  </td>
                )}
              </tr>
            );
          })}
        </tbody>
      </DataTable>
    </section>
  );
}
