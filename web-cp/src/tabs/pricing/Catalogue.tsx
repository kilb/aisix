import { Fragment, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { JsonEditor } from "../../components/JsonEditor";
import { pricing, type CatalogueEntry } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtBeijing } from "../../lib/time";
import { TIER_HINT } from "../../lib/rateUnits";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useRecordTable } from "../../lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { Card, CardHeader, PageHeader } from "../../components/page";
import { CardLines } from "./LibrarySync";
import { Count } from "./kit";
import { ChevronDown } from "lucide-react";

/** A row's identity: a key can appear again with a later `effective_from`. */
const rowId = (e: CatalogueEntry) => `${e.key}@${e.effective_from ?? ""}`;

/** 价格目录: the public list prices built into the control plane, read-only. */
export function Catalogue(_props: TabProps) {
  const [items, setItems] = useState<CatalogueEntry[] | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const [source, setSource] = useState("");
  const records = useRecordTable({
    items: (items ?? []).filter((entry) => source === "" || Boolean(entry.source_url) === (source === "sourced")),
    searchText: (entry) => [entry.key, ...entry.aliases],
    resetKey: source,
  });

  useEffect(() => {
    const live = { on: true };
    pricing
      .catalogue()
      .then((r) => live.on && setItems(r.items))
      .catch((e: unknown) => live.on && setFailure(errorText(e)));
    return () => {
      live.on = false;
    };
  }, []);
  const tierCounts = new Map<string, number>();
  for (const entry of items ?? []) {
    for (const tier of Object.keys(entry.card)) tierCounts.set(tier, (tierCounts.get(tier) ?? 0) + 1);
  }
  const tiers = [...tierCounts.entries()].sort((a, b) => b[1] - a[1]);
  const sourced = items?.filter((entry) => entry.source_url).length ?? 0;

  return (
    <>
      <PageHeader title="价格目录" eyebrow="价格 / 公开目录" dataLead={items === null ? "读取内置公开标价。" : `目录包含 ${fmtInt(items.length)} 个价格版本，覆盖 ${fmtInt(new Set(items.map((e) => e.key)).size)} 个目录标识。`} description="内置的公开标价，只读；每一条注明来源页面与抄录日期。" />
      {failure && (
        <div className="note crit mt-0" role="alert">
          {failure}
        </div>
      )}
      <Card className="pricing-catalogue-summary mb-3">
        <dl className="compact-overview-facts">
          <div><dt>公开价格版本</dt><dd>{items === null ? "—" : fmtInt(items.length)}</dd></div>
          <div><dt>目录标识</dt><dd>{items === null ? "—" : fmtInt(new Set(items.map((entry) => entry.key)).size)}</dd></div>
          <div><dt>附来源链接</dt><dd>{items === null ? "—" : fmtInt(sourced)}</dd></div>
        </dl>
        <div className="pricing-tier-coverage" aria-label="服务档位覆盖">{tiers.map(([tier, count]) => <span key={tier}>{tier} <strong>{fmtInt(count)}</strong></span>)}</div>
      </Card>
      <Card className="catalogue-records mb-0">
        <CardHeader number={3} className="mb-0" title={<>目录条目{items && <Count n={items.length} />}</>} />
        {!failure && items === null && <Loading />}
        {items !== null && items.length === 0 && <EmptyState text="目录里还没有条目" />}
        {items !== null && items.length > 0 && (
          <>
          <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索目录", placeholder: "目录标识或别名" }}>
            <label className="f filter"><span>来源</span><select value={source} onChange={(e) => setSource(e.target.value)}><option value="">全部</option><option value="sourced">附来源链接</option><option value="missing">无来源链接</option></select></label>
          </RecordTableToolbar>
          <DataTable label="价格目录">
            <colgroup>
              <col className="catalogue-col-name" />
              <col className="catalogue-col-alias" />
              <col className="catalogue-col-source" />
              <col className="catalogue-col-date" />
              <col className="catalogue-col-notes" />
              <col className="catalogue-col-actions" />
            </colgroup>
            <thead>
              <tr>
                <th>条目</th>
                <th>别名</th>
                <th>来源</th>
                <th>日期</th>
                <th>说明</th>
                <th className="right"></th>
              </tr>
            </thead>
            <tbody>
              {records.rows.length === 0 && <tr><td colSpan={6}><EmptyState text="没有符合筛选条件的目录条目" /></td></tr>}
              {records.rows.map((e) => {
                const id = rowId(e);
                const expanded = open === id;
                return (
                  <Fragment key={id}>
                    <tr data-finance-record>
                      <td data-label="条目">
                        <code>{e.key}</code>
                      </td>
                      <td data-label="别名">
                        <span className="trunc [--trunc:11rem]" title={e.aliases.join("、")}>
                          {e.aliases.length ? e.aliases.join("、") : "—"}
                        </span>
                      </td>
                      <td data-label="来源">
                        {e.source_url ? (
                          <a
                            href={e.source_url}
                            target="_blank"
                            rel="noreferrer noopener"
                            className="trunc inline-flex min-h-11 min-w-11 items-center [--trunc:15rem]"
                            title={e.source_url}
                          >
                            {e.source_url}
                          </a>
                        ) : (
                          "—"
                        )}
                      </td>
                      <td data-label="日期" className="tabular">
                        {e.effective_from ? `${fmtBeijing(e.effective_from)} 起` : (e.as_of ?? "—")}
                        {e.effective_from && e.as_of && <div className="cell-note">抄录于 {e.as_of}</div>}
                      </td>
                      {/* 说明先显示三行摘要，完整内容可在标题和展开条目中查看。 */}
                      <td data-label="说明" className="w-full max-w-0 text-caption text-muted">
                        <span className="line-clamp-3" title={e.notes ?? undefined}>
                          {e.notes ?? "—"}
                        </span>
                      </td>
                      <td data-label="操作" className="right">
                        <Button
                          size="sm"
                          kind="quiet"
                          type="button"
                          aria-expanded={expanded}
                          aria-label={`${expanded ? "收起" : "展开"} ${e.key}`}
                          title={expanded ? "收起完整价格" : "展开完整价格"}
                          className="finance-icon-button"
                          onClick={() => setOpen(expanded ? null : id)}
                        >
                          <ChevronDown aria-hidden style={{ transform: expanded ? "rotate(180deg)" : undefined }} />
                        </Button>
                      </td>
                    </tr>
                    {expanded && (
                      <tr className="dt-detail">
                        <td colSpan={6} className="whitespace-normal">
                          <div className="catalogue-entry-detail grid gap-6 p-4">
                            <div className="min-w-0">
                              <div className="library-card-block">
                                <p className="m-0 font-medium">完整价格 · 美元</p>
                                <CardLines card={e.card} />
                              </div>
                              <p className="hint mt-3">卡上的价格以每百万单位存储：联网搜索的 10000 即每千次 10 美元。{TIER_HINT}</p>
                            </div>
                            <div className="min-w-0">
                              <JsonEditor value={JSON.stringify(e, null, 2)} details={[]} />
                            </div>
                          </div>
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </DataTable>
          <RecordTablePager page={records.page} pageSize={records.pageSize} onPageChange={records.setPage} onPageSizeChange={records.setPageSize} total={records.filteredCount} label="价格目录分页" />
          </>
        )}
      </Card>
    </>
  );
}
