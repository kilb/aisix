import { Plus, Trash2 } from "lucide-react";
import { useId, useRef, useState, type ReactNode } from "react";
import { ApiError, channelModels, publicModels, type PublicModel, type ServedBy } from "../../lib/api";
import { runPublicModelWrite, type PublicModelWriteGate } from "../../lib/publicModelWriteGate";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { draftOf as routingDraftOf, routingBody, sameRouting, statusesOf, type RoutingDraft } from "../../lib/publicRouting";
import { NEW_TARGET, addedBody, changedBody, draftOf as targetDraftOf, shownDraft, targetsOf, unsendable, withSaved, type TargetDraft, type TargetField } from "../../lib/publicTargets";
import { cn } from "../../lib/cn";
import { CardHeader } from "../../components/page";
import { Button } from "../../ui/Button";
import { Field, useRefusalFocus } from "../../ui/Field";
import { RoutePolicy } from "../../ui/RoutePolicy";
import { RoutingModeField } from "../../ui/RoutingModeField";
import { CooldownEditor } from "../../ui/CooldownEditor";
import { UpstreamModelPicker } from "../../ui/UpstreamModelPicker";
import { GroupBadges } from "../providers/GroupBadges";
import { FORM_FOOTER, RowField, ServedGroups, GENERATED_STRATEGY, type Catalog, type SchemaInfo } from "./catalog";

const UNPRICED_STRATEGY = "least_cost";

/** Where a target row's refusal goes: one of its fields, or the row itself. */
interface RowRefusal {
  fields: Partial<Record<TargetField | "account", string>>;
  /** What none of the row's fields explains; null when they say it all. */
  message: string | null;
}

/** The field of a target row each refusal path names. */
const TARGET_PATHS: Record<string, TargetField | "account"> = {
  "/upstream_model": "upstream",
  "/priority": "priority",
  "/weight": "weight",
  "/provider_key_id": "account",
};

/** A target write's refusal, at the fields of the row it was for. */
function rowRefusal(e: unknown): RowRefusal {
  if (!(e instanceof ApiError)) return { fields: {}, message: errorText(e) };
  const fields: RowRefusal["fields"] = {};
  const rest: string[] = [];
  for (const d of e.details) {
    const field = d.path.startsWith("/cooldown") ? "cooldown" : TARGET_PATHS[d.path];
    if (field === undefined) rest.push(d.message);
    else fields[field] = fields[field] ? `${fields[field]}；${d.message}` : d.message;
  }
  if (rest.length > 0) return { fields, message: rest.join("；") };
  return { fields, message: Object.keys(fields).length > 0 ? null : messageFor(e) };
}

/** A target the form is adding: an account that does not route this model yet. */
interface Addition {
  key: number;
  /** Empty until the operator picks one. */
  accountId: string;
  draft: TargetDraft;
}

/** The refusal key of an added row; a served row's is its account id. */
const addedRow = (key: number) => `new:${key}`;

/** `record` without `key`. */
function without<T>(record: Record<string, T>, key: string): Record<string, T> {
  const next = { ...record };
  delete next[key];
  return next;
}

/**
 * One public model's routing: its settings, and its targets — one per
 * upstream account that serves it — each with the upstream name the account
 * is asked for, its priority and its weight.
 *
 * The settings are the model's own: saved through
 * `PATCH /admin/public-models/{id}`, which regenerates the routing document
 * in the same transaction, so a later account change cannot revert them. A
 * hand edit of the document could not do this — the document is owned by the
 * public model, and a save on it answers `409 managed_resource`.
 *
 * A target is that account's mapping of the model, so it is edited, added
 * and removed as the mapping (`PUT` / `DELETE
 * /admin/provider-keys/{account}/channel-models/{public model id}`), which
 * regenerates the document from the mappings. The account's own 模型 section
 * edits the same rows.
 *
 * One 保存 sends each changed or added target, in order, then the settings
 * if they changed: separate writes, so each keeps its own outcome. A refused
 * write is shown at the field its refusal names, on its own row; a row that
 * was written takes what the control plane now holds and is not sent again,
 * so saving again after a fix sends only what is left. A field the operator
 * has not touched always shows what was last read.
 */
export function PublicRoutingForm({
  publicModel,
  writeGate,
  catalog,
  schema,
  readOnly,
  onClose,
  onSaved,
  onTargetRemoved,
}: {
  writeGate: PublicModelWriteGate;
  publicModel: PublicModel;
  catalog: Catalog;
  schema: SchemaInfo | null;
  readOnly: boolean;
  onClose: () => void;
  /**
   * A save ended: `view` is the model as it now stands, or null when nothing
   * was written; `complete` says nothing was refused.
   */
  onSaved: (view: PublicModel | null, complete: boolean) => Promise<void>;
  /** 删除成功后至少应用已知移除结果，并等待父页面读回结束。 */
  onTargetRemoved: (accountName: string, view: PublicModel) => Promise<void>;
}) {
  const stored = publicModel.routing ?? {};
  // The settings the operator typed, or null while untouched: a field not
  // here shows what was last read, and only what was typed is sent, so a
  // reload after another operator's change neither hides it nor reverts it.
  const [settings, setSettings] = useState<Partial<RoutingDraft> | null>(null);
  // The target fields the operator typed, by account id; a field not here
  // shows what the mapping last read, so an untouched field follows every
  // later read and is never sent back as the value it had at first edit.
  const [edits, setEdits] = useState<Record<string, Partial<TargetDraft>>>({});
  const [additions, setAdditions] = useState<Addition[]>([]);
  const nextKey = useRef(1);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [busy, setBusy] = useState(false);
  // The last save's outcome: the settings' refusal, each refused row's, the
  // rows it wrote while something else was refused, and what it says overall.
  const [err, setErr] = useState<ApiError | string | null>(null);
  const [refusals, setRefusals] = useState<Record<string, RowRefusal>>({});
  const [written, setWritten] = useState<string[]>([]);
  const [outcome, setOutcome] = useState<string | null>(null);
  // The account whose removal was clicked once.
  const [removing, setRemoving] = useState<string | null>(null);
  const [removingBusy, setRemovingBusy] = useState(false);
  const title = readOnly ? `模型「${publicModel.name}」` : `编辑模型「${publicModel.name}」`;
  const writing = busy || removingBusy || writeGate.busy;
  const locked = readOnly || writing;

  const effectiveStrategy = typeof publicModel.effective_routing?.strategy === "string" ? publicModel.effective_routing.strategy : GENERATED_STRATEGY;
  const draft: RoutingDraft = { ...routingDraftOf(stored, effectiveStrategy), ...settings };
  const statuses = statusesOf(draft.statuses);
  const settingsBody = routingBody(draft, stored, GENERATED_STRATEGY, statuses);
  const settingsChanged = settings !== null && !sameRouting(settingsBody, stored);

  const targets = targetsOf(publicModel);
  const shownOf = (t: ServedBy) => shownDraft(t, edits[t.provider_key_id]);
  const changedTargets = targets.flatMap((t) => {
    const body = changedBody(shownOf(t), t);
    return body === null ? [] : [{ target: t, typed: shownOf(t), body }];
  });
  const dirty = settingsChanged || changedTargets.length > 0 || additions.length > 0;
  // A disabled mapping is not a target; adding its account enables it again.
  const disabledRows = new Map(
    publicModel.served_by.filter((s) => !s.enabled).map((s) => [s.provider_key_id, s] as const),
  );

  function set<K extends keyof RoutingDraft>(key: K, value: RoutingDraft[K]) {
    setSettings((d) => ({ ...d, [key]: value }));
  }

  /** Forgets the last save's word on one row: the operator is changing it. */
  function settle(row: string) {
    setRefusals((r) => without(r, row));
    setWritten((w) => w.filter((id) => id !== row));
  }

  function editTarget(t: ServedBy, patch: Partial<TargetDraft>) {
    // Only what was typed is held: an untouched field keeps following the
    // stored mapping, so a reload's newer value is what a save compares.
    setEdits((e) => ({ ...e, [t.provider_key_id]: { ...e[t.provider_key_id], ...patch } }));
    settle(t.provider_key_id);
  }

  function editAddition(key: number, patch: Partial<Addition>) {
    setAdditions((list) => list.map((a) => (a.key === key ? { ...a, ...patch } : a)));
    settle(addedRow(key));
  }

  function chooseAccount(a: Addition, accountId: string) {
    // An account whose mapping was disabled starts from what it stored, so
    // enabling it again does not silently rewrite its upstream name.
    const before = disabledRows.get(accountId);
    editAddition(a.key, { accountId, draft: before ? targetDraftOf(before) : a.draft });
  }

  function addTarget() {
    const key = nextKey.current++;
    setAdditions((list) => [...list, { key, accountId: "", draft: NEW_TARGET }]);
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    // A removal in flight reloads the targets when it lands; a save from
    // under it would race it on the same document.
    if (removingBusy || !dirty) return;
    // What cannot be sent at all stops the save before anything is written.
    const unsent: Record<string, RowRefusal> = {};
    for (const { target, typed } of changedTargets) {
      const fields = unsendable(typed, target);
      if (Object.keys(fields).length > 0) unsent[target.provider_key_id] = { fields, message: null };
    }
    for (const a of additions) {
      const fields: RowRefusal["fields"] = unsendable(a.draft);
      if (a.accountId === "") fields.account = "请选择上游账号";
      if (Object.keys(fields).length > 0) unsent[addedRow(a.key)] = { fields, message: null };
    }
    if (Object.keys(unsent).length > 0) {
      setRefusals(unsent);
      setOutcome(null);
      focusRefusal();
      return;
    }

    await runPublicModelWrite(writeGate, async () => {
      setBusy(true);
      try {
        // Fixed here: what this save writes does not change under it.
        const batch = [
          ...changedTargets.map(({ target, body }) => ({
            row: target.provider_key_id,
            accountId: target.provider_key_id,
            body,
            added: null as number | null,
          })),
          ...additions.map((a) => ({ row: addedRow(a.key), accountId: a.accountId, body: addedBody(a.draft), added: a.key })),
        ];
        const settingsSent = settingsChanged ? settingsBody : null;
        setErr(null);
        setRefusals({});
        setWritten([]);
        setOutcome(null);

        let view = publicModel;
        const refused: Record<string, RowRefusal> = {};
        const wrote: typeof batch = [];
        for (const w of batch) {
          try {
            const saved = await channelModels.put(w.accountId, publicModel.id, w.body);
            const account = catalog.accounts.get(w.accountId);
            view = withSaved(view, w.accountId, saved, {
              display_name: account?.doc.display_name ?? w.accountId,
              groups: account?.doc.groups ?? [],
            });
            wrote.push(w);
          } catch (e) {
            refused[w.row] = rowRefusal(e);
          }
        }
        let settingsSaved = false;
        let settingsRefused: ApiError | string | null = null;
        if (settingsSent !== null) {
          try {
            // Answered after its own commit, so it is also the read-back of every target written above.
            view = await publicModels.patchRouting(publicModel.id, settingsSent);
            settingsSaved = true;
          } catch (e) {
            settingsRefused = e instanceof ApiError ? e : String(e);
          }
        }
        let readbackFailed = false;
        if (wrote.length > 0 && !settingsSaved) {
          try {
            view = await publicModels.get(publicModel.id);
          } catch {
            // The rows as their writes answered them stand in for the read:
            // what was written is shown as written, and is not sent again.
            readbackFailed = true;
          }
        }

        const failed = Object.keys(refused).length + (settingsRefused === null ? 0 : 1);
        setEdits((prev) => wrote.reduce((next, w) => (w.added === null ? without(next, w.accountId) : next), prev));
        setAdditions((prev) => prev.filter((a) => !wrote.some((w) => w.added === a.key)));
        if (settingsSaved) setSettings(null);
        setErr(settingsRefused ?? (readbackFailed ? "目标已写入，但未能读回当前完整路由配置，请刷新后核对。" : null));
        setRefusals(refused);
        setWritten(failed > 0 ? wrote.map((w) => w.accountId) : []);
        setOutcome(
          failed === 0
            ? null
            : wrote.length > 0 || settingsSaved
              ? `有 ${failed} 项没有保存，原因标在各自的位置；其余改动已保存。改正后再点「保存」。`
              : "没有保存：原因标在各自的位置。",
        );
        await onSaved(wrote.length > 0 || settingsSaved ? view : null, failed === 0 && !readbackFailed);
        if (failed > 0) focusRefusal();
      } finally {
        setBusy(false);
      }
    });
  }

  async function removeTarget(accountId: string, accountName: string) {
    await runPublicModelWrite(writeGate, async () => {
      setRemovingBusy(true);
      try {
        try {
          await channelModels.remove(accountId, publicModel.id);
        } catch (e) {
          setRefusals((r) => ({ ...r, [accountId]: rowRefusal(e) }));
          return;
        }
        // DELETE 已确认的移除不能因完整读回失败而丢失；其它字段保持原样。
        let view: PublicModel = { ...publicModel, served_by: publicModel.served_by.filter((target) => target.provider_key_id !== accountId) };
        setErr(null);
        try {
          view = await publicModels.get(publicModel.id);
        } catch {
          setErr("目标已移除，但完整路由读回失败，请刷新核对。");
        }
        setRemoving(null);
        setEdits((e) => without(e, accountId));
        settle(accountId);
        await onTargetRemoved(accountName, view);
      } finally {
        setRemovingBusy(false);
      }
    });
  }

  const details = err instanceof ApiError ? err.details : [];
  const at = (path: string) => problemAt(details, (p) => p === path);
  // The strategy options come from the schema, never from a list here: the
  // one the generator uses for "not set" leads, since an operator who wants it
  // back cannot find it otherwise (it is not the schema's own default).
  //
  // `least_cost` is left out: it ranks targets by their price, a generated
  // target carries none, and the control plane refuses it for a public model.
  // A stored one is still shown, below, so the form reads what is there.
  const fromSchema =
    schema && "strategies" in schema ? schema.strategies.filter((v) => v !== UNPRICED_STRATEGY) : null;
  const strategies =
    fromSchema === null
      ? null
      : fromSchema.includes(GENERATED_STRATEGY)
        ? fromSchema
        : [GENERATED_STRATEGY, ...fromSchema];
  const bad = schema && "error" in schema ? schema.error : null;
  const shown = [
    "/routing/strategy",
    "/routing/retries",
    "/routing/max_fallbacks",
    "/routing/retry_on_429",
    "/routing/fallback_on_statuses",
    "/routing/when_all_unavailable",
  ];
  const listed = details.filter((d) => !shown.includes(d.path));

  // The accounts an added row may pick: not one that routes the model
  // already, nor one another added row has picked.
  const routed = new Set(targets.map((t) => t.provider_key_id));
  function accountOptions(a: Addition) {
    const taken = new Set([
      ...routed,
      ...additions.filter((x) => x.key !== a.key && x.accountId !== "").map((x) => x.accountId),
    ]);
    return [...catalog.accounts.values()]
      .filter((account) => !taken.has(account.id))
      .map((account) => ({
        id: account.id,
        label: `${account.doc.display_name || account.id}（${account.doc.groups?.length ? account.doc.groups.join("、") : "未设分组"}）`,
      }));
  }
  const groupBadges = (groups: string[] | undefined) => <GroupBadges groups={groups ?? []} empty="—（账号未设分组）" />;

  return (
    <form ref={form} className="panel mb-0" aria-label={title} onSubmit={submit}>
      <CardHeader
        number={3}
        title={title}
        className="mb-5"
        description={`公共模型「${publicModel.name}」的路由：每个目标是一个为它服务的上游账号。选路设置和目标都在这里改，点「保存」一并写入；移除目标立即生效。`}
      />
      <div className="@container">
        <div className="mb-4"><RoutePolicy routing={publicModel.effective_routing} /></div>
        <h4 className="mb-3 text-body font-semibold">选路设置</h4>
        {bad && (
          <div className="note crit mt-0" role="alert">
            {bad}
          </div>
        )}
        <div className="grid gap-x-4 @md:grid-cols-2 @3xl:grid-cols-3">
          <RoutingModeField
            value={{ strategy: draft.strategy, affinity: draft.affinity }}
            onChange={(next) => setSettings((previous) => ({ ...previous, ...next }))}
            disabled={locked || strategies === null}
            strategies={strategies}
            storedHashOn={stored.hash_on}
            error={at("/routing/strategy")}
            hashError={at("/routing/hash_on")}
          />
          <Field label="全部不可用时的处理" hint="只针对后台健康检查报告全部目标不健康。全部目标都在故障冷却时仍会尝试。" error={at("/routing/when_all_unavailable")}>
            {(aria) => (
              <select disabled={locked} value={draft.whenAllUnavailable} onChange={(e) => set("whenAllUnavailable", e.target.value)} {...aria}>
                <option value="">（网关默认）</option>
                <option value="fail">fail（返回 503）</option>
                <option value="try_anyway">try_anyway（仍然尝试）</option>
              </select>
            )}
          </Field>
          <Field label="重试次数" hint="留空时目标设值优先；目标未设置且仍有后备目标时为 0 次，最后一个目标继承网关配置。" error={at("/routing/retries")}>
            {(aria) => (
              <input
                type="number"
                min={0}
                step={1}
                disabled={locked}
                value={draft.retries}
                placeholder="目标未设且有后备时 0"
                onChange={(e) => set("retries", e.target.value)}
                {...aria}
              />
            )}
          </Field>
          <Field label="最多切换几个目标" hint="整条候选链的切换上限，过小可能到不了低优先级；留空表示后面的目标都可以试。" error={at("/routing/max_fallbacks")}>
            {(aria) => (
              <input
                type="number"
                min={0}
                step={1}
                disabled={locked}
                value={draft.maxFallbacks}
                placeholder={String(Math.max(0, targets.length + additions.filter((row) => row.accountId !== "").length - 1))}
                onChange={(e) => set("maxFallbacks", e.target.value)}
                {...aria}
              />
            )}
          </Field>
          <div className="min-w-0 @md:col-span-2 @3xl:col-span-2">
            <Field
              label="额外重试的状态码"
              hint="逗号分隔，例如 408, 429。上游用 4xx 表示临时故障时才有用；5xx 本来就会重试。"
              error={at("/routing/fallback_on_statuses")}
            >
              {(aria) => (
                <input
                  disabled={locked}
                  value={draft.statuses}
                  placeholder="（网关默认：不额外重试）"
                  onChange={(e) => set("statuses", e.target.value)}
                  {...aria}
                />
              )}
            </Field>
            {draft.statuses.trim() !== "" && statuses === null && (
              <p className="hint -mt-2 mb-4 text-caption">状态码要写成数字，用逗号分隔。</p>
            )}
          </div>
        </div>
        <label className="f mb-0">
          <input type="checkbox" disabled={locked} checked={draft.retryOn429} onChange={(e) => set("retryOn429", e.target.checked)} />{" "}
          <span>429 也参与重试和切换</span>
        </label>
      </div>

      <div className="@container mt-6 border-t border-line pt-5">
        <h4 className="mb-3 text-body font-semibold">路由目标（{targets.length}）</h4>
        {targets.length === 0 && additions.length === 0 ? (
          <p className="hint rounded-cell border border-dashed border-line px-4 py-6 text-center text-caption">
            还没有账号服务这个模型：请求会以「上游不可用」失败。{!readOnly && "用下面的「添加目标」加一个账号。"}
          </p>
        ) : (
          <div className="overflow-hidden rounded-cell border border-line">
            <div aria-hidden="true" className={cn(TARGET_ROW, "hidden border-b border-line bg-well px-3 py-2 text-caption font-medium text-ink-soft @3xl:grid")}>
              <span>账号</span>
              <span>上游模型名称</span>
              <span>优先级</span>
              <span>权重</span>
              <span />
            </div>
            <ul>
              {targets.map((t) => {
                const accountId = t.provider_key_id;
                const accountName = t.display_name || "（账号已不存在）";
                return (
                  <TargetRow
                    key={accountId}
                    label={`目标 ${accountName}`}
                    accountId={accountId}
                    draft={shownOf(t)}
                    publicName={publicModel.name}
                    locked={locked}
                    refusal={refusals[accountId]}
                    written={written.includes(accountId)}
                    onChange={(patch) => editTarget(t, patch)}
                    accountShown={false}
                    heading={
                      <>
                        <div className="truncate pt-2 font-medium @3xl:pt-1.5" title={accountName}>
                          <span className="font-normal text-muted">账号：</span>
                          {accountName}
                        </div>
                        <ServedGroups>{groupBadges(t.groups)}</ServedGroups>
                      </>
                    }
                    action={
                      !readOnly &&
                      removing !== accountId && (
                        <Button size="sm" kind="quiet" disabled={writing} onClick={() => setRemoving(accountId)}>
                          <Trash2 aria-hidden />
                          移除该目标
                        </Button>
                      )
                    }
                  >
                    {!readOnly &&
                      // Two steps held here rather than by `TwoStep`: a removal
                      // reloads the targets behind this form, and the step has to
                      // survive the re-render that causes.
                      removing === accountId && (
                        <div className="flex flex-wrap items-center justify-between gap-3 rounded-cell border border-line bg-well px-3 py-2.5">
                          <p role="alert" className="min-w-0 flex-1 basis-64 text-caption">
                            移除后，账号「{accountName}」不再为「{publicModel.name}」提供服务，它对该模型的映射也会删除。
                          </p>
                          <div className="flex gap-2">
                            <Button size="sm" disabled={writing} onClick={() => setRemoving(null)}>
                              取消
                            </Button>
                            <Button
                              kind="danger"
                              size="sm"
                              busy={removingBusy}
                              busyLabel="移除中…"
                              disabled={writing}
                              onClick={() => void removeTarget(accountId, accountName)}
                            >
                              确认移除该目标
                            </Button>
                          </div>
                        </div>
                      )}
                  </TargetRow>
                );
              })}
              {additions.map((a, i) => {
                const refusal = refusals[addedRow(a.key)];
                const account = catalog.accounts.get(a.accountId);
                return (
                  <TargetRow
                    key={addedRow(a.key)}
                    label={`新目标 ${i + 1}`}
                    accountId={a.accountId}
                    draft={a.draft}
                    publicName={publicModel.name}
                    locked={locked}
                    refusal={refusal}
                    written={false}
                    onChange={(patch) => editAddition(a.key, { draft: { ...a.draft, ...patch } })}
                    accountShown
                    heading={
                      <>
                        <RowField label="上游账号" at="3xl" error={refusal?.fields.account}>
                          {(aria) => (
                            <select disabled={locked} value={a.accountId} onChange={(e) => chooseAccount(a, e.target.value)} {...aria}>
                              <option value="">（选择上游账号）</option>
                              {accountOptions(a).map((o) => (
                                <option key={o.id} value={o.id}>
                                  {o.label}
                                </option>
                              ))}
                            </select>
                          )}
                        </RowField>
                        {account && <ServedGroups className="mt-1.5">{groupBadges(account.doc.groups)}</ServedGroups>}
                        {disabledRows.has(a.accountId) && (
                          <div className="sub mt-1 text-meta">该账号对这个模型的映射已停用，保存后重新启用。</div>
                        )}
                      </>
                    }
                    action={
                      <Button
                        size="sm"
                        kind="quiet"
                        disabled={writing}
                        onClick={() => {
                          setAdditions((list) => list.filter((x) => x.key !== a.key));
                          settle(addedRow(a.key));
                        }}
                      >
                        取消添加
                      </Button>
                    }
                  />
                );
              })}
            </ul>
          </div>
        )}
        <div className="mt-3 flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
          {!readOnly && (
            <Button type="button" size="sm" disabled={writing} onClick={addTarget}>
              <Plus aria-hidden />
              添加目标
            </Button>
          )}
          <p className="hint max-w-[52ch] text-caption">
            优先级高的一组先用，这一组都不可用时才用下一组；同组选择遵循上面的已存策略。上游模型名称留空即用公共模型的名称。
          </p>
        </div>
      </div>

      {(outcome || err) && (
        <div className="note crit" role="alert">
          {outcome && <p>{outcome}</p>}
          {err && (typeof err === "string" ? err : `路由设置：${messageFor(err)}`)}
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

      <div className={FORM_FOOTER}>
        <Button type="button" disabled={writing} onClick={onClose}>
          关闭
        </Button>
        {!readOnly && (
          <Button
            kind="primary"
            type="submit"
            busy={busy}
            busyLabel="保存中…"
            disabled={writing || statuses === null || !dirty}
            disabledReason={statuses !== null && !writing && !dirty ? "没有要保存的改动" : undefined}
          >
            保存
          </Button>
        )}
      </div>
    </form>
  );
}

/**
 * A public model's target row: stacked on a phone; from `@md` the account
 * and its action on one line with the three fields under them; from `@3xl`
 * one line under the header that names the columns.
 */
const TARGET_ROW =
  "grid grid-cols-2 items-start gap-x-3 gap-y-2 @md:grid-cols-[minmax(0,1fr)_6rem_6rem] @3xl:grid-cols-[minmax(0,1fr)_minmax(0,1.3fr)_6rem_6rem_minmax(7rem,auto)]";

/**
 * One target of a public model's routing form: who serves it (`heading`),
 * then the upstream name, priority and weight, each with its own refusal,
 * the row's own action (`action`), and under them whatever the row asks
 * about itself (`children`).
 */
function TargetRow({
  label,
  accountId,
  heading,
  accountShown,
  draft,
  publicName,
  locked,
  refusal,
  written,
  onChange,
  action,
  children,
}: {
  /** The row's accessible name. */
  label: string;
  /** The account whose model list the upstream field offers; empty offers none. */
  accountId: string;
  heading: ReactNode;
  /** Whether `heading` has an account field that shows a refusal of the account itself. */
  accountShown: boolean;
  draft: TargetDraft;
  publicName: string;
  locked: boolean;
  refusal: RowRefusal | undefined;
  /** Written by a save that left something else unsaved. */
  written: boolean;
  onChange: (patch: Partial<TargetDraft>) => void;
  action?: ReactNode;
  children?: ReactNode;
}) {
  const id = useId();
  const upstreamError = refusal?.fields.upstream ?? null;
  // A row without an account field takes a refusal of its account as its own.
  const message = [refusal?.message, accountShown ? null : refusal?.fields.account]
    .filter((m): m is string => typeof m === "string" && m !== "")
    .join("；");
  return (
    <li className={cn(TARGET_ROW, "border-line p-3 not-last:border-b")} aria-label={label}>
      <div className="col-span-2 min-w-0 @md:col-start-1 @md:row-start-1 @3xl:col-span-1">
        {heading}
        {written && <span className="tag mt-1.5 border-line bg-well text-ink">已保存</span>}
      </div>
      <div className="col-span-2 min-w-0 @md:col-span-1 @md:col-start-1 @md:row-start-2 @3xl:col-start-2 @3xl:row-start-1">
        <div className="flex flex-col gap-1.5">
          <span className="text-caption font-medium text-muted @3xl:hidden">上游模型名称</span>
          <UpstreamModelPicker
            // Keyed by the account: a row that switches accounts is a
            // different model list, so nothing the first account answered
            // (a list, a reason, a call still in flight) carries over.
            key={accountId}
            accountId={accountId}
            value={draft.upstream}
            publicName={publicName}
            label="上游模型名称"
            onChange={(upstream) => onChange({ upstream })}
            disabled={locked}
            invalid={upstreamError !== null}
            describedBy={upstreamError === null ? undefined : `${id}-upstream`}
          />
        </div>
        {upstreamError !== null && (
          <p className="field-problem" id={`${id}-upstream`}>
            {upstreamError}
          </p>
        )}
      </div>
      <RowField label="优先级" at="3xl" error={refusal?.fields.priority} className="@md:col-start-2 @md:row-start-2 @3xl:col-start-3 @3xl:row-start-1">
        {(aria) => (
          <input
            type="number"
            step={1}
            className="text-right tabular-nums"
            disabled={locked}
            value={draft.priority}
            onChange={(e) => onChange({ priority: e.target.value })}
            {...aria}
          />
        )}
      </RowField>
      <RowField label="权重" at="3xl" error={refusal?.fields.weight} className="@md:col-start-3 @md:row-start-2 @3xl:col-start-4 @3xl:row-start-1">
        {(aria) => (
          <input
            type="number"
            step={1}
            className="text-right tabular-nums"
            disabled={locked}
            value={draft.weight}
            onChange={(e) => onChange({ weight: e.target.value })}
            {...aria}
          />
        )}
      </RowField>
      <div className="col-span-2 flex min-h-11 items-center justify-end @md:col-span-1 @md:col-start-3 @md:row-start-1 @3xl:col-start-5">
        {action}
      </div>
      <div className="col-span-full"><CooldownEditor value={draft.cooldown ?? null} disabled={locked} onChange={(cooldown) => onChange({ cooldown })} onValidityChange={(cooldownProblem) => onChange({ cooldownProblem })} error={refusal?.fields.cooldown} /></div>
      {message !== "" && (
        <div className="note crit col-span-full m-0" role="alert">
          {message}
        </div>
      )}
      {children && <div className="col-span-full min-w-0">{children}</div>}
    </li>
  );
}
