import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import { RateCardEditor } from "../../components/RateCardEditor";
import {
  ApiError,
  pricing,
  resources,
  type CatalogueEntry,
  type Detail,
  type PriceRow,
  type PriceWarning,
  type RateCard,
  type ResourceView,
  type SellRevision,
} from "../../lib/api";
import { messageFor, problemAt, warningText } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtUtc } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { SidePanel } from "../../ui/SidePanel";
import { useToast } from "../../ui/Toast";
import "./SellPrices.css";

const HISTORY_PAGE = 20;

/** The default tier's input and output rates of a card, when it has them. */
function headline(card: RateCard | null): { input: string; output: string } | null {
  const base = card?.default?.base;
  if (!base || base.input === undefined || base.output === undefined) return null;
  return { input: base.input, output: base.output };
}

/** `加价（%）` typed with at most 2 decimals, as basis points; null when it is not such a number. */
export function markupBps(text: string): number | null {
  const m = /^(\d{1,5})(?:\.(\d{1,2}))?$/.exec(text.trim());
  if (!m) return null;
  return Number(m[1]) * 100 + Number((m[2] ?? "").padEnd(2, "0"));
}

/** The model document fields this page reads, to find its catalogue entry. */
interface ModelDoc {
  provider?: string;
  model_name?: string;
}

export function SellPrices({ canEdit }: TabProps) {
  const [rows, setRows] = useState<PriceRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [catalogue, setCatalogue] = useState<CatalogueEntry[]>([]);
  const [models, setModels] = useState<ResourceView<ModelDoc>[]>([]);

  const load = useCallback(async () => {
    try {
      setRows(await pricing.models());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void load();
    // The catalogue and the models only pre-select an entry; the page works without them.
    pricing.catalogue().then((c) => setCatalogue(c.items), () => undefined);
    resources.list<ModelDoc>("models").then(setModels, () => undefined);
  }, [load]);

  const detail = rows?.find((r) => r.model_id === selected) ?? null;

  return (
    <div className={detail ? "with-side is-open" : "with-side"}>
      <div className="side-main">
        <div className="panel">
          <h2>
            平台价格
            <Button size="sm" type="button" icon="refresh" onClick={() => void load()}>
              刷新
            </Button>
          </h2>
          <p className="hint">没有定价的模型排在最前面：它们的调用无法计费。价格只能从现在或将来开始生效，历史价格不会被改写。</p>
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
            <DataTable label="平台价格">
              <thead>
                <tr>
                  <th>模型</th>
                  <th>现行价格（美元/百万单位）</th>
                  <th>版本</th>
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
                      </td>
                      <td>
                        {!r.current ? (
                          <span className="price-unpriced">未定价</span>
                        ) : rates ? (
                          <span className="tabular">
                            输入 {rates.input} 输出 {rates.output}
                          </span>
                        ) : (
                          "—"
                        )}
                      </td>
                      <td className="tabular">{r.current ? `#${r.current.revision_id}` : "—"}</td>
                      <td className="tabular">{r.current ? fmtUtc(r.current.effective_from) : "—"}</td>
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
        <ModelPrice
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

function ModelPrice({
  row,
  canEdit,
  catalogue,
  doc,
  onClose,
  onSaved,
}: {
  row: PriceRow;
  canEdit: boolean;
  catalogue: CatalogueEntry[];
  doc: ModelDoc | null;
  onClose: () => void;
  onSaved: () => Promise<void>;
}) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [history, setHistory] = useState<SellRevision[] | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);

  const [mode, setMode] = useState<"card" | "import">("card");
  const [card, setCard] = useState<RateCard | null>(row.current?.card ?? null);
  const [cardOk, setCardOk] = useState(true);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });
  const [entry, setEntry] = useState("");
  const [markup, setMarkup] = useState("0");
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<PriceWarning[]>([]);

  const keys = useMemo(() => [...new Set(catalogue.map((e) => e.key))], [catalogue]);
  // The model's own provider/model_name, when the catalogue knows it.
  const own = useMemo(() => {
    if (!doc?.provider || !doc.model_name) return "";
    const name = `${doc.provider}/${doc.model_name}`;
    return catalogue.find((e) => e.key === name || e.aliases.includes(name))?.key ?? "";
  }, [catalogue, doc]);
  const chosen = entry || own;

  const loadHistory = useCallback(
    async (before?: number) => {
      try {
        const page = await pricing.listSell({ model_id: row.model_id, limit: HISTORY_PAGE, ...(before ? { before } : {}) });
        setHistory((h) => (before && h ? [...h, ...page.items] : page.items));
        setNext(page.next_before);
        setHistoryError(null);
      } catch (e) {
        setHistoryError(errorText(e));
      }
    },
    [row.model_id],
  );
  useEffect(() => {
    void loadHistory();
  }, [loadHistory]);

  async function more() {
    if (next === null) return;
    setLoadingMore(true);
    await loadHistory(next);
    setLoadingMore(false);
  }

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setFailure(null);
    setWarnings([]);
    const refused: Detail[] = [];
    let bps: number | undefined;
    if (mode === "card") {
      if (!cardOk) refused.push({ path: "/card", message: "价格卡 JSON 不合法，修改尚未生效；请先改正" });
    } else {
      bps = markupBps(markup) ?? undefined;
      if (bps === undefined) refused.push({ path: "/markup_bps", message: "加价应是百分数，最多两位小数，例如 20 或 12.5" });
    }
    if (refused.length > 0) {
      setDetails(refused);
      focusRefusal();
      return;
    }
    setDetails([]);
    setSaving(true);
    try {
      const body =
        mode === "card"
          ? { card: card ?? {} }
          : { catalogue_key: chosen, markup_bps: bps ?? 0 };
      const created = await sendAt(resolver.current, (effective_from) =>
        pricing.createSell({ model_id: row.model_id, effective_from, ...body }),
      );
      if (created === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC 计" }]);
        focusRefusal();
        return;
      }
      setWarnings(created.warnings);
      toast("已保存");
      await Promise.all([onSaved(), loadHistory()]);
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
    (d) => !(d.path.startsWith("/card") || d.path === "/effective_from" || d.path === "/catalogue_key" || d.path === "/markup_bps"),
  );

  return (
    <SidePanel title={row.name} label={`${row.name} 的价格`} onClose={onClose}>
      {canEdit && (
        <form ref={form} className="sell-form" aria-label="新价格" onSubmit={(e) => void save(e)} noValidate>
          <h3>新价格</h3>
          <Field label="方式">
            <select value={mode} onChange={(e) => setMode(e.target.value as "card" | "import")}>
              <option value="card">按价格卡</option>
              <option value="import">从价格目录导入</option>
            </select>
          </Field>
          {mode === "card" ? (
            <RateCardEditor value={card} onChange={setCard} details={details} onValidity={setCardOk} />
          ) : (
            <>
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
              <Field label="加价（%）" error={problemAt(details, (p) => p === "/markup_bps")}>
                {(aria) => <input {...aria} inputMode="decimal" autoComplete="off" value={markup} onChange={(e) => setMarkup(e.target.value)} />}
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
          {warnings.length > 0 && (
            <div className="note warn" role="status">
              <ul className="sell-warnings">
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
      <section className="sell-history" aria-label="价格历史">
        <h3>价格历史</h3>
        {historyError && (
          <div className="note crit" role="alert">
            {historyError}
          </div>
        )}
        {history === null ? (
          !historyError && <Loading />
        ) : history.length === 0 ? (
          <EmptyState text="这个模型还没有价格记录。" />
        ) : (
          <DataTable label="价格历史" compact>
            <thead>
              <tr>
                <th>版本</th>
                <th>生效时间（UTC）</th>
                <th>输入</th>
                <th>输出</th>
                <th>来源</th>
              </tr>
            </thead>
            <tbody>
              {history.map((h) => {
                const rates = headline(h.card);
                return (
                  <tr key={h.revision_id}>
                    <td className="tabular">#{h.revision_id}</td>
                    <td className="tabular">{fmtUtc(h.effective_from)}</td>
                    <td className="tabular">{rates?.input ?? "—"}</td>
                    <td className="tabular">{rates?.output ?? "—"}</td>
                    <td>
                      <span className="trunc" title={h.source}>
                        {h.source}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </DataTable>
        )}
        {next !== null && (
          <p>
            <Button type="button" busy={loadingMore} busyLabel="载入中…" onClick={() => void more()}>
              更多
            </Button>
          </p>
        )}
      </section>
    </SidePanel>
  );
}
