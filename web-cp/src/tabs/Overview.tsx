import { useCallback, useEffect, useState } from "react";
import type { TabId, TabProps } from "../App";
import { Msg, type Note } from "../components/Msg";
import { environment, pricingStatus, rollout, type Environment, type PricingStatus, type RolloutSummary } from "../lib/api";
import { errorText, fmtInt } from "../lib/fmt";
import { statusView } from "../lib/pricingStatus";
import { Button } from "../ui/Button";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../components/page";
import { RankBars } from "../components/charts";
import { ApiMetricsPanel } from "../components/ApiMetricsPanel";
import "./providers/OperationsBento.css";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";

/** How often the summary is read again: about one gateway heartbeat. */
const REFRESH_MS = 5_000;
/** The pricing status costs the control plane a query over the charges, so it is read less often. */
const PRICING_REFRESH_MS = 60_000;

type MetricsRange = "24h" | "7d" | "30d" | "90d";
const METRICS_RANGES: { value: MetricsRange; label: string; days: number }[] = [
  { value: "24h", label: "最近 24 小时", days: 1 },
  { value: "7d", label: "最近 7 天", days: 7 },
  { value: "30d", label: "最近 30 天", days: 30 },
  { value: "90d", label: "最近 90 天", days: 90 },
];

/** 仅在主动切换或刷新时固定新的真实时间窗口，轮询不改变窗口。 */
function metricsWindow(range: MetricsRange) {
  const to = new Date();
  const days = METRICS_RANGES.find((r) => r.value === range)?.days ?? 1;
  return { range, from: new Date(to.getTime() - days * 86_400_000).toISOString(), to: to.toISOString() };
}

/**
 * Where a document of each kind is edited. Kinds the tenant and key pages
 * generate go to those pages; every kind without a form of its own goes to
 * the generic page.
 */
const TAB_OF_KIND: Record<string, { tab: TabId; label: string }> = {
  provider_keys: { tab: "providers", label: "上游账号" },
  models: { tab: "models", label: "模型与路由" },
  cache_policies: { tab: "cache", label: "缓存策略" },
  api_keys: { tab: "keys", label: "密钥" },
  teams: { tab: "tenants", label: "租户" },
  users: { tab: "tenants", label: "租户" },
};
const GENERIC = { tab: "resources", label: "全部资源" } as const;

/**
 * The environment at a glance: which environment and control plane this
 * is, how many gateways are online, how far saved documents have reached
 * them, and everything that stops a document from taking effect. Re-read
 * every few seconds while open.
 */
export function Overview({ operator, onGoto }: TabProps) {
  const isAdmin = operator.role === "admin";
  const [env, setEnv] = useState<Environment | null>(null);
  const [metrics, setMetrics] = useState(() => metricsWindow("24h"));
  const [envError, setEnvError] = useState<string | null>(null);
  const [pricing, setPricing] = useState<PricingStatus | null>(null);
  const [pricingError, setPricingError] = useState<string | null>(null);
  const [summary, setSummary] = useState<RolloutSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);
  // Which recovery is in flight.
  const [acting, setActing] = useState<"take-over" | "clean" | null>(null);

  // A failed read keeps the last summary on screen, under the error.
  const readSummary = useCallback(async () => {
    try {
      setSummary(await rollout.summary());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  const readPricing = useCallback(async () => {
    try {
      setPricing(await pricingStatus.get());
      setPricingError(null);
    } catch (e) {
      setPricingError(errorText(e));
    }
  }, []);

  useEffect(() => {
    let live = true;
    let timer: number | undefined;
    environment.get().then(
      (e) => live && setEnv(e),
      (e) => live && setEnvError(errorText(e)),
    );
    const tick = async () => {
      await readSummary();
      if (live) timer = window.setTimeout(() => void tick(), REFRESH_MS);
    };
    void tick();
    void readPricing();
    const slow = window.setInterval(() => void readPricing(), PRICING_REFRESH_MS);
    return () => {
      live = false;
      window.clearTimeout(timer);
      window.clearInterval(slow);
    };
  }, [readSummary, readPricing]);

  async function allowDeletes() {
    setBusy(true);
    setActing("clean");
    setNote(null);
    try {
      const r = await rollout.reconcile(true);
      setNote({ kind: "ok", text: `已清理 ${fmtInt(r.deleted_strays)} 个键。` });
    } catch (e) {
      setNote({ kind: "crit", text: errorText(e) });
    } finally {
      setBusy(false);
      setActing(null);
      await readSummary();
    }
  }

  async function takeOver(owner: string) {
    setBusy(true);
    setActing("take-over");
    setNote(null);
    try {
      await rollout.takeOver(owner);
      setNote({ kind: "ok", text: "已接管该环境，本控制面恢复下发配置。" });
    } catch (e) {
      setNote({ kind: "crit", text: errorText(e) });
    } finally {
      setBusy(false);
      setActing(null);
      await readSummary();
    }
  }

  const pricingView = pricing ? statusView(pricing) : null;
  const s = summary;
  const blocked = s?.projection_blocked;
  const hasRecovery = Boolean(blocked || s?.projection_stalled || s?.reconcile_blocked || s?.unsafe.length || note);
  const waiting = s ? s.pending + s.propagating : null;
  const currentState = !s ? "正在读取下发状态" : blocked || s.projection_stalled ? "配置下发被阻止" : s.reconcile_blocked ? "环境清理已暂停" : s.rejected.length > 0 ? "有资源被网关拒绝" : s.unsafe.length > 0 ? "网关忽略了必需字段" : waiting ? "配置正在下发" : s.online_instances === 0 ? "尚无在线网关" : s.partially_compatible.length > 0 ? "配置部分兼容" : "当前配置已下发";
  return (
    <>
      <PageHeader
        eyebrow="OPERATIONS · 当前环境"
        title="运行概况"
        dataLead={s ? <>当前有 {fmtInt(s.online_instances)} 个在线网关，{fmtInt(s.pending)} 个资源待下发、{fmtInt(s.propagating)} 个资源下发中。{error && "刷新失败，以下保留最近成功结果。"}</> : "查看环境、网关心跳、配置下发与最近 24 小时的计价健康。"}
      />
      <BentoGrid aria-label="运行概况">
        <div className={span.d12}>
          <div className="mb-5 flex flex-wrap items-end justify-between gap-4">
            <div className="min-w-0">
              <p className="text-caption font-medium text-muted">API 请求统计</p>
            </div>
            <div className="flex max-w-full flex-wrap items-end gap-3">
              <label className="f filter m-0">
                <span>统计范围</span>
                <select value={metrics.range} onChange={(e) => setMetrics(metricsWindow(e.target.value as MetricsRange))}>
                  {METRICS_RANGES.map((r) => <option key={r.value} value={r.value}>{r.label}</option>)}
                </select>
              </label>
              <Button onClick={() => setMetrics(metricsWindow(metrics.range))}>刷新统计</Button>
            </div>
          </div>
          <ApiMetricsPanel scope="admin" enabled={true} from={metrics.from} to={metrics.to} numberStart={1} compact />
        </div>
        <Card className={span.d8}>
          <CardHeader number={3} title="配置下发 · 当前" actions={<Button onClick={() => void readSummary()}>刷新状态</Button>} />
          {s === null ? (
            error ? <CardMessage onRetry={() => void readSummary()}>读取下发状态失败：{error}</CardMessage> : <MetricSkeleton />
          ) : (
            <>
              <div className="ops-metric-line">
                <Metric size="hero">{fmtInt(waiting ?? 0)}</Metric>
                <div className="text-caption text-muted"><p>尚未完成下发的资源</p><p className="mt-2 text-ink">{currentState}</p></div>
              </div>
              <CardFooter>
                <RankBars
                  items={[
                    { label: "待下发", value: s.pending, displayValue: fmtInt(s.pending) },
                    { label: "下发中", value: s.propagating, displayValue: fmtInt(s.propagating) },
                  ]}
                  accessibleLabel={`当前 ${fmtInt(s.pending)} 个资源待下发，${fmtInt(s.propagating)} 个资源下发中`}
                />
                <p className="mt-3 text-caption text-muted">待下发：尚未写入 kine。下发中：在线网关尚未全部应用。</p>
              </CardFooter>
            </>
          )}
          {error && s && <p className="note crit" role="alert">读取失败：{error}</p>}
          <div className="mt-5 grid gap-3 border-t border-ink/7 pt-5 tablet:grid-cols-2">
            <div><p className="text-micro text-muted">环境</p><p className="mt-1 break-all font-mono text-caption">{env?.env_id ?? "—"}</p></div>
            <div><p className="text-micro text-muted">控制面版本</p><p className="mt-1 break-all font-mono text-caption">{env?.cp_version ?? "—"}</p></div>
          </div>
          {envError && <p className="note crit" role="alert">读取环境信息失败：{envError}</p>}
        </Card>

        <Card className={span.d4} tone="inverse">
          <CardHeader number={4} title="在线网关" actions={<span className="text-micro text-on-inverse-muted">当前心跳</span>} />
          {s ? <Metric size="display">{fmtInt(s.online_instances)}</Metric> : error ? <CardMessage onRetry={() => void readSummary()}>心跳数据暂不可用。</CardMessage> : <MetricSkeleton />}
          <p className="mt-4 text-caption text-on-inverse-muted">近期发过心跳的网关实例。离线详情与凭据在网关页查看。</p>
          <CardFooter>
            <dl className="ops-facts">
              <div><dt>已返回的拒绝资源</dt><dd>{s ? fmtInt(s.rejected.length) : "—"}</dd></div>
              <div><dt>忽略必需字段的记录</dt><dd>{s ? fmtInt(s.unsafe.length) : "—"}</dd></div>
            </dl>
            <Button className="mt-4" onClick={() => onGoto("gateways")}>查看网关</Button>
          </CardFooter>
        </Card>

        <Card className={span.d5} aria-label="计价异常">
          <CardHeader number={5} title="未计价调用 · 最近 24 小时" actions={<Button size="sm" onClick={() => onGoto("pricing", "status")}>去「价格 · 计价状态」</Button>} />
          {pricingView ? <Metric size="figure">{fmtInt(pricingView.unpricedTotal)}</Metric> : pricingError ? <CardMessage onRetry={() => void readPricing()}>读取计价状态失败：{pricingError}</CardMessage> : <MetricSkeleton />}
          {pricingView && (
            <CardFooter>
              <RankBars items={pricingView.unpriced.map((r) => ({ label: r.label, value: r.count, displayValue: fmtInt(r.count) }))} accessibleLabel={`最近 24 小时仍有 ${fmtInt(pricingView.unpricedTotal)} 次调用未计价，按原因统计`} emptyLabel="最近 24 小时没有未计价调用。" />
            </CardFooter>
          )}
          {pricingError && pricingView && <p className="note crit" role="alert">读取计价状态失败：{pricingError}</p>}
        </Card>

        <Card className={span.d3}>
          <CardHeader number={6} title="不可用价格版本" />
          {pricingView ? <Metric size="figure">{fmtInt(pricingView.invalid.length)}</Metric> : pricingError ? <CardMessage>暂不可用。</CardMessage> : <MetricSkeleton />}
          <p className="mt-4 text-caption text-muted">计价引擎会跳过这些版本。具体原因在计价状态页查看。</p>
          {pricingView && pricingView.invalid.length > 0 && <p className="note warn" role="status">有价格版本不可用，请查看计价状态。</p>}
        </Card>

        <Card className={span.d4}>
          <CardHeader number={7} title="已更正计价 · 最近 24 小时" />
          {pricing ? pricing.corrected_last_24h ? <Metric size="figure">{fmtInt(pricing.corrected_last_24h.total)}</Metric> : <CardMessage>控制面未提供已更正计数。</CardMessage> : pricingError ? <CardMessage>暂不可用。</CardMessage> : <MetricSkeleton />}
          <p className="mt-4 text-caption text-muted">最近 24 小时已经通过重新计价更正的调用，不计入仍未计价的数量。</p>
        </Card>

        <Card className={span.d7}>
          <CardHeader number={8} title="被拒绝的资源" description="网关加载时拒绝了这些资源，它们没有生效。原因由网关给出。" />
          {s === null ? error ? <CardMessage onRetry={() => void readSummary()}>读取失败，请重试。</CardMessage> : <MetricSkeleton /> : s.rejected.length === 0 ? (
            <CardMessage><Lamp state="applied">没有被拒绝的资源。</Lamp></CardMessage>
          ) : (
            <ul className="rows">
              {s.rejected.map((r) => {
                const where = TAB_OF_KIND[r.kind] ?? GENERIC;
                return (
                  <li key={`${r.kind}/${r.id}`}>
                    <div className="row-head"><code>{r.kind}</code> <strong>{r.name ?? r.id}</strong> <Button size="sm" onClick={() => onGoto(where.tab)}>去「{where.label}」</Button></div>
                    {r.errors.map((e) => <div className="rollout-errors" key={e.instance_id}><code>{e.hostname || e.instance_id}</code>：{e.error}</div>)}
                  </li>
                );
              })}
            </ul>
          )}
          {s && s.rejections_truncated.length > 0 && <p className="note warn">以下网关的拒绝明细超过返回上限：{s.rejections_truncated.join("、")}。当前列表未包含全部拒绝记录。</p>}
        </Card>

        <Card className={span.d5Wide}>
          <CardHeader number={9} title="部分兼容" description="网关加载了这些资源，但忽略了其中的部分字段。" />
          {s === null ? error ? <CardMessage onRetry={() => void readSummary()}>读取失败，请重试。</CardMessage> : <MetricSkeleton /> : s.partially_compatible.length === 0 ? (
            <CardMessage><Lamp state="applied">所有网关都完整加载了配置。</Lamp></CardMessage>
          ) : (
            <ul className="rows">
              {s.partially_compatible.map((p) => <li key={`${p.instance_id}/${p.kind}/${p.field}`}><code>{p.instance_id}</code>：<code>{p.kind}</code> 的 <code>{p.field}</code>（{fmtInt(p.count)} 个资源）</li>)}
            </ul>
          )}
        </Card>

        {hasRecovery && (
          <Card className={span.d12}>
            <CardHeader number={10} title="下发恢复与必需字段" />
            <Msg note={note} />
        {blocked && (
          <div className="note crit">
            <strong>本控制面没有向网关下发配置：该环境归另一个控制面的数据库所有。</strong>
            <p>
              持有该环境的数据库 <code>{blocked.owner_database_id ?? "未知"}</code>
              ，本控制面的数据库 <code>{blocked.database_id}</code>。
            </p>
            {isAdmin &&
              (blocked.owner_database_id ? (
                <TwoStep
                  label="接管该环境"
                  disabled={busy}
                  busy={acting === "take-over"}
                  warning={
                    `将把该环境从数据库 ${blocked.owner_database_id} 接管到本控制面的数据库 ${blocked.database_id}。` +
                    "请先停止旧控制面的所有实例。此操作会记入审计日志。"
                  }
                  onConfirm={() => void takeOver(blocked.owner_database_id!)}
                />
              ) : (
                <p className="hint">kine 中没有记录持有该环境的数据库，暂时无法接管。</p>
              ))}
          </div>
        )}

        {s?.projection_stalled && (
          <div className="note crit">
            <strong>配置下发已停止。</strong>
            {s.projection_stalled.kind && (
              <p>
                停在 <code>{s.projection_stalled.kind}</code> <code>{s.projection_stalled.id}</code>
              </p>
            )}
            <p>{s.projection_stalled.error ?? "未知错误"}</p>
          </div>
        )}

        {s?.reconcile_blocked && (
          <div className="note crit">
            <strong>kine 中有大量控制面不认识的键，已暂停删除</strong>
            {isAdmin && (
              <div>
                <TwoStep
                  label="允许清理"
                  disabled={busy}
                  busy={acting === "clean"}
                  warning="将删除 kine 中所有控制面不认识的键，删除后无法恢复。"
                  onConfirm={() => void allowDeletes()}
                />
              </div>
            )}
          </div>
        )}

        {s && s.unsafe.length > 0 && (
          <div className="note crit">
            <strong>该网关忽略了必须执行的字段，请升级</strong>
            <ul>
              {s.unsafe.map((u) => (
                <li key={`${u.instance_id}/${u.kind}/${u.field}`}>
                  <code>{u.hostname || u.instance_id}</code>：<code>{u.kind}</code> 的{" "}
                  <code>{u.field}</code>
                </li>
              ))}
            </ul>
          </div>
        )}
          </Card>
        )}
      </BentoGrid>
    </>
  );
}
