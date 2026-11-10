import { Fragment, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { JsonEditor } from "../../components/JsonEditor";
import { pricing, type CatalogueEntry } from "../../lib/api";
import { errorText } from "../../lib/fmt";
import { TIER_HINT } from "../../lib/rateUnits";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { CardLines } from "./LibrarySync";

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
    <div className="panel">
      <h2>价格目录</h2>
      <p className="hint">内置的公开标价，只读；每一条注明来源页面与抄录日期。</p>
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
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
                    <td>{e.aliases.length ? e.aliases.join("、") : "—"}</td>
                    <td>
                      {e.source_url ? (
                        <a href={e.source_url} target="_blank" rel="noreferrer noopener" className="trunc" title={e.source_url}>
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
                    <td>{e.notes ?? "—"}</td>
                    <td className="right">
                      <Button
                        size="sm"
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
                    <tr>
                      <td colSpan={6}>
                        <p className="library-card-block">
                          价格（美元/百万 tokens）：
                          <CardLines card={e.card} />
                        </p>
                        <p className="hint">卡上的价格以每百万单位存储：联网搜索的 10000 即每千次 10 美元。{TIER_HINT}</p>
                        <JsonEditor value={JSON.stringify(e, null, 2)} details={[]} />
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </DataTable>
      )}
    </div>
  );
}
