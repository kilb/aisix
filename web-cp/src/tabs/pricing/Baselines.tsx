import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import { RateCardEditor } from "../../components/RateCardEditor";
import {
  ApiError,
  baselines,
  pricing,
  resources,
  type BaselineRevision,
  type BaselineRow,
  type BaselineSource,
  type CatalogueEntry,
  type Detail,
  type PriceWarning,
  type RateCard,
  type ResourceView,
} from "../../lib/api";
import { messageFor, problemAt, warningText } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { catalogueUpdateWords } from "../../lib/baselineUpdate";
import { fmtUtc } from "../../lib/money";
import { safeHttpUrl } from "../../lib/safeUrl";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { SidePanel } from "../../ui/SidePanel";
import { useToast } from "../../ui/Toast";
import "./Baselines.css";

/** The rates of a baseline's `default` tier, when the card has both. */
function headline(card: RateCard | null): { input: string; output: string } | null {
  const base = card?.default?.base;
  if (!base || base.input === undefined || base.output === undefined) return null;
  return { input: base.input, output: base.output };
}

/** A rate as the page shows it: `$4 / 百万 tokens`. */
export function usdPerMillion(rate: string): string {
  return `$${rate} / 百万 tokens`;
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
                <span className="tabular">确认于 {fmtUtc(s.attested_at)}</span>
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

  const load = useCallback(async () => {
    try {
      setRows(await baselines.list());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void load();
    // The catalogue and the models only preselect an entry; the page works without them.
    pricing.catalogue().then((c) => setCatalogue(c.items), () => undefined);
    resources.list<ModelDoc>("models").then(setModels, () => undefined);
  }, [load]);

  const detail = rows?.find((r) => r.model_id === selected) ?? null;

  return (
    <div className={detail ? "with-side is-open" : "with-side"}>
      <div className="side-main">
        <div className="panel">
          <h2>
            基准价
            <Button size="sm" type="button" icon="refresh" onClick={() => void load()}>
              刷新
            </Button>
          </h2>
          <p className="hint">
            每个模型的官方美元标价。租户按它乘以所在分组的换算率计价。没有基准价的模型排在最前面：它们无法计费。
          </p>
          <p>
            <Button type="button" size="sm" onClick={() => onGoto("pricing", "imports")}>
              去「导入审核」确认第三方数据
            </Button>
          </p>
          {error && (
            <div className="note crit" role="alert">
              {error}{" "}
              <Button type="button" onClick={() => void load()}>
                重试
              </Button>
            </div>
          )}
          {rows === null ? (
            !error && <Loading />
          ) : rows.length === 0 ? (
            <EmptyState text="还没有模型。先在「模型与路由」中创建模型。" />
          ) : (
            <DataTable label="基准价">
              <thead>
                <tr>
                  <th>模型</th>
                  <th className="num">输入</th>
                  <th className="num">输出</th>
                  <th>来源</th>
                  <th>生效时间（UTC）</th>
                  <th className="num">待生效</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => {
                  const rates = headline(r.current?.card ?? null);
                  return (
                    <tr key={r.model_id} data-state={r.current ? "priced" : "unpriced"}>
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
                        {r.current ? fmtUtc(r.current.effective_from) : "—"}
                        {r.upcoming.length > 0 && (
                          <span className="cell-note">
                            待生效 {r.upcoming.map((u) => fmtUtc(u.effective_from)).join("、")}
                          </span>
                        )}
                      </td>
                      <td className="num">{r.upcoming.length}</td>
                    </tr>
                  );
                })}
              </tbody>
            </DataTable>
          )}
        </div>
      </div>
      {detail && (
        <BaselinePanel
          key={detail.model_id}
          row={detail}
          canEdit={canEdit}
          catalogue={catalogue}
          doc={models.find((m) => m.id === detail.model_id)?.doc ?? null}
          onClose={() => setSelected(null)}
          onSaved={load}
        />
      )}
    </div>
  );
}

function BaselinePanel({
  row,
  canEdit,
  catalogue,
  doc,
  onClose,
  onSaved,
}: {
  row: BaselineRow;
  canEdit: boolean;
  catalogue: CatalogueEntry[];
  doc: ModelDoc | null;
  onClose: () => void;
  onSaved: () => Promise<void>;
}) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [mode, setMode] = useState<"catalogue" | "manual">("catalogue");
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
      if (!cardOk) refused.push({ path: "/card", message: "价目卡 JSON 不合法，修改尚未生效；请先改正" });
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
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC 计" }]);
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
        <form ref={form} className="baseline-form" aria-label="新基准价" onSubmit={(e) => void save(e)} noValidate>
          <h3>新基准价</h3>
          <Field label="方式">
            {(aria) => (
              <select {...aria} value={mode} onChange={(e) => setMode(e.target.value as "catalogue" | "manual")}>
                <option value="catalogue">从官方目录</option>
                <option value="manual">手动录入</option>
              </select>
            )}
          </Field>
          {mode === "catalogue" ? (
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
          ) : (
            <>
              <RateCardEditor value={card} onChange={setCard} details={details} onValidity={setCardOk} />
              <Field label="说明" hint="写明这份价格是从哪里抄来的，最多 500 字" error={problemAt(details, (p) => p === "/note")}>
                {(aria) => (
                  <textarea {...aria} rows={3} autoComplete="off" value={note} onChange={(e) => setNote(e.target.value)} />
                )}
              </Field>
            </>
          )}
          <EffectiveFrom
            onResolve={(r) => {
              resolver.current = r;
            }}
            error={problemAt(details, (p) => p === "/effective_from")}
          />
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
              {later.map((r) => fmtUtc(r.effective_from)).join("、")}
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
          <p>
            <Button type="submit" kind="primary" busy={saving} busyLabel="保存中…">
              保存
            </Button>
          </p>
        </form>
      )}
      <section className="baseline-now" aria-label="现行基准价">
        <h3>现行与待生效</h3>
        {row.current ? (
          <div className="baseline-revision">
            <div className="baseline-rates money">
              {headline(row.current.card) ? (
                <>
                  {usdPerMillion(headline(row.current.card)!.input)}
                  {" · "}
                  {usdPerMillion(headline(row.current.card)!.output)}
                </>
              ) : (
                "价目卡无法读取"
              )}
            </div>
            <p className="hint tabular">生效于 {fmtUtc(row.current.effective_from)}</p>
            <BaselineSourceBadge revision={row.current} />
          </div>
        ) : (
          <p className="baseline-unpriced">未定价</p>
        )}
        {row.upcoming.length > 0 && (
          <DataTable label="待生效的基准价" compact>
            <thead>
              <tr>
                <th>生效时间（UTC）</th>
                <th>输入</th>
                <th>输出</th>
                <th>来源</th>
              </tr>
            </thead>
            <tbody>
              {row.upcoming.map((u) => {
                const rates = headline(u.card);
                return (
                  <tr key={u.revision_id}>
                    <td className="tabular">{fmtUtc(u.effective_from)}</td>
                    <td className="tabular">{rates ? usdPerMillion(rates.input) : "—"}</td>
                    <td className="tabular">{rates ? usdPerMillion(rates.output) : "—"}</td>
                    <td>
                      <BaselineSourceBadge revision={u} />
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </DataTable>
        )}
        {row.catalogue_update && (
          <div className="note warn" role="status">
            {catalogueUpdateWords(row.catalogue_update).panel}
          </div>
        )}
      </section>
    </SidePanel>
  );
}
