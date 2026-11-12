import { useEffect, useRef, useState } from "react";
import type { TabProps } from "../App";
import { createPublicModelWriteGate } from "../lib/publicModelWriteGate";
import { errorText } from "../lib/fmt";
import { defaultAt, enumAt, loadSchema } from "../lib/schema";
import { useResource } from "../lib/useResource";
import { Msg, type Note } from "../components/Msg";
import { PublicModels } from "./pricing/PublicModels";
import { PublicRoutingForm } from "./models/PublicRoutingForm";
import type { AccountDoc, ModelDoc, SchemaInfo } from "./models/catalog";

/** 公共模型的生命周期、选路设置和账号映射在同一入口维护。 */
export function PublicRoutes(props: TabProps) {
  const writeActive = useRef(false);
  const [writeBusy, setWriteBusy] = useState(false);
  const writeGate = { ...createPublicModelWriteGate(writeActive, setWriteBusy), busy: writeBusy };
  const models = useResource<ModelDoc>("models");
  const accounts = useResource<AccountDoc>("provider_keys");
  const [note, setNote] = useState<Note | null>(null);
  const [schema, setSchema] = useState<SchemaInfo | null>(null);
  useEffect(() => {
    let live = true;
    const at = "/definitions/Routing/properties/strategy";
    void loadSchema("model").then((document) => {
      if (!live) return;
      try {
        const preset = defaultAt(document, at);
        setSchema({ strategies: enumAt(document, at), preset: typeof preset === "string" ? preset : null });
      } catch (error) { setSchema({ error: errorText(error) }); }
    }, (error) => { if (live) setSchema({ error: errorText(error) }); });
    return () => { live = false; };
  }, []);
  const catalog = {
    models: new Map((models.items ?? []).map((model) => [model.id, model])),
    accounts: new Map((accounts.items ?? []).map((account) => [account.id, account])),
  };
  return <>
    <Msg note={note} />
    {accounts.error && <p className="note crit" role="alert">上游账号读取失败，请刷新后再添加目标：{errorText(accounts.error)}</p>}
    <PublicModels {...props} writeGate={writeGate} renderRouting={(model, reload, close, apply) => model.retired_at ? undefined : <PublicRoutingForm
      key={model.id} writeGate={writeGate} publicModel={model} catalog={catalog} schema={schema} readOnly={!props.canEdit} onClose={close}
      onSaved={async (view, complete) => {
        setNote(complete && view ? { kind: "ok", text: `已保存「${view.name}」的路由设置。` } : null);
        if (view) {
          apply(view);
          void models.reload();
          if (!await reload()) setNote({ kind: "warn", text: "改动已保存，但完整列表读回失败，请刷新核对。" });
        }
      }}
      onTargetRemoved={async (account, view) => {
        setNote({ kind: "ok", text: `已移除账号「${account}」对「${model.name}」的映射，该目标已不在路由里。` });
        apply(view);
        void models.reload();
        if (!await reload()) setNote({ kind: "warn", text: `已移除账号「${account}」的映射，但完整列表读回失败，请刷新核对。` });
      }}
    />} />
  </>;
}
