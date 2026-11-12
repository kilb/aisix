import { Fragment, useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { ApiError, library, type LibraryMeta, type LibraryPage } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { droppedFieldText, flagText } from "../../lib/importReview";
import { longContextLines, longContextText } from "../../lib/library";
import { fmtBeijing } from "../../lib/time";
import { safeHttpUrl } from "../../lib/safeUrl";
import { Button } from "../../ui/Button";
import { DataTable } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import type { RecordPageSize } from "../../lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { useToast } from "../../ui/Toast";
import { usdPerMillion } from "./Baselines";
import { LibrarySync } from "./LibrarySync";
import { CREDIT } from "./PricingImport";
import { BentoGrid, Card, CardHeader, CardMessage, ChartSkeleton, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { CardFooter, Count } from "./kit";
import { PriceCardDetails } from "./PriceCardDetails";

/** Where the library in use came from, in one line. */
export function libraryWords(meta: LibraryMeta): string {
  const origin = meta.origin === "fetch" ? "在线获取" : meta.origin === "upload" ? "文件上传" : meta.origin;
  const revision = meta.source_ref ? `，版本 ${meta.source_ref}` : "";
  return `${origin}${revision}，${meta.entries} 个条目，获取于 ${fmtBeijing(meta.fetched_at)}（${meta.created_by.email}）`;
}

/** 价格 · 价格库: the stored copy of the public price dataset, its search, and the sync of baselines from it. */
export function PriceLibrary({ canEdit, onGoto }: TabProps) {
  const toast = useToast();
  const [q, setQ] = useState("");
  const [pageNumber, setPageNumber] = useState(1);
  const [pageSize, setPageSize] = useState<RecordPageSize>(25);
  const [loading, setLoading] = useState(false);
  const [page, setPage] = useState<LibraryPage | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  // 摘要标注最近一次成功读取的查询，输入变化时不把旧结果冒充新命中。
  const [loadedQuery, setLoadedQuery] = useState("");
  const [loadedPage, setLoadedPage] = useState(1);
  const [loadedPageSize, setLoadedPageSize] = useState<RecordPageSize>(25);
  const [error, setError] = useState<string | null>(null);
  const [fetching, setFetching] = useState(false);
  const [fetchError, setFetchError] = useState<string | null>(null);
  /** Counts the library's replacements on this page, so the sync preview reads again after one. */
  const [generation, setGeneration] = useState(0);
  /** The newest search sent: an answer to an older one is dropped. */
  const latest = useRef(0);
  const libraryId = useRef<string | null>(null);

  const load = useCallback(async (text: string, number: number, size: RecordPageSize) => {
    const n = ++latest.current;
    setLoading(true);
    try {
      const next = await library.browse(text, size, (number - 1) * size);
      if (n !== latest.current) return;
      const replaced = libraryId.current !== null && libraryId.current !== next.library?.id;
      libraryId.current = next.library?.id ?? null;
      const lastPage = Math.max(1, Math.ceil(next.total / size));
      if (number > 1 && (replaced || number > lastPage)) {
        setPageNumber(replaced ? 1 : lastPage);
        return;
      }
      setPage(next);
      setLoadedQuery(text.trim());
      setLoadedPage(number);
      setLoadedPageSize(size);
      setError(null);
    } catch (e) {
      if (n !== latest.current) return;
      setError(errorText(e));
    } finally {
      if (n === latest.current) setLoading(false);
    }
  }, []);

  // A search is read once typing pauses.
  useEffect(() => {
    const timer = setTimeout(() => void load(q, pageNumber, pageSize), 250);
    return () => {
      clearTimeout(timer);
      latest.current += 1;
    };
  }, [q, pageNumber, pageSize, load]);

  const refresh = () => load(q, pageNumber, pageSize);

  async function update() {
    if (fetching) return;
    setFetching(true);
    setFetchError(null);
    try {
      await library.fetch();
      toast("价格库已更新");
      setGeneration((g) => g + 1);
      if (pageNumber === 1) await load(q, 1, pageSize);
      else setPageNumber(1);
    } catch (e) {
      const detail = e instanceof ApiError && e.details.length > 0 ? `：${e.details.map((d) => d.message).join("；")}` : "";
      setFetchError(`${e instanceof ApiError ? messageFor(e) : errorText(e)}${detail}`);
    } finally {
      setFetching(false);
    }
  }

  const meta = page?.library ?? null;
  const sourceHref = safeHttpUrl(meta?.source_url);
  const displayedLongContext = page?.items.filter((entry) => longContextLines(entry.card).length > 0).length ?? 0;
  const displayedFlagged = page?.items.filter((entry) => entry.flags.length > 0).length ?? 0;
  return (
    <>
      <PageHeader
        eyebrow="价格管理 / 价格库"
        title="价格库"
        dataLead={meta && page ? <>当前价格库包含 {meta.entries.toLocaleString("zh-CN")} 个条目；{loadedQuery ? `「${loadedQuery}」匹配` : "全部搜索范围有"} {page.total.toLocaleString("zh-CN")} 条，显示 {page.items.length.toLocaleString("zh-CN")} 条。核对完整价格与来源后再同步基准价。</> : "使用实际存储的价格库检索模型，核对来源与完整价格后再同步。"}
        actions={
          <Button type="button" icon="refresh" onClick={() => void refresh()}>
            刷新
          </Button>
        }
      />
      <p className="mb-6 text-body text-ink-soft">{CREDIT}</p>
      <BentoGrid className="mb-3" aria-label="价格库概览">
        <Card className={span.d7}>
          <CardHeader number={1} title="当前价格库" description="实际存储版本 · 全部条目" />
          {meta ? <Metric size="hero">{meta.entries.toLocaleString("zh-CN")}</Metric> : page ? <CardMessage>还没有价格库</CardMessage> : error ? <CardMessage onRetry={() => void refresh()}>未能读取价格库</CardMessage> : <MetricSkeleton />}
          <p className="library-status mt-5 text-body text-ink-soft" aria-label="价格库状态">
            {page === null ? error ? "价格库读取失败。" : "正在读取价格库…" : meta ? <>当前价格库：{libraryWords(meta)}</> : "还没有价格库。"}
          </p>
          {meta && (
            <dl className="library-provenance mt-4 text-caption">
              <div><dt>数据集</dt><dd className="mono">{meta.dataset}</dd></div>
              <div><dt>来源版本</dt><dd className="mono">{meta.source_ref || "未提供版本"}</dd></div>
              <div><dt>来源地址</dt><dd>{sourceHref ? <a href={sourceHref} target="_blank" rel="noopener noreferrer">{sourceHref}</a> : meta.source_url ? "来源地址不是可打开的 HTTP(S) 页面" : "未提供来源地址"}</dd></div>
              <div><dt>内容校验</dt><dd className="mono">{meta.sha256}</dd></div>
            </dl>
          )}
          {canEdit && (
            <>
              <p className="mt-4 text-caption text-muted">从数据集的发布地址获取最新数据。无法访问外网时，可在「导入审核」上传数据集文件，同样会更新价格库。</p>
              <CardFooter>
                <Button type="button" onClick={() => onGoto("pricing", "imports")}>去「导入审核」上传</Button>
                <Button type="button" kind="primary" busy={fetching} busyLabel="更新中…" onClick={() => void update()}>更新价格库</Button>
              </CardFooter>
            </>
          )}
          {fetchError && <div className="note crit mb-0" role="alert">更新失败：{fetchError}</div>}
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="搜索命中" description={page ? loadedQuery ? `最近成功查询：${loadedQuery}` : "最近成功查询：全部条目" : "等待读取搜索结果"} />
          {meta && page ? <Metric size="display">{page.total.toLocaleString("zh-CN")}</Metric> : page ? <CardMessage>更新或上传数据集后即可搜索</CardMessage> : error ? <CardMessage onRetry={() => void refresh()}>未能读取搜索结果</CardMessage> : <MetricSkeleton />}
          <p className="mt-4 text-body text-on-inverse-muted">搜索支持名称、ID、别名与供应商。结果按供应商和模型 ID 排序，可翻页查看全部命中。</p>
          {page && meta && <CardFooter className="justify-start border-surface/12 text-caption text-on-inverse-muted">当前显示 {page.items.length} 条，其中 {displayedLongContext} 条含长上下文价、{displayedFlagged} 条带审核标记。此计数仅覆盖当前显示条目。</CardFooter>}
        </Card>
      </BentoGrid>
      <Card>
        <CardHeader
          number={3}
          title={<>价格库条目{page?.library && <Count n={page.total} />}</>}
          description="输入、输出为默认档基础价；完整价格卡保留所有维度、服务档与长上下文价格。"
        />
        <RecordTableToolbar search={{ value: q, onChange: (value) => { latest.current += 1; setQ(value); setPageNumber(1); }, label: "搜索模型", placeholder: "模型名称、ID、别名或供应商" }} />
        {error && (
          <div className="note crit" role="alert">
            {error}{" "}
            <Button type="button" onClick={() => void refresh()}>
              重试
            </Button>
          </div>
        )}
        {page === null ? (
          !error && <ChartSkeleton />
        ) : !meta ? (
          <EmptyState text="还没有价格库。点「更新价格库」获取，或在「导入审核」上传数据集文件。" />
        ) : page.items.length === 0 ? (
          <EmptyState text="没有匹配的模型。" />
        ) : (
          <>
            <div className="library-browser-table" aria-busy={loading}><DataTable label="价格库条目" scroll>
              <thead>
                <tr>
                  <th>供应商</th>
                  <th>模型</th>
                  <th className="num">输入</th>
                  <th className="num">输出</th>
                  <th>价格详情</th>
                  <th>价格页面</th>
                </tr>
              </thead>
              <tbody>
                {page.items.map((e) => {
                  const base = e.card?.default?.base;
                  const href = safeHttpUrl(e.pricing_url);
                  const bands = longContextLines(e.card);
                  const entryId = `${e.provider}/${e.model}`;
                  const open = expanded === entryId;
                  const detailId = `library-price-${encodeURIComponent(entryId)}`;
                  return (
                    <Fragment key={entryId}><tr>
                      <td className="text-muted">{e.provider}</td>
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
                        {bands.length > 0 && (
                          <span className="library-long-context mt-1">
                            <span className="tag">含长上下文价</span>
                            {bands.map((line) => (
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
                        {e.card ? <Button type="button" kind="quiet" size="sm" aria-expanded={open} aria-controls={open ? detailId : undefined} aria-label={`${open ? "收起" : "展开"} ${entryId} 的完整价格卡`} onClick={() => setExpanded(open ? null : entryId)}>{open ? "收起价格" : "展开价格"}</Button> : <span className="text-muted">无可同步价格卡</span>}
                        {e.prices_checked && <span className="cell-note">价格核对日期：{e.prices_checked}</span>}
                      </td>
                      <td>
                        {href ? (
                          <a href={href} target="_blank" rel="noopener noreferrer" className="inline-flex min-h-11 min-w-11 items-center">
                            价格页面
                          </a>
                        ) : e.pricing_url ? (
                          // 非 HTTP(S) 地址保留可读文本，不生成链接。
                          <span className="trunc" title={e.pricing_url}>
                            {e.pricing_url}
                          </span>
                        ) : (
                          "—"
                        )}
                      </td>
                    </tr>
                    {open && <tr className="dt-detail"><td colSpan={6} className="library-expanded-cell whitespace-normal">
                      <section id={detailId} className="library-expanded-price" aria-label={`${entryId} 的完整价格卡`}>
                        <header>
                          <h3>{e.name || e.model}</h3>
                          <p>{entryId} · 完整价格（美元）</p>
                        </header>
                        <PriceCardDetails card={e.card} label={`${entryId} 的全部档位与价格维度`} />
                      </section>
                    </td></tr>}
                    </Fragment>
                  );
                })}
              </tbody>
            </DataTable></div>
          </>
        )}
        {page && meta && <RecordTablePager
          page={loadedPage}
          pageSize={loadedPageSize}
          onPageChange={(number) => {
            latest.current += 1;
            if (number === pageNumber) void load(q, number, pageSize);
            else setPageNumber(number);
          }}
          onPageSizeChange={(size) => {
            latest.current += 1;
            if (size === pageSize && pageNumber === 1) void load(q, 1, size);
            else { setPageSize(size); setPageNumber(1); }
          }}
          total={page.total}
          busy={loading || (error === null && (q.trim() !== loadedQuery || pageNumber !== loadedPage || pageSize !== loadedPageSize))}
          label="价格库分页"
        />}
      </Card>
      <LibrarySync canEdit={canEdit} generation={generation} cardNumber={4} />
    </>
  );
}
