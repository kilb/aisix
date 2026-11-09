import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import {
  ApiError,
  groupPrices,
  publicModels,
  type ModelGroupPriceRow,
  type PublicModel,
} from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { PublicModelDetail } from "./PublicModelDetail";
import "./PublicModels.css";

/** The counts a `409 public_model_in_use` carries. */
type UseCounts = { tenants?: number; keys?: number; channels?: number };

/**
 * What the retire confirmation says will happen. While a grant or an enabled
 * serving account still holds the model the retire is refused, so it says
 * that, with the counts and what to clear. Otherwise the retire succeeds and
 * cannot be undone: it says what goes — the routing document, the direct
 * documents generated for each account, and the accounts' mappings — and
 * that the name stays taken.
 */
function retireWarning(model: PublicModel): string {
  const accounts = model.served_by.filter((s) => s.enabled).length;
  const holders = [
    ...(model.tenants > 0 || model.keys > 0 ? [`${model.tenants} 个租户、${model.keys} 个密钥的授权`] : []),
    ...(accounts > 0 ? [`${accounts} 个启用的账号`] : []),
  ];
  if (holders.length === 0) {
    const mappings = model.served_by.length > 0 ? `，以及 ${model.served_by.length} 个账号上的映射设置` : "";
    return `停用后不能恢复：将删除它的路由文档和为各账号生成的直连文档${mappings}。名称「${model.name}」以后不能再用于公共模型，需要时请用新名称新建一个公共模型。`;
  }
  const clear = [
    ...(model.tenants > 0 || model.keys > 0 ? ["从租户与密钥中移除它"] : []),
    ...(accounts > 0 ? ["在上游账号的「模型」里取消勾选"] : []),
  ];
  return `仍有 ${holders.join("，以及 ")}指向它，停用会被拒绝，什么也不会删除：请先${clear.join("，并")}。`;
}

/** What still names a public model, by name, so the operator knows what to clear. */
export function inUseText(e: ApiError): string {
  const counts = (e.counts ?? {}) as UseCounts;
  return `${messageFor(e)}：租户 ${counts.tenants ?? 0}、密钥 ${counts.keys ?? 0}、启用的账号 ${counts.channels ?? 0}`;
}

/** How many accounts serve a model, and at which priorities: `2 个（优先级 5、0）`. */
function tiersOf(model: PublicModel): string {
  const accounts = model.served_by.filter((s) => s.enabled);
  if (accounts.length === 0) return "没有可用账号";
  const tiers = [...new Set(accounts.map((s) => s.priority))].sort((a, b) => b - a);
  return `${accounts.length} 个（优先级 ${tiers.join("、")}）`;
}

/** Whether a model has a baseline and a rate in every group; `null` while the prices are not read. */
function pricedState(model: PublicModel, prices: Map<string, ModelGroupPriceRow> | null): "priced" | "unpriced" | null {
  if (prices === null) return null;
  const row = prices.get(model.id);
  if (!row || !row.baseline.current) return "unpriced";
  if (row.groups.length === 0) return "unpriced";
  // The API sends `null` for a group with no rate in force, never omits it.
  return row.groups.every((g) => g.cny_per_usd !== null) ? "priced" : "unpriced";
}

/**
 * 价格 · 公共模型: the names tenants send, the accounts that serve them, and
 * whether each is priced.
 *
 * A public model's id is minted once and is the generated routing document's
 * id; grants, baselines and group rates all key on it. So a name is never
 * renamed and never adopted: a name a live model already holds is refused on
 * the name field, and the answer says the operator may use the model that
 * already has it.
 */
export function PublicModels({ canEdit }: TabProps) {
  const toast = useToast();
  const [rows, setRows] = useState<PublicModel[] | null>(null);
  const [prices, setPrices] = useState<Map<string, ModelGroupPriceRow> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);
  /** The refusal belongs on the name field: a live model already holds the name. */
  const [atName, setAtName] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  const load = useCallback(async () => {
    try {
      const [models, priced] = await Promise.all([publicModels.list(), groupPrices.list()]);
      setRows(models);
      setPrices(new Map(priced.map((p) => [p.model_id, p] as const)));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const open = rows?.find((m) => m.id === selected) ?? null;
  if (open) {
    return (
      <PublicModelDetail
        model={open}
        canEdit={canEdit}
        onBack={() => setSelected(null)}
        onChanged={load}
      />
    );
  }

  const nameRefusal = atName ? refusal : null;

  async function create(e: React.FormEvent) {
    e.preventDefault();
    setCreating(true);
    setRefusal(null);
    setAtName(false);
    try {
      const model = await publicModels.create(name);
      setName("");
      toast(`已新建公共模型「${model.name}」。请在任一上游账号的「模型」里勾选它。`);
      await load();
    } catch (err) {
      if (err instanceof ApiError) {
        const onName = problemAt(err.details, (p) => p === "/name");
        setAtName(onName !== null);
        setRefusal(onName ?? messageFor(err));
      } else {
        setRefusal(errorText(err));
      }
      focusRefusal();
    } finally {
      setCreating(false);
    }
  }

  async function retire(model: PublicModel) {
    setBusy(model.id);
    setRefusal(null);
    try {
      await publicModels.retire(model.id);
      toast(`已停用「${model.name}」。`);
      await load();
    } catch (e) {
      setRefusal(e instanceof ApiError ? inUseText(e) : errorText(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="panel">
      <h2>
        公共模型
        <Button size="sm" type="button" icon="refresh" onClick={() => void load()}>
          刷新
        </Button>
      </h2>
      <p className="hint">
        租户请求的模型名。标识只生成一次，授权、基准价和分组换算率都跟着它，所以名称不改、也不接管已有模型；显示名重复时请直接使用已有模型。
      </p>
      {error && (
        <div className="note crit" role="alert">
          读取失败：{error}
        </div>
      )}
      {refusal && !atName && (
        <div className="note crit" role="alert">
          {refusal}
        </div>
      )}
      {rows === null ? (
        !error && <Loading />
      ) : rows.length === 0 ? (
        <EmptyState text="还没有公共模型。" />
      ) : (
        <DataTable label="公共模型">
          <thead>
            <tr>
              <th>名称</th>
              <th>服务账号</th>
              <th>定价</th>
              <th className="num">授权租户</th>
              <th className="num">密钥</th>
              <th>状态</th>
              <th className="right">操作</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((m) => {
              const priced = pricedState(m, prices);
              const retired = m.retired_at !== null;
              return (
                <tr key={m.id} data-model={m.id}>
                  <td>
                    <button type="button" className="link-button" onClick={() => setSelected(m.id)}>
                      {m.name}
                    </button>
                  </td>
                  <td>{tiersOf(m)}</td>
                  <td>
                    {retired ? (
                      "—"
                    ) : priced === null ? (
                      "…"
                    ) : priced === "priced" ? (
                      "已定价"
                    ) : (
                      <span className="pm-unpriced">未定价</span>
                    )}
                  </td>
                  <td className="num">{m.tenants}</td>
                  <td className="num">{m.keys}</td>
                  <td>{retired ? "已停用" : "在用"}</td>
                  <td className="right">
                    {canEdit && !retired && (
                      <TwoStep
                        label="停用"
                        size="sm"
                        busy={busy === m.id}
                        busyLabel="停用中…"
                        // Always asked: refused, it says why; accepted, it cannot be undone.
                        ask={() => true}
                        warning={retireWarning(m)}
                        onConfirm={() => void retire(m)}
                      />
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </DataTable>
      )}
      {canEdit && (
        <form ref={form} className="pm-create" aria-label="新建公共模型" onSubmit={(e) => void create(e)}>
          <h3>新建公共模型</h3>
          <div className="r">
            <Field label="名称" error={nameRefusal}>
              {(aria) => (
                <input
                  required
                  value={name}
                  disabled={creating}
                  placeholder="租户请求时填写的模型名"
                  onChange={(e) => {
                    setName(e.target.value);
                    setRefusal(null);
                    setAtName(false);
                  }}
                  {...aria}
                />
              )}
            </Field>
          </div>
          <p>
            <Button kind="primary" type="submit" busy={creating} busyLabel="新建中…">
              新建
            </Button>
          </p>
        </form>
      )}
    </div>
  );
}
