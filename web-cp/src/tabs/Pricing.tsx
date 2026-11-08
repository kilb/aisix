import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";
import { SellPrices } from "./pricing/SellPrices";
import { TenantPlans } from "./pricing/TenantPlans";
import { CostPrices } from "./pricing/CostPrices";

/** The sections of 「价格」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [{ id: "sell", label: "平台价格", Component: SellPrices, minRole: "viewer" },
  { id: "plans", label: "租户方案", Component: TenantPlans, minRole: "viewer" },
  { id: "cost", label: "成本价", Component: CostPrices, minRole: "viewer" },
];

export function Pricing(props: TabProps) {
  return <SectionHost label="价格" sections={SECTIONS} {...props} />;
}
