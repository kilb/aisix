import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import { RateCardEditor } from "../../components/RateCardEditor";
import {
  ApiError,
  baselines,
  groupPrices,
  pricing,
  resources,
  type BaselineRevision,
  type BaselineRow,
  type BaselineSource,
  type CatalogueEntry,
  type Detail,
  type ModelGroupPriceRow,
  type PriceWarning,
  type RateCard,
  type ResourceView,
} from "../../lib/api";
import { messageFor, problemAt, warningText } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { catalogueUpdateWords } from "../../lib/baselineUpdate";
import { fmtCnyPerMillion } from "../../lib/money";
import { fmtBeijing } from "../../lib/time";
import { safeHttpUrl } from "../../lib/safeUrl";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useRecordTable } from "../../lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { Field, useRefusalFocus } from "../../ui/Field";
import { SidePanel } from "../../ui/SidePanel";
import { useToast } from "../../ui/Toast";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { LibrarySyncOne } from "./LibrarySync";
import { PriceCardDetails } from "./PriceCardDetails";
import { CardFooter, Count, FormGrid } from "./kit";

/** The rates of a baseline's `default` tier, when the card has both. */
function headline(card: RateCard | null): { input: string; output: string } | null {
  const base = card?.default?.base;
  if (!base || base.input === undefined || base.output === undefined) return null;
  return { input: base.input, output: base.output };
}

/** A rate as the page shows it: `$4 / 百万 tokens`. The baseline is USD: it is what the rate multiplies. */
export function usdPerMillion(rate: string): string {
  return `$${rate} / 百万 tokens`;
}

/**
 * Every live group's price for one model, in ¥ per million tokens, from
 * `GET /admin/pricing/models`: the current baseline times the group's rate
 * in force, before any tenant discount. A group with no rate in force, or a
 * model with no baseline, shows which one is missing rather than a figure.
 */
function GroupMatrix({ row }: { row: ModelGroupPriceRow | null }) {
  if (!row) return <p className="hint">正在读取各分组的价格…</p>;
  if (!row.baseline.current) {
    return <p className="hint">这个模型还没有现行基准价，所以哪个分组都算不出售价。</p>;
  }
  if (row.groups.length === 0) return <p className="hint">还没有分组，所以还没有分组售价。</p>;
  return (
    <DataTable label="各分组售价" compact>
      <thead>
        <tr>
          <th>分组</th>
          <th className="num">输入（元/百万）</th>
          <th className="num">输出（元/百万）</th>
        </tr>
      </thead>
      <tbody>
        {row.groups.map((g) => (
          <tr key={g.group_id} data-state={g.input_per_million_cny === undefined ? "unpriced" : "priced"}>
            <td>{g.display_name}</td>
            <td className="num tabular">
              {g.input_per_million_cny === undefined ? (
                <span className="cell-note">分组未设换算率</span>
              ) : (
                fmtCnyPerMillion(g.input_per_million_cny)
              )}
            </td>
            <td className="num tabular">
              {g.output_per_million_cny === undefined ? (
                <span className="cell-note">分组未设换算率</span>
              ) : (
                fmtCnyPerMillion(g.output_per_million_cny)
              )}
            </td>
          </tr>
        ))}
      </tbody>
    </DataTable>
  );
}

/** A revision's source as typed, or null for one this build cannot read. */
function typedSource(r: BaselineRevision): BaselineSource | null {
  const source = r.source_kind;
  if (source !== "catalogue" && source !== "third_party" && source !== "manual") return null;
  return { source_kind: source, source: r.source } as BaselineSource;
}

/**
 * Where a baseline's card came from: the catalogue (with the page it was read
 * from and the reading date), an attested third-party dataset, or an
 * operator's own entry with its note. The same badge everywhere a baseline is
 * shown.
 */
export function BaselineSourceBadge({ revision }: { revision: BaselineRevision }) {
  const source = typedSource(revision);
  const who = revision.created_by?.email;
  if (!source) return <span className="tag">来源未知</span>;
  switch (source.source_kind) {
    case "catalogue": {
      const s = source.source;
      const href = safeHttpUrl(s.source_url);
      return (
        <div className="baseline-source">
          <span className="tag">官方目录</span>
          <span className="cell-note">
            {href ? (
              <a href={href} target="_blank" rel="noopener noreferrer">
                来源页
              </a>
            ) : (
              // Not an http(s) page: the text stays readable, and never as a link.
              <span className="trunc" title={s.source_url}>
                {s.source_url}
              </span>
            )}
            {" · "}
            <span className="tabular">抄录于 {s.as_of}</span>
          </span>
        </div>
      );
    }
    case "third_party": {
      // The provider page the dataset cites: what the attestation was checked
      // against, so it is shown beside the date. A dataset is third-party text,
      // so the URL is linked only when it is an http(s) page; anything else stays
      // plain text, never an href.
      const s = source.source;
      const href = safeHttpUrl(s.pricing_url);
      return (
        <div className="baseline-source">
          <span className="tag">第三方数据</span>
          <span className="cell-note">
            {s.dataset} · <span className="tabular">{s.dataset_ref}</span>
            {s.library_id && <> · 价格库同步</>}
          </span>
          <span className="cell-note">
            <span className="tabular">核对日期 {s.prices_checked ?? "未提供"}</span>
            {s.pricing_url && (
              <>
                {" · "}
                {href ? (
                  <a href={href} target="_blank" rel="noopener noreferrer">
                    核对页面
                  </a>
                ) : (
                  <span className="trunc" title={s.pricing_url}>
                    {s.pricing_url}
                  </span>
                )}
              </>
            )}
          </span>
          <span className="cell-note">
            {who && <>确认人 {who}</>}
            {s.attested_at && (
              <>
                {who && " · "}
                <span className="tabular">确认于 {fmtBeijing(s.attested_at)}</span>
              </>
            )}
          </span>
        </div>
      );
    }
    case "manual":
      return (
        <div className="baseline-source">
          <span className="tag">手动录入</span>
          {who && <span className="cell-note">填写人 {who}</span>}
          <span className="cell-note trunc" title={source.source.note}>
            {source.source.note}
          </span>
        </div>
      );
  }
}

/** 每个版本的来源与生效时间独立呈现，完整价格卡保留整行宽度。 */
function BaselineVersion({ revision }: { revision: BaselineRevision }) {
  const rates = headline(revision.card);
  return (
    <article className="baseline-version" aria-label={`基准价版本 ${revision.revision_id}`}>
      <p className="baseline-version-id">版本 <span className="tabular">#{revision.revision_id}</span></p>
      <dl className="baseline-version-meta">
        <div>
          <dt>生效时间（北京时间）</dt>
          <dd className="tabular"><time dateTime={revision.effective_from}>{fmtBeijing(revision.effective_from)}</time></dd>
        </div>
        <div>
          <dt>价格来源</dt>
          <dd><BaselineSourceBadge revision={revision} /></dd>
        </div>
      </dl>
      {rates && (
        <div className="baseline-version-summary">
          <span>标准档摘要 · 输入 / 输出</span>
          <span className="baseline-rates money tabular-nums">
            {usdPerMillion(rates.input)}{" · "}{usdPerMillion(rates.output)}
          </span>
        </div>
      )}
      <PriceCardDetails card={revision.card} label="完整价格卡" />
    </article>
  );
}

/** The model document fields the page reads, to preselect its catalogue entry. */
interface ModelDoc {
  provider?: string;
  model_name?: string;
}

/** 价格 · 基准价: each model's official USD list price, and where it came from. */
export function Baselines({ canEdit, onGoto }: TabProps) {
  const [rows, setRows] = useState<BaselineRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [catalogue, setCatalogue] = useState<CatalogueEntry[]>([]);
  const [models, setModels] = useState<ResourceView<ModelDoc>[]>([]);
  const [prices, setPrices] = useState<ModelGroupPriceRow[]>([]);
  const [status, setStatus] = useState("");
  const records = useRecordTable({
    items: (rows ?? []).filter((row) => status === "" || (status === "priced" ? row.current !== null : status === "unpriced" ? row.current === null : row.catalogue_update !== null)),
    searchText: (row) => [row.name, row.model_id],
    resetKey: status,
  });

  const load = useCallback(async () => {
    try {
      setRows(await baselines.list());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
    // The matrices are the same models: a failed read leaves them as they were.
    groupPrices.list().then(setPrices, () => undefined);
  }, []);

  useEffect(() => {
    void load();
    // The catalogue and the models only preselect an entry; the page works without them.
    pricing.catalogue().then((c) => setCatalogue(c.items), () => undefined);
    resources.list<ModelDoc>("models").then(setModels, () => undefined);
  }, [load]);

  const detail = rows?.find((r) => r.model_id === selected) ?? null;
  const detailPrice = prices.find((p) => p.model_id === detail?.model_id) ?? null;
  // 分母只取完整基准价清单，不把尚未读取的数据当成零。
  const priced = rows?.filter((r) => r.current !== null).length ?? 0;
  const unpriced = rows?.filter((r) => r.current === null) ?? [];
  const upcoming = rows?.reduce((n, r) => n + r.upcoming.length, 0) ?? 0;
  const changed = rows?.filter((r) => r.catalogue_update !== null).length ?? 0;
  const sources = [
    { kind: "catalogue", label: "官方目录" },
    { kind: "third_party", label: "第三方数据" },
    { kind: "manual", label: "手动录入" },
  ];
  const unknownSources = rows?.filter((r) => r.current && !sources.some((s) => s.kind === r.current?.source_kind)).length ?? 0;

  return (
    <>
      <PageHeader
        title="基准价"
        eyebrow="PRICING · BASELINES"
        description="每个模型的官方美元标价。租户按它乘以密钥所在分组的换算率，得出以人民币计的售价；没有基准价的模型排在最前面：它们无法计费。"
        dataLead={rows === null
          ? error ? "基准价读取失败，请重试。" : "正在读取模型的现行基准价与待生效版本。"
          : rows.length === 0
            ? "尚无可定价的模型。创建模型后，在这里设置基准价与生效时间。"
            : <>共 {rows.length.toLocaleString("zh-CN")} 个模型，{priced.toLocaleString("zh-CN")} 个已有现行基准价；{unpriced.length > 0 ? `${unpriced.length.toLocaleString("zh-CN")} 个未定价模型需要处理。` : "所有模型均已有基准价。"}</>}
        actions={
          <>
            <Button type="button" onClick={() => onGoto("pricing", "imports")}>
              去「导入审核」确认第三方数据
            </Button>
            <Button type="button" icon="refresh" onClick={() => void load()}>
              刷新
            </Button>
          </>
        }
      />
      {error && (
        <div className="note crit mt-0" role="alert">
          {error}{" "}
          <Button type="button" onClick={() => void load()}>
            重试
          </Button>
        </div>
      )}
      <BentoGrid className="mb-3" aria-label="基准价概况">
        <Card className={span.d7}>
          <CardHeader number={1} title="现行基准价覆盖 · 全部模型" />
          {rows === null ? (error ? <CardMessage onRetry={() => void load()}>基准价读取失败</CardMessage> : <MetricSkeleton />) : (
            <>
              <div className="baseline-coverage-value">
                <Metric size="hero">{priced.toLocaleString("zh-CN")}</Metric>
                <span className="font-mono text-meta text-muted">/ {rows.length.toLocaleString("zh-CN")} 个模型</span>
              </div>
              <div className="baseline-coverage-track" role="img" aria-label={`全部 ${rows.length} 个模型中，${priced} 个已有现行基准价，${unpriced.length} 个未定价`}>
                <span style={{ width: `${rows.length === 0 ? 0 : priced / rows.length * 100}%` }} />
              </div>
              <dl className="baseline-source-counts">
                {sources.map(({ kind, label }) => (
                  <div key={kind}>
                    <dt>{label}</dt>
                    <dd>{rows.filter((r) => r.current?.source_kind === kind).length.toLocaleString("zh-CN")}</dd>
                  </div>
                ))}
              </dl>
              {unknownSources > 0 && <p className="mt-3 text-caption text-muted">另有 {unknownSources.toLocaleString("zh-CN")} 个现行版本来源未知，请核对来源。</p>}
              <p className="mt-3 text-caption text-muted">来源统计仅含现行版本；完整价格卡、其他档位及长上下文价格在模型详情中查看。</p>
            </>
          )}
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="待处理 · 定价缺口" />
          {rows === null ? (error ? <CardMessage>定价缺口暂不可用</CardMessage> : <MetricSkeleton />) : (
            <>
              <Metric size="display">{unpriced.length.toLocaleString("zh-CN")}</Metric>
              <p className="mt-3 text-body text-on-inverse-muted">未定价模型无法计费。先补充基准价，再核对分组换算率。</p>
              <dl className="baseline-next-counts">
                <div><dt>待生效版本</dt><dd>{upcoming.toLocaleString("zh-CN")}</dd></div>
                <div><dt>目录更新待确认</dt><dd>{changed.toLocaleString("zh-CN")}</dd></div>
              </dl>
              {unpriced[0] && <CardFooter><Button type="button" onClick={() => setSelected(unpriced[0]!.model_id)}>查看首个未定价模型</Button></CardFooter>}
            </>
          )}
        </Card>
      </BentoGrid>
      <div className={detail ? "with-side baseline-workspace is-open" : "with-side baseline-workspace"}>
        <div className="side-main min-w-0">
          <Card className="mb-0">
            <CardHeader number={3} title={<>模型{rows && <Count n={rows.length} />}</>} description="未定价模型优先。表格展示标准档输入与输出；选择模型查看所有价格维度、档位、来源与待生效版本。" />
            {rows === null ? (
              !error && <Loading />
            ) : rows.length === 0 ? (
              <EmptyState text="还没有模型。先在「价格 → 公共模型」中新建公共模型，或在「上游账号 → 模型」中获取上游模型。" />
            ) : (
              <>
              <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索基准价模型", placeholder: "模型名称或 ID" }}>
                <label className="f filter"><span>定价状态</span><select value={status} onChange={(e) => setStatus(e.target.value)}><option value="">全部</option><option value="priced">已定价</option><option value="unpriced">未定价</option><option value="changed">目录有更新</option></select></label>
              </RecordTableToolbar>
              <DataTable label="基准价">
                <thead>
                  <tr>
                    <th>模型</th>
                    <th className="num">输入</th>
                    <th className="num">输出</th>
                    <th>来源</th>
                    <th>生效时间（北京时间）</th>
                    <th className="num">待生效</th>
                  </tr>
                </thead>
                <tbody>
                  {records.rows.length === 0 && <tr><td colSpan={6}><EmptyState text="没有符合筛选条件的模型" /></td></tr>}
                  {records.rows.map((r) => {
                    const rates = headline(r.current?.card ?? null);
                    return (
                      <tr
                        key={r.model_id}
                        data-state={r.current ? "priced" : "unpriced"}
                        className={selected === r.model_id ? "bg-well" : undefined}
                      >
                        <td>
                          <button
                            type="button"
                            className="link-button"
                            title={r.name}
                            aria-expanded={selected === r.model_id}
                            onClick={() => setSelected(r.model_id)}
                          >
                            {r.name}
                          </button>
                          {r.catalogue_update && (
                            <span className="cell-note baseline-update">{catalogueUpdateWords(r.catalogue_update).row}</span>
                          )}
                        </td>
                        <td className="num">
                          {!r.current ? (
                            <span className="baseline-unpriced">未定价</span>
                          ) : rates ? (
                            <span className="tabular">{usdPerMillion(rates.input)}</span>
                          ) : (
                            "—"
                          )}
                        </td>
                        <td className="num">
                          {!r.current ? "—" : rates ? <span className="tabular">{usdPerMillion(rates.output)}</span> : "—"}
                        </td>
                        <td>{r.current ? <BaselineSourceBadge revision={r.current} /> : "—"}</td>
                        <td className="tabular">
                          {r.current ? fmtBeijing(r.current.effective_from) : "—"}
                          {r.upcoming.length > 0 && (
                            <span className="cell-note">
                              待生效 {r.upcoming.map((u) => fmtBeijing(u.effective_from)).join("、")}
                            </span>
                          )}
                        </td>
                        <td className="num">{r.upcoming.length}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </DataTable>
              <RecordTablePager page={records.page} pageSize={records.pageSize} onPageChange={records.setPage} onPageSizeChange={records.setPageSize} total={records.filteredCount} label="基准价分页" />
              </>
            )}
          </Card>
        </div>
        {detail && (
          <BaselinePanel
            key={detail.model_id}
            row={detail}
            canEdit={canEdit}
            catalogue={catalogue}
            doc={models.find((m) => m.id === detail.model_id)?.doc ?? null}
            prices={detailPrice}
            onClose={() => setSelected(null)}
            onSaved={load}
          />
        )}
      </div>
    </>
  );
}

function BaselinePanel({
  row,
  canEdit,
  catalogue,
  doc,
  prices,
  onClose,
  onSaved,
}: {
  row: BaselineRow;
  canEdit: boolean;
  catalogue: CatalogueEntry[];
  doc: ModelDoc | null;
  /** This model's price in every live group; `null` until the list arrives. */
  prices: ModelGroupPriceRow | null;
  onClose: () => void;
  onSaved: () => Promise<void>;
}) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [mode, setMode] = useState<"catalogue" | "manual">("catalogue");
  // 首次进入手动录入后保留编辑器，切换目录不销毁草稿与错误。
  const [manualVisited, setManualVisited] = useState(false);
  const [card, setCard] = useState<RateCard | null>(row.current?.card ?? null);
  const [cardOk, setCardOk] = useState(true);
  const [note, setNote] = useState("");
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });
  const [entry, setEntry] = useState(row.catalogue_update?.entry_key ?? "");
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<PriceWarning[]>([]);
  const [later, setLater] = useState<BaselineRevision[]>([]);

  const keys = useMemo(() => [...new Set(catalogue.map((e) => e.key))], [catalogue]);
  // The model's own provider/model_name, when the catalogue knows it.
  const own = useMemo(() => {
    if (!doc?.provider || !doc.model_name) return "";
    const name = `${doc.provider}/${doc.model_name}`;
    return catalogue.find((e) => e.key === name || e.aliases.includes(name))?.key ?? "";
  }, [catalogue, doc]);
  const chosen = entry || own;

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setFailure(null);
    setWarnings([]);
    setLater([]);
    const refused: Detail[] = [];
    if (mode === "manual") {
      if (!cardOk) refused.push({ path: "/card", message: "价格卡中有无法读取的内容，修改尚未生效；请先改正" });
    } else if (!chosen) {
      refused.push({ path: "/catalogue_key", message: "请选择目录条目" });
    }
    if (refused.length > 0) {
      setDetails(refused);
      focusRefusal();
      return;
    }
    setDetails([]);
    setSaving(true);
    try {
      // A note belongs to a manual card only: the backend refuses one beside a catalogue key.
      const body =
        mode === "catalogue" ? { catalogue_key: chosen } : { card: card ?? {}, note };
      const created = await sendAt(resolver.current, (effective_from) =>
        baselines.create({ model_id: row.model_id, effective_from, ...body }),
      );
      if (created === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按北京时间计" }]);
        focusRefusal();
        return;
      }
      setWarnings(created.warnings);
      setLater(created.later_revisions);
      toast("已保存");
      await onSaved();
    } catch (err) {
      if (err instanceof ApiError && err.status === 422 && err.details.length > 0) {
        setDetails(err.details);
        focusRefusal();
      } else {
        setFailure(err instanceof ApiError ? messageFor(err) : errorText(err));
      }
    } finally {
      setSaving(false);
    }
  }

  const strayDetails = details.filter(
    (d) =>
      !(
        d.path.startsWith("/card") ||
        d.path === "/effective_from" ||
        d.path === "/catalogue_key" ||
        d.path === "/note"
      ),
  );

  return (
    <SidePanel title={row.name} label={`${row.name} 的基准价`} onClose={onClose}>
      {canEdit && (
        <form ref={form} className="baseline-form mt-5" aria-label="新基准价" onSubmit={(e) => void save(e)} noValidate>
          <h3 className="mb-4 font-mono text-meta text-muted">新基准价</h3>
          <FormGrid>
            <Field label="方式">
              {(aria) => (
                <select
                  {...aria}
                  value={mode}
                  onChange={(e) => {
                    const next = e.target.value as "catalogue" | "manual";
                    if (next === "manual") setManualVisited(true);
                    setMode(next);
                  }}
                >
                  <option value="catalogue">从官方目录</option>
                  <option value="manual">手动录入</option>
                </select>
              )}
            </Field>
            {mode === "catalogue" && (
              <Field label="目录条目" error={problemAt(details, (p) => p === "/catalogue_key")}>
                {(aria) => (
                  <select {...aria} value={chosen} onChange={(e) => setEntry(e.target.value)}>
                    <option value="">请选择</option>
                    {keys.map((k) => (
                      <option key={k} value={k}>
                        {k}
                      </option>
                    ))}
                  </select>
                )}
              </Field>
            )}
          </FormGrid>
          {manualVisited && (
            <fieldset hidden={mode !== "manual"} disabled={mode !== "manual"} className="m-0 min-w-0 border-0 p-0">
              <div className="mt-4 grid gap-4">
                <RateCardEditor value={card} onChange={setCard} details={details} onValidity={setCardOk} />
                <Field label="说明" hint="写明这份价格是从哪里抄来的，最多 500 字" error={problemAt(details, (p) => p === "/note")}>
                  {(aria) => (
                    <textarea {...aria} rows={3} autoComplete="off" value={note} onChange={(e) => setNote(e.target.value)} />
                  )}
                </Field>
              </div>
            </fieldset>
          )}
          <div className="mt-4 flex flex-col gap-3 [&_.field]:mb-0 [&_label.check]:mb-0">
            <EffectiveFrom
              onResolve={(r) => {
                resolver.current = r;
              }}
              error={problemAt(details, (p) => p === "/effective_from")}
            />
          </div>
          {strayDetails.length > 0 && (
            <div className="note crit" role="alert">
              {strayDetails.map((d) => d.message).join("；")}
            </div>
          )}
          {failure && (
            <div className="note crit" role="alert">
              {failure}
            </div>
          )}
          {later.length > 0 && (
            <div className="note" role="status">
              目录中该条目之后的 {later.length} 个价格也已写入：
              {later.map((r) => fmtBeijing(r.effective_from)).join("、")}
            </div>
          )}
          {warnings.length > 0 && (
            <div className="note warn" role="status">
              <ul className="baseline-warnings">
                {warnings.map((w, i) => (
                  <li key={i}>{warningText(w.code, w.message)}</li>
                ))}
              </ul>
            </div>
          )}
          <CardFooter>
            <Button type="submit" kind="primary" busy={saving} busyLabel="保存中…">
              保存
            </Button>
          </CardFooter>
        </form>
      )}
      <section className={canEdit ? "mt-6 border-t border-ink/7 pt-6" : "mt-5"} aria-label="现行基准价">
        <h3 className="mb-3 font-mono text-meta text-muted">现行与待生效</h3>
        <div className="baseline-version-group">
          <div className="baseline-group-heading">
            <h4>现行基准价</h4>
            <span>当前计费依据</span>
          </div>
          {row.current ? <BaselineVersion revision={row.current} /> : <p className="baseline-unpriced">未定价</p>}
        </div>
        <section className="baseline-version-group" aria-label="待生效的基准价">
          <div className="baseline-group-heading">
            <h4>待生效版本<Count n={row.upcoming.length} /></h4>
            <span>按预约时间生效</span>
          </div>
          {row.upcoming.length > 0 ? (
            <div className="baseline-version-list">
              {row.upcoming.map((revision) => <BaselineVersion key={revision.revision_id} revision={revision} />)}
            </div>
          ) : <p className="hint">暂无待生效版本</p>}
        </section>
        {row.catalogue_update && (
          <div className="note warn" role="status">
            {catalogueUpdateWords(row.catalogue_update).panel}
          </div>
        )}
      </section>
      <LibrarySyncOne
        className="mt-6 border-t border-ink/7 pt-6"
        modelId={row.model_id}
        canEdit={canEdit}
        current={row.current}
        onSynced={onSaved}
      />
      <section className="mt-6 border-t border-ink/7 pt-6" aria-label="各分组售价">
        <h3 className="mb-1 font-mono text-meta text-muted">各分组售价</h3>
        <p className="hint text-caption">现行基准价乘以每个分组现行的换算率，按每百万 token 计；这是租户折扣之前的价格。</p>
        <GroupMatrix row={prices} />
      </section>
    </SidePanel>
  );
}
