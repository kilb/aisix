import { useCallback, useEffect, useMemo, useState } from "react";
import { ApiError, channelModels, publicModels, type ChannelModel, type PublicModel } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { GroupList } from "../../ui/GroupName";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { TwoStep } from "../../ui/TwoStep";
import { UpstreamModelPicker } from "../../ui/UpstreamModelPicker";
import { upstreamNameProblem } from "../../lib/upstreamName";
import { FetchUpstream } from "./FetchUpstream";
import "./ChannelModels.css";

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
  groups,
  canEdit,
}: {
  accountId: string;
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
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(0);
  const [total, setTotal] = useState(0);
  const [note, setNote] = useState<string | null>(null);
  const [newName, setNewName] = useState("");
  const [creating, setCreating] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [channels, models] = await Promise.all([channelModels.list(accountId), publicModels.list()]);
      setRows(channels);
      setPublics(models.filter((m) => m.retired_at === null));
      setEveryPublic(models);
      setDrafts(Object.fromEntries(channels.map((r) => [r.public_model_id, draftOf(r)])));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, [accountId]);

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
   * `served_by`, `tenants` and `keys`: the page counts nothing itself.
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

  const filter = query.trim().toLowerCase();
  const shown = (rows ?? []).filter((row) => {
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
    <section className="panel ch-section" aria-label="模型">
      <h2>
        模型
        <Button size="sm" type="button" icon="refresh" disabled={busy} onClick={() => void load()}>
          刷新
        </Button>
      </h2>
      <p className="hint">
        勾选该上游账号提供哪些公共模型，并写明它对上游使用的名称、优先级与权重。价格按公共模型计算，与具体账号无关；成本价要填的就是这里写的上游模型名称。
        优先级相同的按权重分配，失败后按优先级从高到低依次尝试。账号分组：{groups.length > 0 ? <GroupList groups={groups} /> : "—"}。
      </p>
      {error && (
        <div className="note crit" role="alert">
          读取失败：{error}
        </div>
      )}
      <div className="ch-filter">
        <label className="f">
          <span className="f-label">搜索模型</span>
          <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="按公共模型名称或上游名称筛选" />
        </label>
      </div>
      {note && (
        <div className="note" role="status">
          {note}
        </div>
      )}
      {refusal && (
        <div className="note crit" role="alert">
          {refusal}
        </div>
      )}
      {rows === null ? (
        !error && <Loading />
      ) : (
        <DataTable label="模型" scroll>
          <thead>
            <tr>
              <th>服务</th>
              <th>公共模型</th>
              <th>上游模型名称</th>
              <th className="num">优先级</th>
              <th className="num">权重</th>
              <th>启用</th>
              <th>账号分组</th>
            </tr>
          </thead>
          <tbody>
            {shown.map((row) => {
              const draft = drafts[row.public_model_id] ?? draftOf(row);
              const model = modelOf.get(row.public_model_id);
              return (
                <tr key={row.public_model_id} data-model={row.public_model_id}>
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
                    <span className="mono">{row.public_model_name}</span>
                  </td>
                  <td>
                    <UpstreamModelPicker
                      accountId={accountId}
                      value={draft.upstream}
                      publicName={row.public_model_name}
                      label={`上游模型名称 ${row.public_model_name}`}
                      onChange={(next) => set(row.public_model_id, { upstream: next })}
                      disabled={!canEdit || busy || !draft.served}
                      invalid={rowErrors[row.public_model_id] !== undefined}
                    />
                  </td>
                  <td className="num">
                    <input
                      className="ch-num"
                      aria-label={`优先级 ${row.public_model_name}`}
                      value={draft.priority}
                      inputMode="numeric"
                      disabled={!canEdit || busy || !draft.served}
                      onChange={(e) => set(row.public_model_id, { priority: e.target.value })}
                    />
                  </td>
                  <td className="num">
                    <input
                      className="ch-num"
                      aria-label={`权重 ${row.public_model_name}`}
                      value={draft.weight}
                      inputMode="numeric"
                      disabled={!canEdit || busy || !draft.served}
                      onChange={(e) => set(row.public_model_id, { weight: e.target.value })}
                    />
                  </td>
                  <td>
                    <input
                      type="checkbox"
                      aria-label={`启用 ${row.public_model_name}`}
                      checked={draft.enabled}
                      disabled={!canEdit || busy || !draft.served}
                      onChange={(e) => set(row.public_model_id, { enabled: e.target.checked })}
                    />
                  </td>
                  <td className="ch-groups">
                    {model && model.served_by.some((s) => s.provider_key_id === accountId) ? (
                      <GroupList
                        groups={
                          model.served_by.find((s) => s.provider_key_id === accountId)?.groups ?? []
                        }
                      />
                    ) : (
                      groups.length > 0 ? <GroupList groups={groups} /> : "—"
                    )}
                  </td>
                </tr>
              );
            })}
            {shown.length === 0 && (
              <tr>
                <td colSpan={7}>
                  <EmptyState text={rows.length === 0 ? "还没有公共模型。用下面的名称新建一个。" : "没有匹配的模型。"} />
                </td>
              </tr>
            )}
          </tbody>
        </DataTable>
      )}
      {Object.keys(rowErrors).length > 0 && (
        <ul className="note crit ch-errors" role="alert">
          {Object.entries(rowErrors).map(([id, text]) => (
            <li key={id}>
              {rows?.find((r) => r.public_model_id === id)?.public_model_name ?? id}：{text}
            </li>
          ))}
        </ul>
      )}
      {canEdit && rows !== null && (
        <>
          <p className="ch-save">
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
                        停用后「{model.name}」将没有可用账号：影响 {model.tenants} 个租户、{model.keys} 个密钥。
                      </span>
                    ))}
                    停用后这些公共模型仍然保留，但请求会失败（无可用上游）。
                  </>
                )
              }
              onConfirm={() => void write()}
            />
            {changed.length > 0 && <span className="hint">将保存 {changed.length} 个模型的改动。</span>}
          </p>
          <FetchUpstream
            accountId={accountId}
            rows={rows}
            publics={everyPublic}
            disabled={busy || creating}
            onApplied={load}
          />
          <div className="ch-create">
            <p className="hint">上游有、公共模型还没有的名称可以在这里一并新建；价格仍然建在该公共模型上。</p>
            <div className="ch-create-row">
              <UpstreamModelPicker
                accountId={accountId}
                value={newName}
                publicName="输入或选择一个上游模型名称"
                label="新的公共模型名称"
                onChange={setNewName}
                disabled={creating || busy}
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
        </>
      )}
      {!canEdit && <p className="hint">只读角色只能查看，不能修改模型映射。</p>}
    </section>
  );
}
