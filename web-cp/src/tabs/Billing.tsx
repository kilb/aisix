import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";
import { Accounts } from "./billing/Accounts";
import { Charges } from "./billing/Charges";
import { Reconciliation } from "./billing/Reconciliation";
import { Repricing } from "./billing/Repricing";
import { Topups } from "./billing/Topups";

/** The sections of 「计费」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [
  { id: "accounts", label: "账户", Component: Accounts, minRole: "viewer" },
  { id: "charges", label: "计费明细", Component: Charges, minRole: "viewer" },
  { id: "topups", label: "充值审批", Component: Topups, minRole: "viewer" },
  { id: "repricing", label: "重新计价", Component: Repricing, minRole: "viewer" },
  { id: "reconciliation", label: "对账", Component: Reconciliation, minRole: "viewer" },
];

export function Billing(props: TabProps) {
  return <SectionHost label="计费" sections={SECTIONS} {...props} />;
}
