import type { Rollout } from "../lib/api";
import { Lamp, type LampState } from "../ui/Lamp";

/** The lamp each rollout state shows; a state a newer control plane added is grey. */
const LAMP: Record<string, LampState> = {
  pending: "pending",
  propagating: "propagating",
  applied: "applied",
  rejected: "rejected",
  no_gateways: "disabled",
};

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
  return (
    <span className="rollout" data-state={state}>
      <Lamp state={LAMP[state] ?? "disabled"}>{label}</Lamp>
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
