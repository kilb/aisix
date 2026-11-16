import "./FinanceWorkspace.css";
import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";
import { Totals } from "./Totals";
import { Accounts } from "./billing/Accounts";
import { Reconciliation } from "./billing/Reconciliation";
import { Repricing } from "./billing/Repricing";
import { Topups } from "./billing/Topups";

/** The sections of 「计费」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [
  { id: "accounts", label: "账户", Component: Accounts, minRole: "viewer" },
  { id: "topups", label: "充值审批", Component: Topups, minRole: "viewer" },
  { id: "repricing", label: "重新计价", Component: Repricing, minRole: "viewer" },
  { id: "reconciliation", label: "对账", Component: Reconciliation, minRole: "viewer" },
  { id: "profit", label: "本利汇算", Component: Totals, minRole: "viewer" },
];

export function Billing(props: TabProps) {
  return <div className="finance-workspace billing-workspace"><SectionHost label="计费" sections={SECTIONS} {...props} /></div>;
}
