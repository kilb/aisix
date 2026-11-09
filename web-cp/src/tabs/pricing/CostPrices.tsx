import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import { RateCardEditor } from "../../components/RateCardEditor";
import {
  ApiError,
  costPricing,
  resources,
  type CostRevision,
  type Detail,
  type PriceWarning,
  type RateCard,
  type ResourceView,
} from "../../lib/api";
import { messageFor, problemAt, warningText } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtUtc } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { useToast } from "../../ui/Toast";
import "./PriceForms.css";
import "./CostPrices.css";

const PAGE = 20;

interface ModelDoc {
  provider?: string;
  model_name?: string;
}
interface KeyDoc {
  display_name?: string | null;
}

/** The default tier's input and output rates of a card, when it has them. */
function headline(card: RateCard | null): { input: string; output: string } | null {
  const base = card?.default?.base;
  if (!base || base.input === undefined || base.output === undefined) return null;
  return { input: base.input, output: base.output };
}

export function CostPrices({ canEdit }: TabProps) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [models, setModels] = useState<ResourceView<ModelDoc>[]>([]);
  const [keys, setKeys] = useState<ResourceView<KeyDoc>[]>([]);

  // Filters.
  const [fProvider, setFProvider] = useState("");
  const [fModel, setFModel] = useState("");
  const [fAccount, setFAccount] = useState("");

  const [history, setHistory] = useState<CostRevision[] | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  // Only the newest request may write the list, so a slow answer to an older filter cannot replace it.
  const seq = useRef(0);

  // New price.
  const [provider, setProvider] = useState("");
  const [model, setModel] = useState("");
  const [account, setAccount] = useState("");
  const [card, setCard] = useState<RateCard | null>(null);
  const [cardOk, setCardOk] = useState(true);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<PriceWarning[]>([]);

  useEffect(() => {
    // The datalists only suggest; the page works without them.
    resources.list<ModelDoc>("models").then(setModels, () => undefined);
    resources.list<KeyDoc>("provider_keys").then(setKeys, () => undefined);
  }, []);

  const load = useCallback(
    async (before?: number) => {
      const mine = ++seq.current;
      try {
        const page = await costPricing.listCost({
          upstream_provider: fProvider.trim(),
          upstream_model: fModel.trim(),
          provider_key_id: fAccount,
          limit: PAGE,
          ...(before ? { before } : {}),
        });
        if (mine !== seq.current) return;
        setHistory((h) => (before && h ? [...h, ...page.items] : page.items));
        setNext(page.next_before);
        setError(null);
      } catch (e) {
        if (mine === seq.current) setError(errorText(e));
      }
    },
    [fProvider, fModel, fAccount],
  );
  useEffect(() => {
    void load();
  }, [load]);

  async function more() {
    if (next === null) return;
    setLoadingMore(true);
    await load(next);
    setLoadingMore(false);
  }

  const providers = [...new Set(models.map((m) => m.doc.provider).filter((v): v is string => !!v))].sort();
  const upstreamModels = [...new Set(models.map((m) => m.doc.model_name).filter((v): v is string => !!v))].sort();
  const accountName = (id: string | null) =>
    id === null ? "全部" : (keys.find((k) => k.id === id)?.doc.display_name ?? id);

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setFailure(null);
    setWarnings([]);
    if (!cardOk) {
      setDetails([{ path: "/card", message: "价格卡 JSON 不合法，修改尚未生效；请先改正" }]);
      focusRefusal();
      return;
    }
    setDetails([]);
    setSaving(true);
    try {
      const created = await sendAt(resolver.current, (effective_from) =>
        costPricing.createCost({
          upstream_provider: provider,
          upstream_model: model,
          ...(account ? { provider_key_id: account } : {}),
          effective_from,
          card: card ?? {},
        }),
      );
      if (created === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC 计" }]);
        focusRefusal();
        return;
      }
      setWarnings(created.warnings);
      toast("已保存");
      await load();
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
    (d) => !(d.path.startsWith("/card") || ["/effective_from", "/upstream_provider", "/upstream_model"].includes(d.path)),
  );

  return (
    <div className="panel">
      <h2>
        成本价
        <Button size="sm" type="button" icon="refresh" onClick={() => void load()}>
          刷新
        </Button>
      </h2>
      <p className="hint">成本价是平台向上游支付的价格，只用于利润报表，不会出现在租户可见的任何位置。价格只能从现在或将来开始生效。</p>

      {canEdit && (
        <form ref={form} className="sell-form" aria-label="新成本价" onSubmit={(e) => void save(e)} noValidate>
          <h3>新成本价</h3>
          <Field label="上游" error={problemAt(details, (p) => p === "/upstream_provider")}>
            {(aria) => (
              <input {...aria} list="cost-providers" autoComplete="off" value={provider} onChange={(e) => setProvider(e.target.value)} />
            )}
          </Field>
          <Field label="上游模型" error={problemAt(details, (p) => p === "/upstream_model")}>
            {(aria) => (
              <input {...aria} list="cost-models" autoComplete="off" value={model} onChange={(e) => setModel(e.target.value)} />
            )}
          </Field>
          <Field label="上游账号" hint="选「全部」则这个上游模型经任何账号都按此价计成本。">
            <select value={account} onChange={(e) => setAccount(e.target.value)}>
              <option value="">全部</option>
              {keys.map((k) => (
                <option key={k.id} value={k.id}>
                  {k.doc.display_name ?? k.id}
                </option>
              ))}
            </select>
          </Field>
          <RateCardEditor value={card} onChange={setCard} details={details} onValidity={setCardOk} />
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

      <datalist id="cost-providers">
        {providers.map((v) => (
          <option key={v} value={v} />
        ))}
      </datalist>
      <datalist id="cost-models">
        {upstreamModels.map((v) => (
          <option key={v} value={v} />
        ))}
      </datalist>

      <section className="sell-history" aria-label="成本价历史">
        <h3>成本价历史</h3>
        <div role="search" aria-label="筛选成本价" className="sell-filters">
          <Field label="上游">
            <input list="cost-providers" autoComplete="off" value={fProvider} onChange={(e) => setFProvider(e.target.value)} />
          </Field>
          <Field label="上游模型">
            <input list="cost-models" autoComplete="off" value={fModel} onChange={(e) => setFModel(e.target.value)} />
          </Field>
          <Field label="上游账号">
            <select value={fAccount} onChange={(e) => setFAccount(e.target.value)}>
              <option value="">全部</option>
              {keys.map((k) => (
                <option key={k.id} value={k.id}>
                  {k.doc.display_name ?? k.id}
                </option>
              ))}
            </select>
          </Field>
        </div>
        {error && (
          <div className="note crit" role="alert">
            {error}{" "}
            <Button type="button" onClick={() => void load()}>
              重试
            </Button>
          </div>
        )}
        {history === null ? (
          !error && <Loading />
        ) : history.length === 0 ? (
          <EmptyState text="没有符合条件的成本价记录。" />
        ) : (
          <DataTable label="成本价历史" compact>
            <thead>
              <tr>
                <th>版本</th>
                <th>上游</th>
                <th>上游模型</th>
                <th>上游账号</th>
                <th>生效时间（UTC）</th>
                <th>输入（美元/百万单位）</th>
                <th>输出（美元/百万单位）</th>
              </tr>
            </thead>
            <tbody>
              {history.map((h) => {
                const rates = headline(h.card);
                return (
                  <tr key={h.revision_id}>
                    <td className="tabular">#{h.revision_id}</td>
                    <td>{h.upstream_provider}</td>
                    <td>
                      <span className="trunc" title={h.upstream_model}>
                        {h.upstream_model}
                      </span>
                    </td>
                    <td>{accountName(h.provider_key_id)}</td>
                    <td className="tabular">{fmtUtc(h.effective_from)}</td>
                    <td className="tabular">{rates?.input ?? "—"}</td>
                    <td className="tabular">{rates?.output ?? "—"}</td>
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
    </div>
  );
}
