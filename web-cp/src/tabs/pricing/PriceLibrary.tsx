import { useCallback, useEffect, useRef, useState } from "react";
import { Database } from "lucide-react";
import type { TabProps } from "../../App";
import { ApiError, library, type LibraryMeta, type LibraryPage } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { droppedFieldText, flagText } from "../../lib/importReview";
import { longContextLines, longContextText } from "../../lib/library";
import { fmtUtc } from "../../lib/money";
import { safeHttpUrl } from "../../lib/safeUrl";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field } from "../../ui/Field";
import { useToast } from "../../ui/Toast";
import { usdPerMillion } from "./Baselines";
import { LibrarySync } from "./LibrarySync";
import { CREDIT } from "./PricingImport";
import { Card, CardHeader, PageHeader } from "../../components/page";
import { Count } from "./kit";

/** How many entries one search lists. */
const LISTED = 50;

/** Where the library in use came from, in one line. */
export function libraryWords(meta: LibraryMeta): string {
  const origin = meta.origin === "fetch" ? "在线获取" : meta.origin === "upload" ? "文件上传" : meta.origin;
  const revision = meta.source_ref ? `，版本 ${meta.source_ref}` : "";
  return `${origin}${revision}，${meta.entries} 个条目，获取于 ${fmtUtc(meta.fetched_at)}（${meta.created_by.email}）`;
}

/** 价格 · 价格库: the stored copy of the public price dataset, its search, and the sync of baselines from it. */
export function PriceLibrary({ canEdit, onGoto }: TabProps) {
  const toast = useToast();
  const [q, setQ] = useState("");
  const [page, setPage] = useState<LibraryPage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fetching, setFetching] = useState(false);
  const [fetchError, setFetchError] = useState<string | null>(null);
  /** Counts the library's replacements on this page, so the sync preview reads again after one. */
  const [generation, setGeneration] = useState(0);
  /** The newest search sent: an answer to an older one is dropped. */
  const latest = useRef(0);

  const load = useCallback(async (text: string) => {
    const n = ++latest.current;
    try {
      const next = await library.browse(text, LISTED);
      if (n !== latest.current) return;
      setPage(next);
      setError(null);
    } catch (e) {
      if (n !== latest.current) return;
      setError(errorText(e));
    }
  }, []);

  // A search is read once typing pauses.
  useEffect(() => {
    const timer = setTimeout(() => void load(q), 250);
    return () => clearTimeout(timer);
  }, [q, load]);

  async function update() {
    if (fetching) return;
    setFetching(true);
    setFetchError(null);
    try {
      await library.fetch();
      toast("价格库已更新");
      setGeneration((g) => g + 1);
      await load(q);
    } catch (e) {
      const detail = e instanceof ApiError && e.details.length > 0 ? `：${e.details.map((d) => d.message).join("；")}` : "";
      setFetchError(`${e instanceof ApiError ? messageFor(e) : errorText(e)}${detail}`);
    } finally {
      setFetching(false);
    }
  }

  const meta = page?.library ?? null;
  return (
    <>
      <PageHeader
        title="价格库"
        description={CREDIT}
        actions={
          <Button type="button" icon="refresh" onClick={() => void load(q)}>
            刷新
          </Button>
        }
      />
      <Card>
        <div className="flex flex-wrap items-center justify-between gap-x-6 gap-y-3 lg:flex-nowrap">
          <div className="flex min-w-0 items-center gap-3">
            <span className="grid size-9 shrink-0 place-items-center rounded-lg border border-border bg-muted text-muted-foreground" aria-hidden="true">
              <Database className="size-4" />
            </span>
            <p className="library-status m-0 font-medium" aria-label="价格库状态">
              {page === null ? "正在读取价格库…" : meta ? <>当前价格库：{libraryWords(meta)}</> : "还没有价格库。"}
            </p>
          </div>
          {canEdit && (
            <div className="flex shrink-0 flex-wrap items-center gap-2">
              <Button type="button" onClick={() => onGoto("pricing", "imports")}>
                去「导入审核」上传
              </Button>
              <Button type="button" kind="primary" busy={fetching} busyLabel="更新中…" onClick={() => void update()}>
                更新价格库
              </Button>
            </div>
          )}
        </div>
        {canEdit && (
          <p className="hint mt-4 max-w-none border-t border-border pt-3 text-[13px]">
            从数据集的发布地址获取最新数据。无法访问外网时，可在「导入审核」上传数据集文件，同样会更新价格库。
          </p>
        )}
        {fetchError && (
          <div className="note crit mb-0" role="alert">
            更新失败：{fetchError}
          </div>
        )}
      </Card>
      <Card>
        <CardHeader
          className="mb-0"
          title={<>价格库条目{page?.library && <Count n={page.total} />}</>}
          actions={
            <div className="w-full sm:w-80 [&_.field]:mb-0">
              <Field label="搜索模型" hint="按模型名称、ID、别名或供应商">
                {(aria) => <input {...aria} type="search" autoComplete="off" value={q} onChange={(e) => setQ(e.target.value)} />}
              </Field>
            </div>
          }
        />
        {error && (
          <div className="note crit" role="alert">
            {error}{" "}
            <Button type="button" onClick={() => void load(q)}>
              重试
            </Button>
          </div>
        )}
        {page === null ? (
          !error && <Loading />
        ) : !meta ? (
          <EmptyState text="还没有价格库。点「更新价格库」获取，或在「导入审核」上传数据集文件。" />
        ) : page.items.length === 0 ? (
          <EmptyState text="没有匹配的模型。" />
        ) : (
          <>
            <DataTable label="价格库条目" scroll>
              <thead>
                <tr>
                  <th>供应商</th>
                  <th>模型</th>
                  <th className="num">输入</th>
                  <th className="num">输出</th>
                  <th>价格页面</th>
                </tr>
              </thead>
              <tbody>
                {page.items.map((e) => {
                  const base = e.card?.default?.base;
                  const href = safeHttpUrl(e.pricing_url);
                  return (
                    <tr key={`${e.provider}/${e.model}`}>
                      <td className="text-muted-foreground">{e.provider}</td>
                      <td>
                        <span className="mono font-medium">{e.model}</span>
                        {e.name && e.name !== e.model && <span className="cell-note">{e.name}</span>}
                        {e.flags.length > 0 && (
                          <span className="cell-note library-flags">{e.flags.map(flagText).join("、")}</span>
                        )}
                        {e.dropped?.length > 0 && (
                          <span className="cell-note">
                            未计入：{e.dropped.map(droppedFieldText).join("、")}
                          </span>
                        )}
                        {longContextLines(e.card).length > 0 && (
                          <span className="library-long-context mt-1">
                            <span className="tag">含长上下文价</span>
                            {longContextLines(e.card).map((line) => (
                              <span key={`${line.tier}/${line.above_input_tokens}`} className="cell-note tabular">
                                {longContextText(line)}
                              </span>
                            ))}
                          </span>
                        )}
                      </td>
                      <td className="num tabular">{base?.input !== undefined ? usdPerMillion(base.input) : "—"}</td>
                      <td className="num tabular">{base?.output !== undefined ? usdPerMillion(base.output) : "—"}</td>
                      <td>
                        {href ? (
                          <a href={href} target="_blank" rel="noopener noreferrer">
                            价格页面
                          </a>
                        ) : e.pricing_url ? (
                          // Not an http(s) page: readable, and never a link.
                          <span className="trunc" title={e.pricing_url}>
                            {e.pricing_url}
                          </span>
                        ) : (
                          "—"
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </DataTable>
            <p className="hint -mx-6 -mb-6 mt-0 max-w-none rounded-b-[calc(var(--radius)+3px)] border-t border-border bg-muted/40 px-6 py-3 text-[13px]">
              共 {page.total} 条{page.total > page.items.length ? `，显示前 ${page.items.length} 条；输入更多文字以缩小范围` : ""}
            </p>
          </>
        )}
      </Card>
      <LibrarySync canEdit={canEdit} generation={generation} />
    </>
  );
}
