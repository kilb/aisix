import { useEffect, useState } from "react";
import { ApiError, gateways, type Instance } from "../lib/api";
import { messageFor } from "../lib/errors";
import { blockingInstances, minimumIn } from "../lib/gatewayFloor";

/**
 * A write the control plane refused because some registered gateways are too
 * old to read what it would write (`422 gateway_too_old`): what it needs, in
 * the refusal's own words, and the instances that hold it back.
 */
export function GatewayTooOld({ error }: { error: ApiError }) {
  const [instances, setInstances] = useState<Instance[] | null>(null);
  const [readError, setReadError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    gateways.instances().then(
      (list) => live && setInstances(list),
      (e: unknown) => live && setReadError(e instanceof ApiError ? messageFor(e) : String(e)),
    );
    return () => {
      live = false;
    };
  }, []);

  const minimums = error.details.map((d) => minimumIn(d.message)).filter((m): m is string => m !== null);
  const blockers = instances && blockingInstances(instances, minimums, Date.now());

  return (
    <div className="note crit" role="alert">
      <strong>{messageFor(error)}</strong>
      <ul>
        {error.details.map((d, i) => (
          <li key={i}>{d.message}</li>
        ))}
      </ul>
      {readError ? (
        <p>网关实例读取失败：{readError}</p>
      ) : blockers === null ? (
        <p className="hint">正在读取网关实例…</p>
      ) : blockers.length > 0 ? (
        <>
          <p>需要先升级这些网关实例：</p>
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
        <p>实例列表里没有低于该版本的实例；按上面的说明处理。</p>
      )}
      <p className="hint">最近 24 小时内上报过的网关实例都会计入，离线的也算。升级后重试此操作。</p>
    </div>
  );
}
