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
import { errorText, fmtTime } from "../lib/fmt";
import { Button } from "../ui/Button";
import { CopyButton } from "../ui/CopyButton";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";

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
  // Held in this component's state only: it is gone when the tab is left or
  // the page reloads, and is never written to storage, the URL or a log.
  const [bundle, setBundle] = useState<IssuedCredential | null>(null);

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
    setNote(null);
    try {
      const view = await gateways.revoke(c.dp_id);
      setCredentials((list) => list?.map((x) => (x.dp_id === view.dp_id ? view : x)) ?? null);
      setNote({ kind: "ok", text: `已吊销「${view.name}」。` });
    } catch (e) {
      setNote({ kind: "crit", text: errorText(e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="panel">
        <h2>网关实例</h2>
        <p className="hint">每个发过心跳的网关进程一行。超过控制面设定的时间没有心跳即为离线。</p>
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
          <DataTable>
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
                  <td>{i.credential_name}</td>
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
                  <td>{fmtTime(i.last_seen_at)}</td>
                  <td>
                    <Lamp state={i.online ? "applied" : "disabled"}>{i.online ? "在线" : "离线"}</Lamp>
                  </td>
                </tr>
              ))}
            </tbody>
          </DataTable>
        )}
      </div>

      <div className="panel">
        <h2>网关凭据</h2>
        <p className="hint">网关凭据是控制面签发的客户端证书，网关凭它连接控制面。</p>
        {isAdmin && (
          <IssueForm
            onIssued={(b) => {
              setNote(null);
              setBundle(b);
              void readCredentials();
            }}
          />
        )}
        {bundle && <Bundle bundle={bundle} onClose={() => setBundle(null)} />}
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
          <ul className="creds">
            {credentials.map((c) => (
              <li key={c.dp_id}>
                <strong>{c.name}</strong>
                <code>{c.dp_id}</code>
                <span className="hint">有效期至 {fmtTime(c.not_after)}</span>
                {c.revoked_at ? (
                  <Lamp state="disabled">已吊销 · {fmtTime(c.revoked_at)}</Lamp>
                ) : (
                  <Lamp state="applied">未吊销</Lamp>
                )}
                {isAdmin && !c.revoked_at && (
                  <TwoStep
                    label="吊销"
                    size="sm"
                    disabled={busy}
                    warning={`吊销网关凭据「${c.name}」后，使用这份凭据的网关将被控制面拒绝连接，且无法恢复。`}
                    onConfirm={() => void revoke(c)}
                  />
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    </>
  );
}

/** Refusal details shown beside their own input; the others stay in the list under the form. */
const ISSUE_FIELDS = ["/name", "/validity_days"];

/** `签发网关凭据`: a toggle that opens the form in its place. */
function IssueForm({ onIssued }: { onIssued: (b: IssuedCredential) => void }) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [days, setDays] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [details, setDetails] = useState<Detail[]>([]);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  if (!open) {
    return (
      <Button kind="primary" onClick={() => setOpen(true)}>
        签发网关凭据
      </Button>
    );
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    setDetails([]);
    try {
      const issued = await gateways.issue(name.trim(), days.trim() === "" ? undefined : Number(days));
      setOpen(false);
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
    <form ref={form} onSubmit={submit}>
      <div className="r">
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
      <p>
        <Button kind="primary" type="submit" busy={busy} busyLabel="签发中…">
          签发
        </Button>{" "}
        <Button type="button" disabled={busy} onClick={() => setOpen(false)}>
          取消
        </Button>
      </p>
      {err && (
        <div className="note crit" role="alert">
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
    </form>
  );
}

/** The deployment bundle, shown once: the answer to the issue is its only copy. */
function Bundle({ bundle, onClose }: { bundle: IssuedCredential; onClose: () => void }) {
  const [copied, setCopied] = useState<Note | null>(null);
  const lines = envLines(bundle.env);

  const copyFailed = () => setCopied({ kind: "crit", text: "浏览器不允许复制，请手动选中上面的内容复制。" });

  return (
    <section className="note warn bundle" aria-label="部署包">
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
      <h3>环境变量</h3>
      <p className="hint">官方镜像以这些变量启动为托管模式的网关。多行的值用单引号括起。</p>
      <pre>{lines}</pre>
      <p>
        <CopyButton text={lines} label="复制全部" kind="primary" onFailed={copyFailed} />
      </p>
      <Msg note={copied} />
      <h3>证书</h3>
      <pre>{bundle.cert_pem}</pre>
      <h3>私钥</h3>
      <pre>{bundle.key_pem}</pre>
      <h3>CA 证书</h3>
      <pre>{bundle.ca_pem}</pre>
      <p>
        <Button type="button" onClick={onClose}>
          我已保存，关闭
        </Button>
      </p>
    </section>
  );
}
