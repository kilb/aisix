import type { Rollout } from "../lib/api";

/**
 * Where a saved document stands on its way to the gateways. `data-state`
 * carries the raw state; a rejected document lists each gateway's reason.
 */
export function RolloutBadge({ rollout }: { rollout: Rollout }) {
  const { state } = rollout;
  const label =
    state === "pending"
      ? "待下发"
      : state === "propagating"
        ? `下发中 ${rollout.applied}/${rollout.online}`
        : state === "applied"
          ? "已生效"
          : state === "rejected"
            ? "被拒绝"
            : state === "no_gateways"
              ? "无在线网关"
              : // A state a newer control plane added: shown as it is named.
                String(state);
  const tone = state === "applied" ? "ok" : state === "rejected" ? "crit" : "";
  return (
    <span className="rollout" data-state={state}>
      <span className={`pill ${tone}`}>
        <span className="dot" />
        {label}
      </span>
      {state === "rejected" && rollout.rejections.length > 0 && (
        <ul className="rollout-errors">
          {rollout.rejections.map((r) => (
            <li key={r.instance_id}>
              <code>{r.hostname || r.instance_id}</code>：{r.error}
            </li>
          ))}
        </ul>
      )}
    </span>
  );
}
