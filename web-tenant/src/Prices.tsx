import { useCallback, useEffect, useState } from "react";
import { ApiError, prices as priceApi, type PricedGroup, type PricedModel } from "./lib/api";
import { messageFor } from "./lib/errors";
import { fmtCnyPerMillion } from "./lib/money";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { RouteNav } from "./ui/RouteNav";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/** One price cell: the API's own exact decimal string, or an em dash when the model carries no such rate. */
function perMillion(rate: string | undefined): string {
  return rate === undefined ? "—" : fmtCnyPerMillion(rate);
}

/**
 * The organisation's prices: one tab per group it is assigned, each listing
 * what that group charges every granted model it can price, in ¥ per million
 * tokens. Every figure is the API's own decimal string, already the price the
 * organisation pays — the group's rate and the organisation's discount are in
 * it, so the page applies neither again. A model the group cannot price is
 * absent rather than shown at 0, and a route name never reaches this page.
 */
export function Prices({ initialGroup }: { initialGroup: string | null }) {
  const [groups, setGroups] = useState<PricedGroup[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setGroups((await priceApi.list()).groups);
      setErr(null);
    } catch (e) {
      setErr(failure(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The group the address names, when the organisation is assigned it; else
  // the first one. A link to a group the organisation no longer has opens
  // the page rather than an empty tab.
  const chosen =
    groups === null
      ? null
      : (groups.find((g) => g.id === open) ?? groups.find((g) => g.id === initialGroup) ?? groups[0] ?? null);

  if (err !== null) {
    return (
      <div className="note crit" role="alert">
        {err}
      </div>
    );
  }
  if (groups === null) return <Loading />;
  if (chosen === null) {
    return <EmptyState text="运营尚未为本组织分配分组，暂时没有可查看的价格。" />;
  }

  return (
    <div className="panel">
      <h2>价格</h2>
      <RouteNav
        label="分组"
        items={groups.map((g) => ({ id: g.id, label: g.display_name }))}
        current={chosen.id}
        onSelect={setOpen}
      />
      <section id={`tab-${chosen.id}`} role="tabpanel" aria-labelledby={`stop-${chosen.id}`} className="stack">
        {chosen.description !== "" && <p className="hint">{chosen.description}</p>}
        <p className="hint">价格以人民币计，按实际用量计费</p>
        {chosen.models.length === 0 ? (
          <EmptyState text="该分组暂时没有可计价的模型。" mark="route" />
        ) : (
          <PriceTable models={chosen.models} />
        )}
      </section>
    </div>
  );
}

/**
 * A cache read or write price. A model with no cache rate of its own is
 * billed at its input rate for those tokens (the pricing engine's fallback,
 * `Rates::rate_for`), so the cell shows that price and says so, rather than
 * a dash that reads as "not charged" or "unknown".
 */
function CacheRate({ own, input }: { own: string | undefined; input: string }) {
  if (own !== undefined) return <>{perMillion(own)}</>;
  return (
    <>
      {perMillion(input)}
      <span className="hint cell-note">同输入价</span>
    </>
  );
}

function PriceTable({ models }: { models: PricedModel[] }) {
  return (
    <DataTable label="模型单价">
      <thead>
        <tr>
          <th>模型</th>
          <th className="num">输入</th>
          <th className="num">输出</th>
          <th className="num">缓存读</th>
          <th className="num">缓存写</th>
        </tr>
      </thead>
      <tbody>
        {models.map((m) => (
          <tr key={m.model_id}>
            <td>
              <span className="trunc" title={m.name}>
                {m.name}
              </span>
            </td>
            <td className="num">{perMillion(m.input_per_million)}</td>
            <td className="num">{perMillion(m.output_per_million)}</td>
            <td className="num">
              <CacheRate own={m.cache_read_per_million} input={m.input_per_million} />
            </td>
            <td className="num">
              <CacheRate own={m.cache_write_per_million} input={m.input_per_million} />
              {/* A one-hour cache write billed at its own rate says so beneath the default write's. */}
              {m.cache_write_1h_per_million !== undefined && (
                <span className="hint cell-note">1 小时缓存写 {perMillion(m.cache_write_1h_per_million)}</span>
              )}
            </td>
          </tr>
        ))}
      </tbody>
    </DataTable>
  );
}
