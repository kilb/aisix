import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";
import { Groups } from "./pricing/Groups";
import { TenantPlans } from "./pricing/TenantPlans";
import { CostPrices } from "./pricing/CostPrices";
import { Catalogue } from "./pricing/Catalogue";
import { Charges } from "./pricing/Charges";
import { PricingStatus } from "./pricing/PricingStatus";
import { Baselines } from "./pricing/Baselines";
import { PricingImport } from "./pricing/PricingImport";

/** The sections of 「价格」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [{ id: "groups", label: "分组", Component: Groups, minRole: "viewer" },
  { id: "plans", label: "租户方案", Component: TenantPlans, minRole: "viewer" },
  { id: "cost", label: "成本价", Component: CostPrices, minRole: "viewer" },
  { id: "catalogue", label: "价格目录", Component: Catalogue, minRole: "viewer" },
  { id: "charges", label: "计费明细", Component: Charges, minRole: "viewer" },
  { id: "status", label: "计价状态", Component: PricingStatus, minRole: "viewer" },
  { id: "baselines", label: "基准价", Component: Baselines, minRole: "viewer" },
  { id: "imports", label: "导入审核", Component: PricingImport, minRole: "viewer" },
];

export function Pricing(props: TabProps) {
  return <SectionHost label="价格" sections={SECTIONS} {...props} />;
}
