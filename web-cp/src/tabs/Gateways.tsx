import "./operations.css";
import "./Gateways.css";
import { useRecordTable } from "../lib/recordTable";
import { RecordTools, RecordPager } from "../components/RecordTools";
import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../App";
import { Field, useRefusalFocus } from "../ui/Field";
import { Msg, type Note } from "../components/Msg";
import {
  ApiError,
  gateways,
  type Credential,
  type Detail,
  type Instance,
  type IssuedCredential,
} from "../lib/api";
import { problemAt } from "../lib/errors";
import { errorText, fmtInt, fmtTime } from "../lib/fmt";
import { Button } from "../ui/Button";
import { FormDialog } from "../ui/FormDialog";
import { CopyButton } from "../ui/CopyButton";
import { OperationalTable as DataTable, Loading } from "../components/OperationalTable";
import { EmptyState } from "../ui/EmptyState";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";
import { Card, CardHeader, PageHeader, Stat, StatGrid } from "../components/page";
import { Archive, Plus, Trash2 } from "lucide-react";
import { Select } from "../ui/Select";

/** How often the instances are read again: about one gateway heartbeat. */
const REFRESH_MS = 5_000;

/**
 * The bundle's `env` as `KEY=value` lines, values as issued. A value
 * spanning lines (the PEMs) is put in single quotes, so where it ends stays
 * readable; a PEM holds no single quote.
 */
function envLines(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([k, v]) => (v.includes("\n") ? `${k}='${v}'` : `${k}=${v}`))
    .join("\n");
}

/**
 * Gateway instances as their heartbeats report them, and the credentials
 * gateways boot from. Issuing and revoking are an admin's.
 */
export function Gateways({ operator }: TabProps) {
  const isAdmin = operator.role === "admin";
  const [instances, setInstances] = useState<Instance[] | null>(null);
  const [instancesError, setInstancesError] = useState<string | null>(null);
  const [credentials, setCredentials] = useState<Credential[] | null>(null);
  const [credentialsError, setCredentialsError] = useState<string | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);
  // The credential whose revoke is in flight.
  const [revoking, setRevoking] = useState<string | null>(null);
  // Held in this component's state only: it is gone when the tab is left or
  // the page reloads, and is never written to storage, the URL or a log.
  const [bundle, setBundle] = useState<IssuedCredential | null>(null);
  const [issuing, setIssuing] = useState(false);
  const [retireTarget, setRetireTarget] = useState<Instance | null>(null);
  const [retireConfirmed, setRetireConfirmed] = useState(false);
  const [retireBusy, setRetireBusy] = useState(false);
  const [retireError, setRetireError] = useState<string | null>(null);
  const [retireNote, setRetireNote] = useState<Note | null>(null);

  const readCredentials = useCallback(async () => {
    try {
      setCredentials(await gateways.credentials());
      setCredentialsError(null);
    } catch (e) {
      setCredentialsError(errorText(e));
    }
  }, []);

  useEffect(() => {
    let live = true;
    let timer: number | undefined;
    const tick = async () => {
      try {
        const items = await gateways.instances();
        if (!live) return;
        setInstances(items);
        setInstancesError(null);
      } catch (e) {
        if (live) setInstancesError(errorText(e));
      }
      if (live) timer = window.setTimeout(() => void tick(), REFRESH_MS);
    };
    void tick();
    void readCredentials();
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [readCredentials]);

  async function revoke(c: Credential) {
    setBusy(true);
    setRevoking(c.dp_id);
    setNote(null);
    try {
      const view = await gateways.revoke(c.dp_id);
      setCredentials((list) => list?.map((x) => (x.dp_id === view.dp_id ? view : x)) ?? null);
      setNote({ kind: "ok", text: `已吊销「${view.name}」。` });
    } catch (e) {
      setNote({ kind: "crit", text: errorText(e) });
    } finally {
      setBusy(false);
      setRevoking(null);
    }
  }

  async function retire() {
    if (!retireTarget || !retireConfirmed || retireTarget.online) return;
    setRetireBusy(true);
    setRetireError(null);
    setRetireNote(null);
    try {
      await gateways.retire(retireTarget.dp_id, retireTarget.instance_id);
      setRetireNote({ kind: "ok", text: `已退役实例「${retireTarget.hostname || retireTarget.instance_id}」，历史记录已保留。` });
      setRetireTarget(null);
      try {
        setInstances(await gateways.instances());
        setInstancesError(null);
      } catch (e) {
        setInstancesError(errorText(e));
      }
    } catch (e) {
      setRetireError(errorText(e));
    } finally {
      setRetireBusy(false);
    }
  }

  const [onlineFilter, setOnlineFilter] = useState("");
  const [versionFilter, setVersionFilter] = useState("");
  // 实例接口按 first_seen_at 升序返回，反转后按首次注册时间倒序展示，不以心跳替代。
  const newestInstances = [...(instances ?? [])].reverse();
  const newestCredentials = [...(credentials ?? [])].sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at));
  const table = useRecordTable({ items: newestInstances.filter((i) => (!onlineFilter || (i.online ? "online" : "offline") === onlineFilter) && (!versionFilter || (i.version_matches_cp === null ? "unknown" : i.version_matches_cp ? "match" : "mismatch") === versionFilter)), searchText: (i) => [i.instance_id, i.hostname, i.credential_name, i.version], resetKey: `${onlineFilter}:${versionFilter}` });
  const credentialTable = useRecordTable({ items: newestCredentials, searchText: (c) => [c.dp_id, c.name, c.revoked_at ? "已吊销" : "未吊销"] });
  const online = instances?.filter((i) => i.online).length ?? 0;
  const skewed = instances?.filter((i) => i.version_matches_cp === false).length ?? 0;
  const valid = credentials?.filter((c) => !c.revoked_at).length ?? 0;
  const revoked = (credentials?.length ?? 0) - valid;

  return (
    <div className="cp-operations cp-gateways">
      <PageHeader
        title="网关"
        dataLead={instances ? <>已记录 {fmtInt(instances.length)} 个网关实例，其中 {fmtInt(online)} 个在线、{fmtInt(instances.length - online)} 个离线。</> : "查看网关的当前心跳、版本与连接凭据。"}
        actions={
          isAdmin && (
            <Button kind="primary" iconOnly aria-label="签发网关凭据" title="签发网关凭据" onClick={() => setIssuing(true)}>
              <Plus aria-hidden />
            </Button>
          )
        }
      />

      <StatGrid className="gateways-summary" aria-label="网关概况">
        <Stat label="已记录实例" value={instances ? fmtInt(instances.length) : "—"} foot="每个发过心跳的网关进程一行" />
        <Stat label="在线实例" value={instances ? fmtInt(online) : "—"} foot={instances ? `${fmtInt(instances.length - online)} 个离线实例` : "等待网关心跳"} />
        <Stat label="有效网关凭据" value={credentials ? fmtInt(valid) : "—"} foot={credentials ? `共签发 ${fmtInt(credentials.length)} 份，已吊销 ${fmtInt(revoked)} 份` : "凭据签发记录由控制面提供"} />
        <Stat label="版本与控制面不一致" value={instances ? fmtInt(skewed) : "—"} foot={instances ? `${fmtInt(instances.filter((i) => i.version_matches_cp === null).length)} 个实例版本无法比较` : "等待实例版本信息"} />
      </StatGrid>

      {bundle && <Bundle bundle={bundle} onClose={() => setBundle(null)} />}

      <Card className="gateways-list" aria-label="网关凭据">
        <CardHeader
          as="h2"
          title={
            <span className="inline-flex items-center gap-2">
              网关凭据
              {credentials && credentials.length > 0 && <span className="tag">{fmtInt(credentials.length)}</span>}
            </span>
          }
          description="按签发时间倒序。网关凭据是控制面签发的客户端证书，网关凭它连接控制面。"
        />
        {isAdmin && <FormDialog open={issuing} onOpenChange={setIssuing} title="签发网关凭据" description="签发用于连接控制面的客户端证书，不会改变现有网关。">
          <IssueForm
            onCancel={() => setIssuing(false)}
            onIssued={(b) => {
              setIssuing(false);
              setNote(null);
              setBundle(b);
              void readCredentials();
            }}
          />
        </FormDialog>}
        <Msg note={note} />
        {credentialsError && (
          <div className="note crit" role="alert">
            读取失败：{credentialsError}
          </div>
        )}
        {credentials === null ? (
          !credentialsError && <Loading />
        ) : credentials.length === 0 ? (
          <EmptyState mark="gateway" text="还没有签发过网关凭据。" />
        ) : (
          <>
            <RecordTools table={credentialTable} label="网关凭据" />
            <DataTable label="网关凭据">
              <thead>
                <tr>
                  <th>名称 / ID</th>
                  <th>状态</th>
                  <th>签发时间</th>
                  <th>有效期至</th>
                  <th><span className="sr-only">操作</span></th>
                </tr>
              </thead>
              <tbody>
                {credentialTable.rows.map((c) => (
                  <tr key={c.dp_id}>
                    <td>
                      <strong className="ops-cell-main">{c.name}</strong>
                      <code className="ops-cell-secondary" title={c.dp_id}>{c.dp_id}</code>
                    </td>
                    <td>
                      {c.revoked_at ? (
                        <><Lamp state="disabled">已吊销</Lamp><span className="ops-cell-secondary">{fmtTime(c.revoked_at)}</span></>
                      ) : <Lamp state="applied">未吊销</Lamp>}
                    </td>
                    <td className="gateway-time-column text-fg-3"><time dateTime={c.created_at}>{fmtTime(c.created_at)}</time></td>
                    <td className="text-fg-3">{fmtTime(c.not_after)}</td>
                    <td className="right">
                      {isAdmin && !c.revoked_at && (
                        <TwoStep
                          label="吊销"
                          kind="quiet"
                          icon={<Trash2 aria-hidden="true" />}
                          size="sm"
                          disabled={busy}
                          busy={revoking === c.dp_id}
                          warning={`吊销网关凭据「${c.name}」后，使用这份凭据的网关将被控制面拒绝连接，且无法恢复。`}
                          onConfirm={() => void revoke(c)}
                        />
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </DataTable>
            <RecordPager table={credentialTable} label="网关凭据" />
          </>
        )}
      </Card>
      <Card className="gateways-list" aria-label="网关实例">
        <CardHeader
          as="h2"
          title="网关实例"
          description="按首次注册时间倒序，每个发过心跳的网关进程一行。超过控制面设定的时间没有心跳即为离线。"
        />
        <Msg note={retireNote} />
        {isAdmin && <FormDialog open={retireTarget !== null} onOpenChange={(open) => { if (!open) setRetireTarget(null); }} title="退役网关实例" description="退役只适用于已停止且不会重新启用的离线实例，不会停止网关进程。" busy={retireBusy}>
          <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); void retire(); }}>
            <p>实例：<strong>{retireTarget?.hostname || retireTarget?.instance_id}</strong>（<code>{retireTarget?.instance_id}</code>）</p>
            <p className="hint">退役后，该实例不再阻挡固定 Token 计数配置；版本、最近心跳等历史记录保留。若再次收到心跳，退役状态会自动清除，该实例会重新参与版本检查。</p>
            <label className="flex items-start gap-2"><input type="checkbox" checked={retireConfirmed} disabled={retireBusy} onChange={(event) => setRetireConfirmed(event.target.checked)} /><span>我确认已停止此实例，且不会重新启用它。</span></label>
            {retireError && <div className="note crit" role="alert">无法退役：{retireError}</div>}
            <div className="flex justify-end gap-2"><Button type="button" disabled={retireBusy} onClick={() => setRetireTarget(null)}>取消</Button><Button type="submit" kind="primary" disabled={!retireConfirmed} busy={retireBusy} busyLabel="正在退役">确认退役</Button></div>
          </form>
        </FormDialog>}
        {instancesError && (
          <div className="note crit" role="alert">
            读取失败：{instancesError}
          </div>
        )}
        {instances === null ? (
          !instancesError && <Loading />
        ) : instances.length === 0 ? (
          <EmptyState mark="gateway" text="还没有网关发来心跳。" />
        ) : (
          <>
            <RecordTools table={table} label="网关实例"><label className="f filter"><span>在线状态</span><Select size="compact" aria-label="在线状态" value={onlineFilter} onValueChange={(value) => setOnlineFilter(value)}><option value="">全部</option><option value="online">在线</option><option value="offline">离线</option></Select></label><label className="f filter"><span>版本状态</span><Select size="compact" aria-label="版本状态" value={versionFilter} onValueChange={(value) => setVersionFilter(value)}><option value="">全部</option><option value="match">一致</option><option value="mismatch">不一致</option><option value="unknown">无法比较</option></Select></label></RecordTools>
            <DataTable label="网关实例">
              <thead>
                <tr>
                  <th>凭据</th>
                  <th>主机</th>
                  <th>版本</th>
                  <th className="right">已应用修订</th>
                  <th>最近心跳</th>
                  <th>状态</th>
                  <th><span className="sr-only">操作</span></th>
                </tr>
              </thead>
              <tbody>
                {table.rows.map((i) => (
                  <tr key={`${i.dp_id}/${i.instance_id}`}>
                    <td className="font-medium">{i.credential_name}</td>
                    <td>
                      <code>{i.hostname}</code>
                    </td>
                    <td>
                      <code>{i.version}</code>
                      {i.version_matches_cp === false && (
                        <span className="cell-note">
                          <Lamp state="rejected">版本与控制面不一致</Lamp>
                        </span>
                      )}
                    </td>
                    <td className="right num">{i.applied_revision}</td>
                    <td className="text-fg-3">{fmtTime(i.last_seen_at)}</td>
                    <td>
                      <Lamp state={i.online ? "applied" : "disabled"}>{i.retired_at ? "已退役" : i.online ? "在线" : "离线"}</Lamp>
                    </td>
                    <td className="right">
                      {isAdmin && !i.online && !i.retired_at && <Button type="button" kind="quiet" size="sm" iconOnly aria-label="退役" title="退役" disabled={retireBusy} onClick={() => { setRetireTarget(i); setRetireConfirmed(false); setRetireError(null); }}><Archive aria-hidden /></Button>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </DataTable>
            <RecordPager table={table} label="网关实例" />
          </>
        )}
      </Card>
    </div>
  );
}

/** Refusal details shown beside their own input; the others stay in the list under the form. */
const ISSUE_FIELDS = ["/name", "/validity_days"];

/** `签发网关凭据`: the name and the validity, in the credentials card. */
function IssueForm({ onIssued, onCancel }: { onIssued: (b: IssuedCredential) => void; onCancel: () => void }) {
  const [name, setName] = useState("");
  const [days, setDays] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [details, setDetails] = useState<Detail[]>([]);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    setDetails([]);
    try {
      const issued = await gateways.issue(name.trim(), days.trim() === "" ? undefined : Number(days));
      setName("");
      setDays("");
      onIssued(issued);
    } catch (e) {
      setErr(errorText(e));
      if (e instanceof ApiError) setDetails(e.details);
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  const at = (path: string) => problemAt(details, (p) => p === path);
  const listed = details.filter((d) => !ISSUE_FIELDS.includes(d.path));

  return (
    <form ref={form} onSubmit={submit} className="gateway-issue-form">
      <div className="gateway-issue-fields">
        <Field label="名称" error={at("/name")}>
          {(aria) => <input required value={name} onChange={(e) => setName(e.target.value)} {...aria} />}
        </Field>
        <Field label="有效期（天）" error={at("/validity_days")}>
          {(aria) => (
            <input
              type="number"
              min={1}
              step={1}
              placeholder="留空用控制面的默认值"
              value={days}
              onChange={(e) => setDays(e.target.value)}
              {...aria}
            />
          )}
        </Field>
      </div>
      {err && (
        <div className="note crit mt-0" role="alert">
          {err}
          {listed.length > 0 && (
            <ul>
              {listed.map((d) => (
                <li key={d.path}>
                  <code>{d.path}</code>：{d.message}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
      <div className="flex justify-end gap-2">
        <Button type="button" size="sm" disabled={busy} onClick={onCancel}>
          取消
        </Button>
        <Button kind="primary" type="submit" size="sm" busy={busy} busyLabel="签发中…">
          签发
        </Button>
      </div>
    </form>
  );
}

/** A bundle part: one line per line as issued, scrolled sideways rather than broken mid-line. */
const PRE = "whitespace-pre break-normal text-caption leading-relaxed";

/** The deployment bundle, shown once: the answer to the issue is its only copy. */
function Bundle({ bundle, onClose }: { bundle: IssuedCredential; onClose: () => void }) {
  const [copied, setCopied] = useState<Note | null>(null);
  const lines = envLines(bundle.env);

  const copyFailed = () => setCopied({ kind: "crit", text: "浏览器不允许复制，请手动选中上面的内容复制。" });

  return (
    <Card className="bundle gateway-bundle" aria-label="部署包">
      <CardHeader title="一次性部署包" />
      <div className="note warn mt-0">
        <p>
          <strong>部署包「{bundle.name}」：私钥只显示这一次</strong>
          。离开本页或刷新后无法再次查看，请立即保存。
        </p>
        <p>{bundle.note}</p>
        <p>
          多个网关副本时，租户的限流与并发计数要放进 Redis，否则每个副本各算各的：请自行加上{" "}
          <code>AISIX_RATELIMIT__BACKEND=redis</code> 和 <code>AISIX_RATELIMIT__REDIS__URL</code>
          （Redis 地址）。部署包不含这两项，单副本可以不加。
        </p>
      </div>
      <div className="gateway-bundle-fields">
        <div className="min-w-0">
          <div className="mb-2 flex flex-wrap items-end justify-between gap-3">
            <div className="min-w-0">
              <h3 className="m-0 text-body font-semibold">环境变量</h3>
              <p className="hint mt-1 text-caption">官方镜像以这些变量启动为托管模式的网关。多行的值用单引号括起。</p>
            </div>
            <CopyButton text={lines} label="复制全部" size="sm" onFailed={copyFailed} />
          </div>
          <pre className={`${PRE} gateway-bundle-env`}>{lines}</pre>
          <Msg note={copied} />
        </div>
        <div className="grid min-w-0 gap-4">
          <div className="min-w-0">
            <h3 className="mt-0 mb-2 text-body font-semibold">证书</h3>
            <pre className={PRE}>{bundle.cert_pem}</pre>
          </div>
          <div className="min-w-0">
            <h3 className="mt-0 mb-2 text-body font-semibold">私钥</h3>
            <pre className={PRE}>{bundle.key_pem}</pre>
          </div>
          <div className="min-w-0">
            <h3 className="mt-0 mb-2 text-body font-semibold">CA 证书</h3>
            <pre className={PRE}>{bundle.ca_pem}</pre>
          </div>
        </div>
      </div>
      <div className="gateway-bundle-footer">
        <Button type="button" onClick={onClose}>
          我已保存，关闭
        </Button>
      </div>
    </Card>
  );
}
