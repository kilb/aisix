import { useCallback, useEffect, useState } from "react";
import type { TabId, TabProps } from "../App";
import { Msg, type Note } from "../components/Msg";
import { environment, pricingStatus, rollout, type Environment, type PricingStatus, type RolloutSummary } from "../lib/api";
import { errorText, fmtInt } from "../lib/fmt";
import { statusView } from "../lib/pricingStatus";
import { Button } from "../ui/Button";
import { CardHeader, PageHeader } from "../components/page";
import { Loading } from "../ui/DataTable";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";

/** How often the summary is read again: about one gateway heartbeat. */
const REFRESH_MS = 5_000;
/** The pricing status costs the control plane a query over the charges, so it is read less often. */
const PRICING_REFRESH_MS = 60_000;

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
  return (
    <>
      <PageHeader
        title="运行概况"
        description={
          envError ? undefined : (
            <>
              环境 <code>{env?.env_id ?? "…"}</code> · 控制面版本 <code>{env?.cp_version ?? "…"}</code>
            </>
          )
        }
      />
      {envError && (
        <div className="note crit" role="alert">
          读取环境信息失败：{envError}
        </div>
      )}
      {error && (
        <div className="note crit" role="alert">
          读取失败：{error}
        </div>
      )}
      <div className="entries mb-6 grid gap-4 sm:grid-cols-3">
        <Reading label="在线网关" value={s ? fmtInt(s.online_instances) : "…"} foot="近期发过心跳的网关实例" />
        <Reading label="待下发" value={s ? fmtInt(s.pending) : "…"} foot="已保存、尚未写入 kine 的资源" />
        <Reading label="下发中" value={s ? fmtInt(s.propagating) : "…"} foot="已写入 kine、还有在线网关没有应用的资源" />
      </div>
      <div>
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
      </div>

      <div className="grid items-start gap-4 xl:grid-cols-3">
      <div className="panel mb-0" role="region" aria-label="计价异常">
        <CardHeader
          as="h2"
          title="计价异常"
          description="最近 24 小时没能计价的调用，以及计价引擎不能使用的价格版本。"
          actions={
            <Button
              size="sm"
              onClick={() => {
                onGoto("pricing", "status");
              }}
            >
              去「价格 · 计价状态」
            </Button>
          }
        />
        {pricingError && (
          <div className="note crit" role="alert">
            读取计价状态失败：{pricingError}
          </div>
        )}
        {pricingView && (pricingView.unpricedTotal > 0 || pricingView.invalid.length > 0) && (
          <div className="note warn" role="status">
            有调用没能计价或有价格版本不可用，请查看计价状态。
          </div>
        )}
        <div className="entries grid grid-cols-2 gap-3">
          <Reading inset label="未计价调用" value={pricingView ? fmtInt(pricingView.unpricedTotal) : "…"} foot="最近 24 小时" />
          <Reading inset label="不可用的价格版本" value={pricingView ? fmtInt(pricingView.invalid.length) : "…"} foot="计价引擎跳过的版本" />
        </div>
      </div>

      <div className="panel mb-0">
        <CardHeader as="h2" title="被拒绝的资源" description="网关加载时拒绝了这些资源，它们没有生效。原因由网关给出。" />
        {s === null ? (
          !error && <Loading />
        ) : s.rejected.length === 0 ? (
          <p className="status-line">
            <Lamp state="applied">没有被拒绝的资源。</Lamp>
          </p>
        ) : (
          <ul className="rows">
            {s.rejected.map((r) => {
              const where = TAB_OF_KIND[r.kind] ?? GENERIC;
              return (
                <li key={`${r.kind}/${r.id}`}>
                  <div className="row-head">
                    <code>{r.kind}</code> <strong>{r.name ?? r.id}</strong>{" "}
                    <Button size="sm" onClick={() => onGoto(where.tab)}>
                      去「{where.label}」
                    </Button>
                  </div>
                  {r.errors.map((e) => (
                    <div className="rollout-errors" key={e.instance_id}>
                      <code>{e.hostname || e.instance_id}</code>：{e.error}
                    </div>
                  ))}
                </li>
              );
            })}
          </ul>
        )}
      </div>

      <div className="panel mb-0">
        <CardHeader as="h2" title="部分兼容" description="网关加载了这些资源，但忽略了其中的部分字段。" />
        {s === null ? (
          !error && <Loading />
        ) : s.partially_compatible.length === 0 ? (
          <p className="status-line">
            <Lamp state="applied">所有网关都完整加载了配置。</Lamp>
          </p>
        ) : (
          <ul className="rows">
            {s.partially_compatible.map((p) => (
              <li key={`${p.instance_id}/${p.kind}/${p.field}`}>
                <code>{p.instance_id}</code>：<code>{p.kind}</code> 的 <code>{p.field}</code>（
                {fmtInt(p.count)} 个资源）
              </li>
            ))}
          </ul>
        )}
      </div>
      </div>
    </>
  );
}

/** One figure of the overview: its label, the number, and what it counts. */
function Reading({ label, value, foot, inset = false }: { label: string; value: string; foot: string; inset?: boolean }) {
  return (
    <div
      className={
        inset
          ? "entry flex flex-col rounded-xl border border-border bg-muted/50 p-4"
          : "entry flex flex-col rounded-2xl border border-border bg-card p-5 shadow-card"
      }
    >
      <span className="lab">{label}</span>
      <span className={inset ? "val !text-2xl" : "val"}>{value}</span>
      <span className="foot text-[13px] text-muted-foreground">{foot}</span>
    </div>
  );
}
