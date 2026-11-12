import type { TabProps } from "../App";
import { Charges } from "./billing/Charges";

/** 沿用计费尝试粒度，不将一页记录误作最终请求统计。 */
export function Calls(props: TabProps) {
  return <Charges {...props} />;
}
