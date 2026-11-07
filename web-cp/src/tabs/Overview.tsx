import type { TabProps } from "../App";

/** Placeholder until the overview (environment, instances, rollout) is built. */
export function Overview({ operator }: TabProps) {
  return (
    <div className="panel">
      <h2>概览</h2>
      <p className="hint">已登录：{operator.email}。运行概况将在这里显示。</p>
    </div>
  );
}
