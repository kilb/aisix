import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, channelModels, publicModels, type ChannelModel, type PublicModel } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import {
  alreadyRetiredText,
  keptText,
  removalFailedText,
  removeThenRetire,
  retiredText,
  unknownText,
  type Readback,
} from "../../lib/retireOutcome";
import { retireWarning, withoutServing } from "../../lib/retireWarning";
import { Plus, RefreshCw, Search } from "lucide-react";
import { CardHeader } from "../../components/page";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { TwoStep } from "../../ui/TwoStep";
import { UpstreamModelPicker } from "../../ui/UpstreamModelPicker";
import { upstreamNameProblem } from "../../lib/upstreamName";
import { FetchUpstream } from "./FetchUpstream";
import { GroupBadges } from "./GroupBadges";

/** The largest weight the control plane takes, as its own `channel_models` check does. */
const MAX_WEIGHT = 65_535;

/** One row as the operator is editing it: the numbers as typed, so a cleared box is not a zero. */
interface Draft {
  /** Whether this account serves the public model at all. */
  served: boolean;
  upstream: string;
  priority: string;
  weight: string;
  enabled: boolean;
}

function draftOf(row: ChannelModel): Draft {
  return {
    served: row.served,
    upstream: row.upstream_model ?? "",
    priority: String(row.priority),
    weight: String(row.weight),
    enabled: row.enabled,
  };
}

function sameDraft(a: Draft, b: Draft): boolean {
  return a.served === b.served && a.upstream === b.upstream && a.priority === b.priority && a.weight === b.weight && a.enabled === b.enabled;
}

/** A whole number as typed: trimmed, digits only, with the range its column allows. */
function numberProblem(text: string, min: number, max: number, label: string): string | null {
  const typed = text.trim();
  if (typed === "") return `${label}不能为空`;
  if (!/^-?\d+$/.test(typed)) return `${label}必须是整数`;
  const value = Number(typed);
  if (value < min || value > max) return `${label}应在 ${min} 到 ${max} 之间`;
  return null;
}

/**
 * The 模型 section of one upstream account: every live public model, with a
 * tick for whether this account serves it, the upstream name it is asked for,
 * its priority and weight, whether the row is enabled, and the account's own
 * groups.
 *
 * A save is one write per changed row, run in order with its own progress,
 * and each row keeps its own error: one refused mapping does not throw away
 * the others' work. Ticking `服务` off removes the row (`DELETE`); leaving it
 * ticked and `启用` off keeps the row and stops generating its document — the
 * two are different questions, and the control plane answers them differently.
 *
 * **Turning off the last enabled account of a public model is asked twice**
 * (`TwoStep`), naming the tenants and keys that lose the model. The public
 * model stays listed either way and answers `503`: the gateway refuses a
 * routing document with no target, so nothing silently starts serving.
 *
 * A name that is not a public model yet can be created from here: the field
 * offers the account's own upstream list, and `以该名称新建公共模型` creates
 * the public model and this account's row in one flow. `获取上游模型` does the
 * same for a whole selection of the account's list at once ([`FetchUpstream`]).
 */
export function ChannelModels({
  accountId,
  accountName,
  groups,
  canEdit,
}: {
  accountId: string;
  /** The account's name, beside the section's title: the section sits apart from the account's form. */
  accountName: string;
  /** The groups this account lists, which the section states so a tier is readable. */
  groups: string[];
  canEdit: boolean;
}) {
  const [rows, setRows] = useState<ChannelModel[] | null>(null);
  const [publics, setPublics] = useState<PublicModel[] | null>(null);
  // Retired ones included: a retired name cannot be a public model's again.
  const [everyPublic, setEveryPublic] = useState<PublicModel[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [rowErrors, setRowErrors] = useState<Record<string, string>>({});
  const [query, setQuery] = useState("");
  // Off by default: the list shows what this account serves, and revealing
  // the rest is a deliberate act. It reads the server's `served`, not the
  // draft, so unticking 服务 on a served row keeps it in view to be undone.
  const [showAll, setShowAll] = useState(false);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(0);
  const [total, setTotal] = useState(0);
  const [note, setNote] = useState<string | null>(null);
  const [newName, setNewName] = useState("");
  const [creating, setCreating] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);
  // The public model id whose `删除模型` is in flight.
  const [removing, setRemoving] = useState<string | null>(null);

  // The rows the drafts were last started from: what "edited" is measured
  // against, never the rows a reload has just brought.
  const baseline = useRef<ChannelModel[] | null>(null);

  /**
   * Re-reads the checklist, and answers what it read (`null` when the read
   * failed). `keepDrafts` starts every row from the server's values except
   * the rows the operator has edited since the last read, so a reload after
   * 删除模型 does not throw those away; a draft of a row that no longer
   * exists goes with it.
   *
   * "Edited" is the draft against the row it was started from, not against
   * the row just read: a row the server changed under an untouched draft
   * (the mapping 删除模型 just removed) takes the server's values, so it
   * never enters a later save on its own. The rows in `settled` take the
   * server's values whatever their draft says — a write on them has just
   * landed, and the draft described the state before it.
   */
  const load = useCallback(
    async (keepDrafts = false, settled: readonly string[] = []) => {
      try {
        const [channels, models] = await Promise.all([channelModels.list(accountId), publicModels.list()]);
        const before = baseline.current;
        baseline.current = channels;
        setRows(channels);
        setPublics(models.filter((m) => m.retired_at === null));
        setEveryPublic(models);
        setDrafts((prev) => {
          const fresh = Object.fromEntries(channels.map((r) => [r.public_model_id, draftOf(r)]));
          if (!keepDrafts) return fresh;
          for (const row of channels) {
            const id = row.public_model_id;
            const edited = prev[id];
            const was = before?.find((r) => r.public_model_id === id);
            if (edited === undefined || was === undefined || settled.includes(id)) continue;
            if (!sameDraft(edited, draftOf(was))) fresh[id] = edited;
          }
          return fresh;
        });
        setError(null);
        return { channels, models };
      } catch (e) {
        setError(errorText(e));
        return null;
      }
    },
    [accountId],
  );

  useEffect(() => {
    void load();
  }, [load]);

  const modelOf = useMemo(
    () => new Map((publics ?? []).map((m) => [m.id, m] as const)),
    [publics],
  );

  const changed = useMemo(() => {
    return (rows ?? []).filter((row) => {
      const draft = drafts[row.public_model_id];
      return draft !== undefined && !sameDraft(draft, draftOf(row));
    });
  }, [rows, drafts]);

  /**
   * The public models this save would take from their last enabled serving account,
   * by the tenants and keys that reach one today. Read from the API's own
   * `served_by`, `reachable_tenants` and `reachable_keys`: the page counts
   * nothing itself.
   */
  const unserved = useMemo(() => {
    const losing: PublicModel[] = [];
    for (const row of changed) {
      const draft = drafts[row.public_model_id];
      const model = modelOf.get(row.public_model_id);
      if (draft === undefined || model === undefined) continue;
      // Only a row that serves now and stops in this save takes anything
      // away: editing a row that is already off changes nothing served.
      if (!(row.served && row.enabled)) continue;
      // This account keeps serving it while the row is served and enabled.
      if (draft.served && draft.enabled) continue;
      const others = model.served_by.filter((s) => s.enabled && s.provider_key_id !== accountId).length;
      if (others === 0) losing.push(model);
    }
    return losing;
  }, [changed, drafts, modelOf, accountId]);

  function set(id: string, patch: Partial<Draft>) {
    setDrafts((prev) => {
      const row = rows?.find((r) => r.public_model_id === id);
      const base = prev[id] ?? (row === undefined ? undefined : draftOf(row));
      if (base === undefined) return prev;
      return { ...prev, [id]: { ...base, ...patch } };
    });
    setRowErrors((prev) => {
      const next = { ...prev };
      delete next[id];
      return next;
    });
  }

  /** The row's own refusals, before anything is sent. */
  function problems(id: string): string | null {
    const draft = drafts[id];
    if (draft === undefined) return null;
    const upstream = upstreamNameProblem(draft.upstream);
    if (upstream !== null) return upstream;
    if (!draft.served) return null;
    return (
      numberProblem(draft.priority, -2_147_483_648, 2_147_483_647, "优先级") ??
      numberProblem(draft.weight, 0, MAX_WEIGHT, "权重")
    );
  }

  async function write() {
    // The batch is fixed here: a re-render part-way through (a tick, a
    // refusal) must not change what this save is writing.
    const batch = changed;
    const bad = batch.find((row) => problems(row.public_model_id) !== null);
    if (bad !== undefined) {
      setRowErrors((prev) => ({ ...prev, [bad.public_model_id]: problems(bad.public_model_id)! }));
      return;
    }
    setBusy(true);
    setNote(null);
    setRefusal(null);
    setTotal(batch.length);
    setDone(0);
    const failures: Record<string, string> = {};
    for (const row of batch) {
      const draft = drafts[row.public_model_id]!;
      try {
        if (!draft.served && row.served) {
          await channelModels.remove(accountId, row.public_model_id);
        } else {
          await channelModels.put(accountId, row.public_model_id, {
            upstream_model: draft.upstream.trim() === "" ? null : draft.upstream.trim(),
            priority: Number(draft.priority),
            weight: Number(draft.weight),
            enabled: draft.enabled,
          });
        }
      } catch (e) {
        failures[row.public_model_id] = e instanceof ApiError ? messageFor(e) : errorText(e);
      }
      setDone((n) => n + 1);
    }
    setRowErrors(failures);
    setBusy(false);
    const failed = Object.keys(failures).length;
    setNote(failed === 0 ? `已保存 ${batch.length} 个模型的映射。` : `${batch.length} 个模型中有 ${failed} 个保存失败，见各行提示。`);
    await load();
  }

  async function createFromName() {
    const name = newName.trim();
    if (name === "") return;
    setCreating(true);
    setRefusal(null);
    try {
      const model = await publicModels.create(name);
      await channelModels.put(accountId, model.id, { enabled: true });
      setNewName("");
      setNote(`已新建公共模型「${model.name}」，并由该账号提供。`);
      await load();
    } catch (e) {
      setRefusal(e instanceof ApiError ? messageFor(e) : errorText(e));
    } finally {
      setCreating(false);
    }
  }

  /**
   * Drops one public model's row from what this section holds: its row, its
   * draft and its baseline entry. Called once a removal is confirmed, so a
   * read-back that fails cannot leave the removed row (and a stale edit of
   * it) for the next save to write back.
   */
  function forget(id: string) {
    baseline.current = baseline.current?.filter((r) => r.public_model_id !== id) ?? null;
    setRows((prev) => prev?.filter((r) => r.public_model_id !== id) ?? prev);
    setDrafts((prev) => {
      if (prev[id] === undefined) return prev;
      const next = { ...prev };
      delete next[id];
      return next;
    });
  }

  /**
   * 删除模型: this account's mapping goes first, then the public model is
   * stopped — the two writes [removeThenRetire] names, kept apart in the
   * answer because the second can be refused after the first succeeded. A
   * partial outcome is never reported as a plain failure.
   */
  async function deleteModel(row: ChannelModel) {
    const name = row.public_model_name;
    setNote(null);
    setRefusal(null);
    setRemoving(row.public_model_id);
    const id = row.public_model_id;
    const outcome = await removeThenRetire(
      () => channelModels.remove(accountId, id),
      () => publicModels.retire(id),
    );
    if (outcome.kind === "removal_failed") {
      setRemoving(null);
      setRefusal(removalFailedText(name, outcome.error));
      return;
    }
    // The removal is confirmed, so the row goes now, before the read-back:
    // a read-back that fails would otherwise leave the row, and a draft the
    // operator had edited, where the next save would write the mapping back
    // as a new one. The read-back below still settles every other row.
    if (outcome.kind !== "removal_unknown") forget(id);
    // This model's row takes what the server now says, whatever its draft
    // held; every other row keeps whatever was edited.
    const read = await load(true, [id]);
    setRemoving(null);
    switch (outcome.kind) {
      case "retired":
        setNote(retiredText(name));
        break;
      case "already_retired":
        setNote(alreadyRetiredText(name));
        break;
      case "kept_in_use":
      case "kept_failed":
        setRefusal(keptText(name, outcome));
        break;
      case "removal_unknown":
      case "retire_unknown": {
        // Said from what the read-back found, never from the lost answer.
        const readback: Readback | null =
          read === null
            ? null
            : {
                served: read.channels.some((c) => c.public_model_id === id && c.served),
                retired: read.models.find((m) => m.id === id)?.retired_at != null,
              };
        setRefusal(unknownText(name, outcome, readback));
        break;
      }
    }
  }

  const filter = query.trim().toLowerCase();
  // The default view is the models this account serves: an account that
  // serves 1 of 27 reads as "this account serves that one", not as 26
  // unchecked rows that look like state to fix. A revealed row keeps its
  // place while it is being ticked, so the tick is what files it.
  const unservedTotal = (rows ?? []).filter((row) => !row.served).length;
  const visible = (rows ?? []).filter((row) => showAll || row.served);
  const shown = visible.filter((row) => {
    if (filter === "") return true;
    const draft = drafts[row.public_model_id];
    return (
      row.public_model_name.toLowerCase().includes(filter) ||
      (draft?.upstream ?? "").toLowerCase().includes(filter)
    );
  });
  const taken = new Set((publics ?? []).map((m) => m.name));
  const canCreate = newName.trim() !== "" && !taken.has(newName.trim()) && upstreamNameProblem(newName) === null;

  return (
    <section className="ch-section grid gap-3" aria-label="模型">
      <div className="panel mb-0">
        <CardHeader
          number={5}
          title={
            <span className="flex flex-wrap items-center gap-2">
              模型
              <span className="tag font-mono font-normal">{accountName}</span>
            </span>
          }
          description={
            <>
              勾选该上游账号提供哪些公共模型，并写明它对上游使用的名称、优先级与权重。价格按公共模型计算，与具体账号无关；成本价要填的就是这里写的上游模型名称。
              优先级相同的按权重分配，失败后按优先级从高到低依次尝试。账号分组：<GroupBadges groups={groups} />。
            </>
          }
          actions={
            <Button size="sm" type="button" disabled={busy} onClick={() => void load()}>
              <RefreshCw aria-hidden />
              刷新
            </Button>
          }
        />
        {error && (
          <div className="note crit" role="alert">
            读取失败：{error}
          </div>
        )}
        <div className="flex flex-wrap items-center gap-x-6 gap-y-3">
          <label className="relative block w-full sm:w-80">
            <span className="sr-only">搜索模型</span>
            <Search aria-hidden className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted" />
            <input className="pl-9" value={query} onChange={(e) => setQuery(e.target.value)} placeholder="按公共模型名称或上游名称筛选" />
          </label>
          <label className="ch-tick gap-2 text-caption">
            <input type="checkbox" checked={showAll} onChange={(e) => setShowAll(e.target.checked)} />
            <span>显示全部公共模型（{unservedTotal}）</span>
          </label>
        </div>
        {note && (
          <div className="note mb-0" role="status">
            {note}
          </div>
        )}
        {refusal && (
          <div className="note crit mb-0" role="alert">
            {refusal}
          </div>
        )}
        {rows === null ? (
          !error && <Loading />
        ) : (
          <DataTable label="模型" scroll>
            <thead>
              <tr>
                <th className="w-12">服务</th>
                <th>公共模型</th>
                <th>上游模型名称</th>
                <th className="num">优先级</th>
                <th className="num">权重</th>
                <th>启用</th>
                <th>账号分组</th>
                <th className="right">操作</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((row) => {
                const draft = drafts[row.public_model_id] ?? draftOf(row);
                const model = modelOf.get(row.public_model_id);
                return (
                  <tr key={row.public_model_id} data-model={row.public_model_id} className={draft.served ? undefined : "text-muted"}>
                    <td>
                      <label className="ch-tick">
                        <input
                          type="checkbox"
                          aria-label={`服务 ${row.public_model_name}`}
                          checked={draft.served}
                          disabled={!canEdit || busy}
                          onChange={(e) => {
                            const on = e.target.checked;
                            // Ticking an unserved model is the operator asking
                            // for it to serve, so the row starts enabled; the
                            // two boxes answer different questions only once
                            // the row exists.
                            set(row.public_model_id, { served: on, enabled: on });
                          }}
                        />
                      </label>
                    </td>
                    <td>
                      <span className="mono font-mono text-caption font-medium">{row.public_model_name}</span>
                    </td>
                    <td className="min-w-56 whitespace-normal">
                      <UpstreamModelPicker
                        accountId={accountId}
                        value={draft.upstream}
                        publicName={row.public_model_name}
                        label={`上游模型名称 ${row.public_model_name}`}
                        onChange={(next) => set(row.public_model_id, { upstream: next })}
                        disabled={!canEdit || busy || !draft.served}
                        invalid={rowErrors[row.public_model_id] !== undefined}
                        compact
                      />
                    </td>
                    <td className="num">
                      <input
                        className="ch-num h-11"
                        aria-label={`优先级 ${row.public_model_name}`}
                        value={draft.priority}
                        inputMode="numeric"
                        disabled={!canEdit || busy || !draft.served}
                        onChange={(e) => set(row.public_model_id, { priority: e.target.value })}
                      />
                    </td>
                    <td className="num">
                      <input
                        className="ch-num h-11"
                        aria-label={`权重 ${row.public_model_name}`}
                        value={draft.weight}
                        inputMode="numeric"
                        disabled={!canEdit || busy || !draft.served}
                        onChange={(e) => set(row.public_model_id, { weight: e.target.value })}
                      />
                    </td>
                    <td>
                      <label className="ch-tick min-h-11 min-w-11 justify-center">
                        <input
                          type="checkbox"
                          aria-label={`启用 ${row.public_model_name}`}
                          checked={draft.enabled}
                          disabled={!canEdit || busy || !draft.served}
                          onChange={(e) => set(row.public_model_id, { enabled: e.target.checked })}
                        />
                      </label>
                    </td>
                    <td className="whitespace-normal">
                      <GroupBadges
                        groups={
                          model && model.served_by.some((s) => s.provider_key_id === accountId)
                            ? (model.served_by.find((s) => s.provider_key_id === accountId)?.groups ?? [])
                            : groups
                        }
                      />
                    </td>
                    <td className="right">
                      {canEdit && row.served && (
                        <TwoStep
                          label="删除模型"
                          size="sm"
                          kind="secondary"
                          busy={removing === row.public_model_id}
                          busyLabel="删除中…"
                          disabled={busy}
                          warning={
                            model === undefined ? (
                              // The view was not read for it: say what the two writes do, without counts.
                              `删除模型「${row.public_model_name}」：先移除本账号对它的映射，再停用它；停用后它的路由文档、各账号的直连文档和账号映射都会被删除，账号映射不能恢复（公共模型本身可以在「价格 → 公共模型」恢复）。若仍有租户授权、密钥或启用的账号指向它，它会被保留，随后的提示会写明要先解除哪些。`
                            ) : (
                              <>
                                <span className="ch-warn-line">
                                  删除模型「{row.public_model_name}」：先移除本账号对它的映射，再停用它。
                                </span>
                                {/* The retire meets the state after this account's row is gone, so it is asked that way. */}
                                {retireWarning(withoutServing(model, accountId))}
                              </>
                            )
                          }
                          onConfirm={() => void deleteModel(row)}
                        />
                      )}
                    </td>
                  </tr>
                );
              })}
              {shown.length === 0 && (
                <tr>
                  <td colSpan={8} className="whitespace-normal">
                    <EmptyState
                      text={
                        rows.length === 0
                          ? "还没有公共模型。用下面的名称新建一个。"
                          : !showAll && unservedTotal === rows.length
                            ? "这个账号还没有服务任何模型。勾选「显示全部公共模型」来添加。"
                            : "没有匹配的模型。"
                      }
                    />
                  </td>
                </tr>
              )}
            </tbody>
          </DataTable>
        )}
        {Object.keys(rowErrors).length > 0 && (
          <ul className="note crit ch-errors mb-0" role="alert">
            {Object.entries(rowErrors).map(([id, text]) => (
              <li key={id}>
                {rows?.find((r) => r.public_model_id === id)?.public_model_name ?? id}：{text}
              </li>
            ))}
          </ul>
        )}
        {(rows !== null || !canEdit) && (
          // The save closes the card: what it will write on the left, the write on the right.
          <div className="ch-save -mx-7 -mb-7 mt-6 flex flex-wrap items-start justify-end gap-x-4 gap-y-2 rounded-b-card border-t border-line bg-well px-7 py-3 [&_.hint]:text-ink-soft">
            {canEdit ? (
              <>
                {changed.length > 0 && <span className="hint mr-auto self-center text-caption">将保存 {changed.length} 个模型的改动。</span>}
                <div className="right">
                  <TwoStep
                    label={busy ? `写入中…（${done}/${total}）` : "写入映射"}
                    kind="primary"
                    busy={busy}
                    busyLabel="保存中…"
                    disabled={changed.length === 0}
                    ask={() => unserved.length > 0}
                    warning={
                      unserved.length === 0 ? undefined : (
                        <>
                          {unserved.map((model) => (
                            <span key={model.id} className="ch-warn-line">
                              停用后「{model.name}」将没有可用账号：影响 {model.reachable_tenants} 个租户、{model.reachable_keys} 个密钥。
                            </span>
                          ))}
                          停用后这些公共模型仍然保留，但请求会失败（无可用上游）。
                        </>
                      )
                    }
                    onConfirm={() => void write()}
                  />
                </div>
              </>
            ) : (
              <p className="hint mr-auto text-caption">只读角色只能查看，不能修改模型映射。</p>
            )}
          </div>
        )}
      </div>

      {canEdit && rows !== null && (
        // The section's two tools, side by side: the account's own list, and one name typed in.
        <div className="grid items-start gap-3 desktop:grid-cols-2">
          <FetchUpstream
            accountId={accountId}
            rows={rows}
            publics={everyPublic}
            disabled={busy || creating}
            onApplied={async () => {
              await load();
            }}
          />
          <div className="panel ch-create mb-0">
            <CardHeader number={7} title="新建公共模型" />
            <div className="flex items-start gap-3">
              <span aria-hidden="true" className="grid size-11 shrink-0 place-items-center rounded-cell border border-line bg-well text-ink-soft">
                <Plus className="size-4" />
              </span>
              <p className="hint text-caption">上游有、公共模型还没有的名称可以在这里一并新建；价格仍然建在该公共模型上。</p>
            </div>
            <div className="mt-4 flex flex-wrap items-start gap-2">
              <UpstreamModelPicker
                accountId={accountId}
                value={newName}
                publicName="输入或选择一个上游模型名称"
                label="新的公共模型名称"
                onChange={setNewName}
                disabled={creating || busy}
                className="min-w-[14rem] flex-1"
              />
              <Button
                type="button"
                busy={creating}
                busyLabel="新建中…"
                disabled={!canCreate || busy}
                disabledReason={newName.trim() === "" ? "先填写名称" : taken.has(newName.trim()) ? "该名称已是一个公共模型，直接在上面勾选即可" : undefined}
                onClick={() => void createFromName()}
              >
                以该名称新建公共模型
              </Button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
