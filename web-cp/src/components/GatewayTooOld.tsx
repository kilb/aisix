import { useEffect, useState } from "react";
import { ApiError, gateways, type Instance } from "../lib/api";
import { messageFor } from "../lib/errors";
import { errorText } from "../lib/fmt";
import { blockingInstances, minimumIn, tokenCountReviewInstances } from "../lib/gatewayFloor";

/**
 * A write the control plane refused because some registered gateways are too
 * old to read what it would write (`422 gateway_too_old`): what it needs, in
 * the refusal's own words, and the instances that hold it back.
 */
export function GatewayTooOld({ error, allowRetirement = false }: { error: ApiError; allowRetirement?: boolean }) {
  const [instances, setInstances] = useState<Instance[] | null>(null);
  const [readError, setReadError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    gateways.instances().then(
      (list) => live && setInstances(list),
      (e: unknown) => live && setReadError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, []);

  const minimums = error.details.map((d) => minimumIn(d.message)).filter((m): m is string => m !== null);
  const blockers = instances && (allowRetirement ? tokenCountReviewInstances(instances, Date.now()) : blockingInstances(instances, minimums, Date.now()));

  return (
    <div className="note crit text-body" role="alert">
      <strong>{messageFor(error)}</strong>
      <ul>
        {error.details.map((d, i) => (
          <li key={i}>{d.message}</li>
        ))}
      </ul>
      {readError ? (
        <p>网关实例读取失败：{readError}</p>
      ) : blockers === null ? (
        <p className="hint text-caption text-ink-soft">正在读取网关实例…</p>
      ) : blockers.length > 0 ? (
        <>
          <p>{allowRetirement ? "最近 24 小时内未退役的实例，请核对固定计数支持情况：" : "需要先升级这些网关实例："}</p>
          <ul>
            {blockers.map((i) => (
              <li key={`${i.dp_id}/${i.instance_id}`}>
                <code>{i.hostname || i.instance_id}</code>（凭据 {i.credential_name}）：版本 {i.version || "未上报"}
                {i.online ? "" : "，当前离线"}
              </li>
            ))}
          </ul>
        </>
      ) : (
        <p>{allowRetirement ? "当前实例列表里没有待核对的近期实例，请刷新后按上面的说明处理。" : "实例列表里没有低于该版本的实例；按上面的说明处理。"}</p>
      )}
      <p className="hint text-caption text-ink-soft">{allowRetirement ? <>最近 24 小时内上报过且未退役的实例都会计入，离线的也算。请到<a href="#gateways">网关页</a>升级实例；只有确认已停止且不会重新启用的离线实例，才可明确退役。退役保留历史记录，再次心跳会清除退役状态。</> : "最近 24 小时内上报过的网关实例都会计入，离线的也算。升级后重试此操作。"}</p>
    </div>
  );
}
