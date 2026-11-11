import { useEffect, useState, type ReactNode } from "react";
import {
  ApiError,
  MIN_PASSWORD_CHARS,
  changePassword,
  org as readOrg,
  type MemberRole,
  type Org as OrgView,
  type SessionMember,
  type TenantStatus,
} from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";
import { Button } from "./ui/Button";
import { Field } from "./ui/Field";
import { Lamp, type LampState } from "./ui/Lamp";
import { Card, CardHeader, PageHeader, Split, Stat, StatGrid } from "./components/page";

export const ROLE_LABEL: Record<MemberRole, string> = {
  owner: "所有者",
  admin: "管理员",
  member: "成员",
  billing: "账单",
};

const STATUS_LABEL: Record<TenantStatus, string> = {
  pending: "等待审核",
  active: "已启用",
  suspended: "已停用",
};

const STATUS_LAMP: Record<TenantStatus, LampState> = {
  pending: "pending",
  active: "applied",
  suspended: "disabled",
};

/** A cap a key may be given, or none. */
function cap(value: number | undefined): string {
  return value === undefined ? "不限" : value.toLocaleString("en-US");
}

/** One reading of the organisation: its label over its value, a cell of the card's grid. */
function Reading({ label, value, title }: { label: string; value: ReactNode; title?: string }) {
  return (
    <div className="entry flex min-w-0 flex-col gap-1.5 px-6 py-5">
      <span className="lab">{label}</span>
      <span className="val text-[15px]" title={title}>
        {value}
      </span>
    </div>
  );
}

/**
 * The organisation: its name, its state, the caps its keys may be given,
 * the caller's role, and the caller's own password. It is the whole portal
 * while the organisation is pending or suspended: the control plane then
 * serves its members `GET /api/org` and `POST /api/password` only.
 */
export function Org({ member, status }: { member: SessionMember; status: TenantStatus }) {
  const [org, setOrg] = useState<OrgView | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    readOrg().then(
      (o) => live && setOrg(o),
      (e: unknown) => live && setErr(e instanceof ApiError ? messageFor(e) : String(e)),
    );
    return () => {
      live = false;
    };
  }, []);

  const shown = org?.status ?? status;
  return (
    <>
      <PageHeader title="组织" />
      <Split
        asideWidth="400px"
        main={
          <div className="grid gap-6">
            {status === "pending" && (
              <div className="note warn">组织正在等待平台审核，审核通过后即可创建密钥和邀请成员</div>
            )}
            {status === "suspended" && <div className="note crit">组织已被停用，请联系平台</div>}
            {err && (
              <div className="note crit" role="alert">
                {err}
              </div>
            )}
            {/* Who the organisation is, and the caller in it: one row of readings. */}
            <Card className="p-0">
              <div className="entries grid divide-border max-sm:divide-y sm:grid-cols-3 sm:divide-x">
                <Reading label="名称" value={org?.name ?? "—"} title={org?.name} />
                <Reading
                  label="状态"
                  value={STATUS_LAMP[shown] ? <Lamp state={STATUS_LAMP[shown]}>{STATUS_LABEL[shown]}</Lamp> : shown}
                />
                <Reading label="你的角色" value={ROLE_LABEL[member.role] ?? member.role} />
              </div>
            </Card>
            {/* The caps a key may be given: the figures the key form is held to. */}
            <StatGrid className="mb-0 xl:grid-cols-2">
              <Stat label="单个密钥的并发上限" value={org ? cap(org.limits.key_concurrency_max) : "—"} />
              <Stat label="单个密钥的每分钟请求上限" value={org ? cap(org.limits.key_rpm_max) : "—"} />
            </StatGrid>
          </div>
        }
        aside={<PasswordForm />}
      />
    </>
  );
}

/** The caller's own password. A change ends their other sessions. */
function PasswordForm() {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setMsg(null);
    if ([...next].length < MIN_PASSWORD_CHARS) {
      setMsg({ ok: false, text: `新口令至少需要 ${MIN_PASSWORD_CHARS} 个字符` });
      return;
    }
    setBusy(true);
    try {
      await changePassword(current, next);
      setCurrent("");
      setNext("");
      setMsg({ ok: true, text: "口令已修改，其他设备上的登录已退出" });
    } catch (e) {
      const text =
        e instanceof ApiError
          ? e.code === "invalid_credentials"
            ? "当前口令不正确"
            : formMessageFor(e)
          : String(e);
      setMsg({ ok: false, text });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader title="修改口令" />
      <form className="grid gap-4" onSubmit={submit}>
        <Field label="当前口令">
          <input
            type="password"
            autoComplete="current-password"
            required
            value={current}
            onChange={(e) => setCurrent(e.target.value)}
          />
        </Field>
        <Field label="新口令" hint={<>至少 {MIN_PASSWORD_CHARS} 个字符</>}>
          {(aria) => (
            <input
              type="password"
              autoComplete="new-password"
              required
              value={next}
              onChange={(e) => setNext(e.target.value)}
              {...aria}
            />
          )}
        </Field>
        <div className="flex justify-end">
          <Button kind="primary" type="submit" busy={busy}>
            修改口令
          </Button>
        </div>
      </form>
      {msg &&
        (msg.ok ? (
          <div className="note ok" role="status">
            {msg.text}
          </div>
        ) : (
          <div className="note crit" role="alert">
            {msg.text}
          </div>
        ))}
    </Card>
  );
}
