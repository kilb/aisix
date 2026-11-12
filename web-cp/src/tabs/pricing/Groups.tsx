import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import { ApiError, groups, type Detail, type Group } from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtBeijing } from "../../lib/time";
import { exactInteger, formatCount } from "../../lib/chartFormat";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useRecordTable } from "../../lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { Sheet, SheetContent } from "../../components/ui/sheet";
import { GroupDetail } from "./GroupDetail";
import { Count, FormGrid } from "./kit";

/** What the operator reads for each count a refusal carries, in the order the page shows them. */
const COUNT_WORDS: [keyof GroupUseCounts, string][] = [
  ["tenants", "租户"],
  ["defaults", "默认设置"],
  ["keys", "密钥"],
  ["provider_keys", "上游账号"],
  ["default_group", "默认分组"],
];

/** The counts a `409 group_in_use` carries. */
type GroupUseCounts = {
  tenants?: number;
  defaults?: number;
  keys?: number;
  provider_keys?: number;
  default_group?: number;
};

/**
 * The counts still naming a group, each by name: a refusal that says only
 * "in use" leaves the operator to guess which of the five to clear.
 */
export function inUseText(e: ApiError): string {
  const counts = (e.counts ?? {}) as GroupUseCounts;
  const parts = COUNT_WORDS.map(([key, word]) => `${word} ${counts[key] ?? 0}`);
  return `${messageFor(e)}：${parts.join("、")}`;
}

/** A rate typed by a person that buys nothing: `0`, however it is written. An empty field is not a rate at all. */
function isZeroRate(text: string): boolean {
  const typed = text.trim();
  return typed !== "" && /^0*(\.0*)?$/.test(typed);
}

/** A rate plan's current rate as the page shows it: its own decimal string, or 未设置. */
function currentRate(plan: Group["default_rate"]): string {
  return plan.current?.cny_per_usd ?? "未设置";
}

/** The revisions a plan has scheduled after it, as their starts and rates. */
function scheduled(plan: Group["default_rate"]): string {
  return plan.upcoming.map((r) => `${fmtBeijing(r.effective_from)} 起 ${r.cny_per_usd ?? "结束覆盖"}`).join("、");
}

/** 引用总数逐项精确累加，接口已失去精度的计数保持未知。 */
function referenceCount(rows: Group[] | null, field: "tenants" | "keys"): string {
  if (rows === null) return "—";
  let total = 0n;
  for (const group of rows) {
    const count = exactInteger(group[field]);
    if (count === null) return "—";
    total += count;
  }
  return formatCount(total);
}

/**
 * 价格 · 分组: every provider group, what still names it, and its default
 * rate. Selecting one opens its per-model rates.
 *
 * The routing name is drawn in mono and never leaves this page: it is the
 * identifier an upstream account and a key document carry, and a tenant sees
 * only the display name and the description. That is what the warning beside
 * the form is for.
 */
export function Groups({ canEdit }: TabProps) {
  const [rows, setRows] = useState<Group[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);

  const load = useCallback(async () => {
    try {
      setRows(await groups.list());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const [rateState, setRateState] = useState("");
  const records = useRecordTable({
    items: (rows ?? []).filter((group) => rateState === "" || (rateState === "configured" ? group.default_rate.current?.cny_per_usd != null : rateState === "missing" ? group.default_rate.current?.cny_per_usd == null : group.default_rate.upcoming.length > 0)),
    searchText: (group) => [group.display_name, group.name, group.id],
    resetKey: rateState,
  });
  const open = rows?.find((g) => g.id === selected) ?? null;
  if (open) {
    return <GroupDetail group={open} canEdit={canEdit} onBack={() => setSelected(null)} onChanged={load} />;
  }
  const unset = rows?.filter((g) => g.default_rate.current?.cny_per_usd == null).length ?? 0;
  const scheduledCount = rows?.reduce((total, g) => total + g.default_rate.upcoming.length, 0) ?? 0;

  return (
    <>
      <PageHeader
        title="分组"
        eyebrow="价格 / 分组"
        dataLead={rows !== null ? `已载入全部 ${fmtInt(rows.length)} 个分组，${fmtInt(unset)} 个尚未设置现行默认换算率。` : "读取分组与现行默认换算率。"}
        description="一个分组给同一批上游账号定一个换算率：模型的官方美元价 × 这个分组在模型上的换算率（没有时为分组默认），就是租户付的人民币价（租户折扣另算）。密钥在创建时选一个分组，此后只路由到该分组的上游账号。"
        actions={
          <>
            <Button type="button" icon="refresh" onClick={() => void load()}>
              刷新
            </Button>
            {canEdit && (
              <Button type="button" kind="primary" icon="plus" onClick={() => setCreating(true)}>
                新建分组
              </Button>
            )}
          </>
        }
      />
      {error && (
        <div className="note crit mt-0" role="alert">
          {error}{" "}
          <Button type="button" onClick={() => void load()}>
            重试
          </Button>
        </div>
      )}
      {refusal && (
        <div className="note crit mt-0" role="alert">
          {refusal}
        </div>
      )}
      <BentoGrid className="mb-3">
        <Card className={span.d7}>
          <CardHeader number={1} title="分组 · 全部" />
          {rows === null ? error ? <CardMessage onRetry={() => void load()}>数据读取失败</CardMessage> : <MetricSkeleton /> : <Metric size="hero">{fmtInt(rows.length)}</Metric>}
          <dl className="mt-6 grid grid-cols-2 gap-4 border-t border-ink/7 pt-4">
            <div><dt className="font-mono text-micro text-muted">租户引用</dt><dd className="mt-1 break-all text-stat tabular-nums">{referenceCount(rows, "tenants")}</dd></div>
            <div><dt className="font-mono text-micro text-muted">密钥引用</dt><dd className="mt-1 break-all text-stat tabular-nums">{referenceCount(rows, "keys")}</dd></div>
          </dl>
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="默认换算率 · 待生效修订" />
          {rows === null ? error ? <CardMessage onRetry={() => void load()}>数据读取失败</CardMessage> : <MetricSkeleton /> : <Metric size="display">{fmtInt(scheduledCount)}</Metric>}
          <p className="mt-4 text-caption text-on-inverse-muted">只统计全部分组默认换算率的预约版本；单模型覆盖在分组明细查看。</p>
          <p className="mt-6 border-t border-surface/12 pt-4 text-body">默认分组：{rows === null ? "—" : rows.find((g) => g.is_default)?.display_name ?? "未设置"}</p>
        </Card>
      </BentoGrid>
      <Card className="mb-0">
        <CardHeader number={3} className="mb-0" title={<>全部分组{rows && <Count n={rows.length} />}</>} />
        {rows === null ? (
          !error && <Loading />
        ) : rows.length === 0 ? (
          <EmptyState text="还没有分组。" />
        ) : (
          <>
          <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索分组", placeholder: "显示名、路由名或 ID" }}>
            <label className="f filter"><span>默认换算率</span><select value={rateState} onChange={(e) => setRateState(e.target.value)}><option value="">全部</option><option value="configured">已设置</option><option value="missing">未设置</option><option value="scheduled">有待生效版本</option></select></label>
          </RecordTableToolbar>
          <DataTable label="分组">
            <thead>
              <tr>
                <th>显示名</th>
                <th>路由名</th>
                <th>说明</th>
                <th className="num">默认换算率</th>
                <th className="num">租户</th>
                <th className="num">密钥</th>
                <th className="num">上游账号</th>
                <th className="right">操作</th>
              </tr>
            </thead>
            <tbody>
              {records.rows.length === 0 && <tr><td colSpan={8}><EmptyState text="没有符合筛选条件的分组" /></td></tr>}
              {records.rows.map((g) => (
                <GroupRowView
                  key={g.id}
                  group={g}
                  canEdit={canEdit}
                  onOpen={() => setSelected(g.id)}
                  onRefused={(e) => setRefusal(inUseText(e))}
                  onChanged={load}
                />
              ))}
            </tbody>
          </DataTable>
          <RecordTablePager page={records.page} pageSize={records.pageSize} onPageChange={records.setPage} onPageSizeChange={records.setPageSize} total={records.filteredCount} label="分组分页" />
          </>
        )}
      </Card>
      {canEdit && (
        <Sheet open={creating} onOpenChange={setCreating}>
          <SheetContent
            title="新建分组"
            onEscapeKeyDown={(e) => {
              // Escape inside an open confirmation takes back the confirmation, not the whole form.
              if (document.activeElement?.closest(".two-step.is-asking")) e.preventDefault();
            }}
          >
            <CreateGroup
              onCreated={async () => {
                setCreating(false);
                await load();
              }}
            />
          </SheetContent>
        </Sheet>
      )}
    </>
  );
}

/** One row of the list, with the actions that do not leave it. */
function GroupRowView({
  group,
  canEdit,
  onOpen,
  onRefused,
  onChanged,
}: {
  group: Group;
  canEdit: boolean;
  onOpen: () => void;
  onRefused: (e: ApiError) => void;
  onChanged: () => Promise<void>;
}) {
  const toast = useToast();
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);

  async function makeDefault() {
    setBusy(true);
    try {
      await groups.makeDefault(group.id);
      toast("已设为默认分组");
      await onChanged();
    } catch (e) {
      if (e instanceof ApiError) onRefused(e);
      else onRefused(new ApiError(errorText(e), 0, "internal", [], [], null));
    } finally {
      setBusy(false);
    }
  }

  async function retire() {
    setBusy(true);
    try {
      await groups.retire(group.id);
      toast("已停用");
      await onChanged();
    } catch (e) {
      if (e instanceof ApiError) onRefused(e);
      else onRefused(new ApiError(errorText(e), 0, "internal", [], [], null));
    } finally {
      setBusy(false);
    }
  }

  const later = scheduled(group.default_rate);
  return (
    <>
      <tr>
        <td>
          <button type="button" className="link-button" title={group.display_name} onClick={onOpen}>
            {group.display_name}
          </button>
          {group.is_default && (
            <span className="tag grp-default" data-default="true">
              默认分组
            </span>
          )}
        </td>
        <td>
          <span className="mono">{group.name}</span>
        </td>
        <td>
          <span className="trunc text-text-2" title={group.description}>
            {group.description || "—"}
          </span>
        </td>
        <td className="num">
          <span className="tabular">{currentRate(group.default_rate)}</span>
          {later && <span className="cell-note">待生效 {later}</span>}
        </td>
        <td className="num">{group.tenants}</td>
        <td className="num">{group.keys}</td>
        <td className="num">{group.provider_keys.length}</td>
        <td className="right">
          {canEdit && (
            <span className="row-actions">
              <Button size="sm" onClick={() => setEditing((v) => !v)} aria-expanded={editing}>
                编辑
              </Button>
              {!group.is_default && (
                <TwoStep label="设为默认分组" size="sm" busy={busy} onConfirm={() => void makeDefault()} />
              )}
              <TwoStep
                label="停用"
                size="sm"
                busy={busy}
                warning="停用后它不再出现在任何选择里；已经停用的分组不能恢复。"
                onConfirm={() => void retire()}
              />
            </span>
          )}
        </td>
      </tr>
      {editing && canEdit && (
        <tr className="dt-detail">
          <td colSpan={8} className="!px-6 !pb-4">
            <EditGroup
              group={group}
              onDone={async () => {
                setEditing(false);
                await onChanged();
              }}
              onCancel={() => setEditing(false)}
            />
          </td>
        </tr>
      )}
    </>
  );
}

/** What tenants see: the display name and the description, and nothing that routes. */
function EditGroup({
  group,
  onDone,
  onCancel,
}: {
  group: Group;
  onDone: () => Promise<void>;
  onCancel: () => void;
}) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLDivElement>();
  const [display, setDisplay] = useState(group.display_name);
  const [description, setDescription] = useState(group.description);
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);

  async function save() {
    if (saving) return;
    setSaving(true);
    setDetails([]);
    setFailure(null);
    try {
      await groups.update(group.id, { display_name: display, description });
      toast("已保存");
      await onDone();
    } catch (e) {
      if (e instanceof ApiError && e.status === 422 && e.details.length > 0) {
        setDetails(e.details);
        focusRefusal();
      } else {
        setFailure(e instanceof ApiError ? messageFor(e) : errorText(e));
      }
    } finally {
      setSaving(false);
    }
  }

  return (
    <div
      ref={form}
      className="whitespace-normal rounded-cell bg-well p-4"
      role="group"
      aria-label="编辑分组"
    >
      <p className="hint mb-4">显示名和说明对租户可见，不要写入上游渠道名称。路由名创建后不能更改。</p>
      <FormGrid>
        <Field label="显示名" error={problemAt(details, (p) => p === "/display_name")}>
          {(aria) => <input {...aria} value={display} onChange={(e) => setDisplay(e.target.value)} />}
        </Field>
        <Field label="说明" error={problemAt(details, (p) => p === "/description")}>
          {(aria) => <textarea {...aria} rows={2} value={description} onChange={(e) => setDescription(e.target.value)} />}
        </Field>
      </FormGrid>
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <Button onClick={onCancel}>关闭</Button>
        <Button kind="primary" busy={saving} busyLabel="保存中…" onClick={() => void save()}>
          保存
        </Button>
      </div>
    </div>
  );
}

/** A new group: a routing name, what tenants see, and the rate it starts at. */
function CreateGroup({ onCreated }: { onCreated: () => Promise<void> }) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLDivElement>();
  const [name, setName] = useState("");
  const [display, setDisplay] = useState("");
  const [description, setDescription] = useState("");
  const [rate, setRate] = useState("");
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });

  async function submit() {
    if (saving) return;
    if (rate.trim() === "") {
      setDetails([{ path: "/default_cny_per_usd", message: "请填写默认换算率" }]);
      focusRefusal();
      return;
    }
    setSaving(true);
    setDetails([]);
    setFailure(null);
    try {
      const sent = await sendAt(resolver.current, (effective_from) =>
        groups.create({ name, display_name: display, description, default_cny_per_usd: rate, effective_from }),
      );
      if (sent === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按北京时间计" }]);
        focusRefusal();
        return;
      }
      toast("已新建");
      setName("");
      setDisplay("");
      setDescription("");
      setRate("");
      await onCreated();
    } catch (e) {
      if (e instanceof ApiError && e.status === 422 && e.details.length > 0) {
        setDetails(e.details);
        focusRefusal();
      } else {
        setFailure(e instanceof ApiError ? messageFor(e) : errorText(e));
      }
    } finally {
      setSaving(false);
    }
  }

  const stray = details.filter(
    (d) => !["/name", "/display_name", "/description", "/default_cny_per_usd", "/effective_from"].includes(d.path),
  );

  return (
    <div ref={form} role="group" aria-label="新建分组">
      <p className="hint mb-5 text-caption">显示名和说明对租户可见，不要写入上游渠道名称。</p>
      <FormGrid>
        <Field
          label="路由名"
          wide
          hint="密钥与上游账号按它匹配，创建后不能更改，只含小写字母、数字、下划线和连字符"
          error={problemAt(details, (p) => p === "/name")}
        >
          {(aria) => (
            <input {...aria} autoFocus value={name} autoComplete="off" onChange={(e) => setName(e.target.value)} />
          )}
        </Field>
        <Field label="显示名" wide error={problemAt(details, (p) => p === "/display_name")}>
          {(aria) => <input {...aria} value={display} onChange={(e) => setDisplay(e.target.value)} />}
        </Field>
        <Field label="说明" wide error={problemAt(details, (p) => p === "/description")}>
          {(aria) => <textarea {...aria} rows={3} value={description} onChange={(e) => setDescription(e.target.value)} />}
        </Field>
        <Field
          label="默认换算率"
          hint="1 美元等于多少人民币，0 到 100，最多四位小数；换算率 0 表示这个分组免费"
          error={problemAt(details, (p) => p === "/default_cny_per_usd")}
        >
          {(aria) => (
            <input {...aria} inputMode="decimal" autoComplete="off" value={rate} onChange={(e) => setRate(e.target.value)} />
          )}
        </Field>
        <div className="flex flex-col gap-3 @lg:pt-8">
          <EffectiveFrom
            onResolve={(r) => {
              resolver.current = r;
            }}
            error={problemAt(details, (p) => p === "/effective_from")}
          />
        </div>
      </FormGrid>
      {stray.length > 0 && (
        <div className="note crit" role="alert">
          {stray.map((d) => d.message).join("；")}
        </div>
      )}
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      <div className="right mt-6 flex justify-end border-t border-border pt-4">
        <TwoStep
          label="新建分组"
          kind="primary"
          busy={saving}
          busyLabel="新建中…"
          ask={() => isZeroRate(rate)}
          warning="换算率 0 表示这个分组下的调用不计价。确认无误再继续。"
          onConfirm={() => void submit()}
        />
      </div>
    </div>
  );
}
