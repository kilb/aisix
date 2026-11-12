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
import { CopyButton } from "../ui/CopyButton";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";
import { Card, CardHeader, PageHeader, Split } from "../components/page";
import { Plus } from "lucide-react";
import { InventorySummary } from "./providers/OperationsBento";

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

  const online = instances?.filter((i) => i.online).length ?? 0;
  const skewed = instances?.filter((i) => i.version_matches_cp === false).length ?? 0;
  const valid = credentials?.filter((c) => !c.revoked_at).length ?? 0;
  const revoked = (credentials?.length ?? 0) - valid;

  return (
    <>
      <PageHeader
        eyebrow="GATEWAYS · 实例与凭据"
        title="网关"
        dataLead={instances ? <>已记录 {fmtInt(instances.length)} 个网关实例，其中 {fmtInt(online)} 个在线、{fmtInt(instances.length - online)} 个离线。</> : "查看网关的当前心跳、版本与连接凭据。"}
        actions={
          isAdmin &&
          !issuing && (
            <Button kind="primary" onClick={() => setIssuing(true)}>
              <Plus aria-hidden />
              签发网关凭据
            </Button>
          )
        }
      />

      <InventorySummary
        title="网关实例 · 当前心跳"
        total={instances?.length ?? null}
        totalLabel="已记录实例"
        breakdown={instances ? [
          { label: "在线", value: online, displayValue: fmtInt(online) },
          { label: "离线", value: instances.length - online, displayValue: fmtInt(instances.length - online) },
        ] : []}
        breakdownLabel="已记录实例的在线状态"
        secondaryTitle="未吊销的网关凭据"
        secondaryValue={credentials ? valid : null}
        secondaryFoot={credentials ? `共签发 ${fmtInt(credentials.length)} 份，已吊销 ${fmtInt(revoked)} 份。` : "凭据签发记录由控制面提供。"}
        error={instancesError !== null}
        secondaryError={credentialsError !== null}
      >
        <dl className="ops-facts">
          <div><dt>版本与控制面不一致</dt><dd>{instances ? fmtInt(skewed) : "—"}</dd></div>
          <div><dt>版本无法比较</dt><dd>{instances ? fmtInt(instances.filter((i) => i.version_matches_cp === null).length) : "—"}</dd></div>
        </dl>
      </InventorySummary>

      {bundle && <Bundle bundle={bundle} onClose={() => setBundle(null)} />}

      <Split
        asideWidth="minmax(380px, 34%)"
        main={
          <Card className="mb-0">
            <CardHeader
              number={bundle ? 4 : 3}
              as="h2"
              title="网关实例"
              description="每个发过心跳的网关进程一行。超过控制面设定的时间没有心跳即为离线。"
            />
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
              <DataTable label="网关实例">
                <thead>
                  <tr>
                    <th>凭据</th>
                    <th>主机</th>
                    <th>版本</th>
                    <th className="right">已应用修订</th>
                    <th>最近心跳</th>
                    <th>状态</th>
                  </tr>
                </thead>
                <tbody>
                  {instances.map((i) => (
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
                      <td className="text-muted">{fmtTime(i.last_seen_at)}</td>
                      <td>
                        <Lamp state={i.online ? "applied" : "disabled"}>{i.online ? "在线" : "离线"}</Lamp>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </DataTable>
            )}
          </Card>
        }
        aside={
          <Card className="mb-0">
            <CardHeader
              number={bundle ? 5 : 4}
              as="h2"
              title={
                <span className="inline-flex items-center gap-2">
                  网关凭据
                  {credentials && credentials.length > 0 && <span className="tag">{fmtInt(credentials.length)}</span>}
                </span>
              }
              description="网关凭据是控制面签发的客户端证书，网关凭它连接控制面。"
            />
            {isAdmin && issuing && (
              <IssueForm
                onCancel={() => setIssuing(false)}
                onIssued={(b) => {
                  setIssuing(false);
                  setNote(null);
                  setBundle(b);
                  void readCredentials();
                }}
              />
            )}
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
              <ul className="-mx-[var(--pad)] -mb-[var(--pad)] mt-4 divide-y divide-border border-t border-line">
                {credentials.map((c) => (
                  <li key={c.dp_id} className="flex flex-col gap-1.5 px-[var(--pad)] py-3.5">
                    <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-3 gap-y-1">
                      <strong className="min-w-0 font-medium break-all">{c.name}</strong>
                      {c.revoked_at ? (
                        <Lamp state="disabled">已吊销 · {fmtTime(c.revoked_at)}</Lamp>
                      ) : (
                        <Lamp state="applied">未吊销</Lamp>
                      )}
                    </div>
                    <code className="truncate text-caption text-muted" title={c.dp_id}>
                      {c.dp_id}
                    </code>
                    <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-2">
                      <span className="hint pt-1.5 text-caption">有效期至 {fmtTime(c.not_after)}</span>
                      {isAdmin && !c.revoked_at && (
                        <span className="right ml-auto min-w-0 max-w-full has-[.is-asking]:basis-full [&_.two-step]:max-w-full [&_.two-step-warn]:w-auto [&_.two-step-warn]:max-w-full">
                          <TwoStep
                            label="吊销"
                            size="sm"
                            disabled={busy}
                            busy={revoking === c.dp_id}
                            warning={`吊销网关凭据「${c.name}」后，使用这份凭据的网关将被控制面拒绝连接，且无法恢复。`}
                            onConfirm={() => void revoke(c)}
                          />
                        </span>
                      )}
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </Card>
        }
      />
    </>
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
    <form ref={form} onSubmit={submit} className="mt-2 rounded-cell border border-line bg-well p-4">
      <h3 className="mb-3 text-body font-semibold">签发网关凭据</h3>
      <div className="grid gap-x-4 tablet:grid-cols-2 desktop:grid-cols-1">
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
    <Card className="bundle mb-10" aria-label="部署包">
      <CardHeader number={3} title="一次性部署包" />
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
      <div className="grid items-start gap-3 desktop:grid-cols-2">
        <div className="min-w-0">
          <div className="mb-2 flex flex-wrap items-end justify-between gap-3">
            <div className="min-w-0">
              <h3 className="m-0 text-body font-semibold">环境变量</h3>
              <p className="hint mt-1 text-caption">官方镜像以这些变量启动为托管模式的网关。多行的值用单引号括起。</p>
            </div>
            <CopyButton text={lines} label="复制全部" kind="primary" size="sm" onFailed={copyFailed} />
          </div>
          <pre className={`${PRE} max-h-[640px]`}>{lines}</pre>
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
      <div className="-mx-[var(--pad)] -mb-[var(--pad)] mt-6 flex flex-wrap justify-end gap-2 rounded-b-card border-t border-line bg-well px-[var(--pad)] py-3 [&_.hint]:text-ink-soft">
        <Button type="button" onClick={onClose}>
          我已保存，关闭
        </Button>
      </div>
    </Card>
  );
}
