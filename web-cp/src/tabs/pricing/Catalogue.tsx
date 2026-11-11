import { Fragment, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { JsonEditor } from "../../components/JsonEditor";
import { pricing, type CatalogueEntry } from "../../lib/api";
import { errorText } from "../../lib/fmt";
import { TIER_HINT } from "../../lib/rateUnits";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Card, CardHeader, PageHeader } from "../../components/page";
import { CardLines } from "./LibrarySync";
import { Count } from "./kit";

/** A row's identity: a key can appear again with a later `effective_from`. */
const rowId = (e: CatalogueEntry) => `${e.key}@${e.effective_from ?? ""}`;

/** 价格目录: the public list prices built into the control plane, read-only. */
export function Catalogue(_props: TabProps) {
  const [items, setItems] = useState<CatalogueEntry[] | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

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

  return (
    <>
      <PageHeader title="价格目录" description="内置的公开标价，只读；每一条注明来源页面与抄录日期。" />
      {failure && (
        <div className="note crit mt-0" role="alert">
          {failure}
        </div>
      )}
      <Card className="mb-0">
        <CardHeader className="mb-0" title={<>目录条目{items && <Count n={items.length} />}</>} />
        {!failure && items === null && <Loading />}
        {items !== null && items.length === 0 && <EmptyState text="目录里还没有条目" />}
        {items !== null && items.length > 0 && (
          <DataTable label="价格目录">
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
              {items.map((e) => {
                const id = rowId(e);
                const expanded = open === id;
                return (
                  <Fragment key={id}>
                    <tr>
                      <td>
                        <code>{e.key}</code>
                      </td>
                      <td>
                        <span className="trunc [--trunc:11rem]" title={e.aliases.join("、")}>
                          {e.aliases.length ? e.aliases.join("、") : "—"}
                        </span>
                      </td>
                      <td>
                        {e.source_url ? (
                          <a
                            href={e.source_url}
                            target="_blank"
                            rel="noreferrer noopener"
                            className="trunc [--trunc:15rem]"
                            title={e.source_url}
                          >
                            {e.source_url}
                          </a>
                        ) : (
                          "—"
                        )}
                      </td>
                      <td className="tabular">
                        {e.effective_from ? `${e.effective_from} 起` : (e.as_of ?? "—")}
                        {e.effective_from && e.as_of && <div className="cell-note">抄录于 {e.as_of}</div>}
                      </td>
                      {/* The notes take whatever width is left, cut to one line: their whole text is in the title and the expanded entry. */}
                      <td className="w-full max-w-0 text-[13px] text-muted-foreground">
                        <span className="block truncate" title={e.notes ?? undefined}>
                          {e.notes ?? "—"}
                        </span>
                      </td>
                      <td className="right">
                        <Button
                          size="sm"
                          kind="quiet"
                          type="button"
                          aria-expanded={expanded}
                          aria-label={`${expanded ? "收起" : "展开"} ${e.key}`}
                          onClick={() => setOpen(expanded ? null : id)}
                        >
                          {expanded ? "收起" : "展开"}
                        </Button>
                      </td>
                    </tr>
                    {expanded && (
                      <tr className="dt-detail">
                        <td colSpan={6} className="whitespace-normal">
                          <div className="grid gap-6 rounded-xl border border-border bg-muted/50 p-4 lg:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
                            <div className="min-w-0">
                              <p className="library-card-block">
                                价格（美元/百万 tokens）：
                                <CardLines card={e.card} />
                              </p>
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
        )}
      </Card>
    </>
  );
}
