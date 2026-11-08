import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";
import { Accounts } from "./billing/Accounts";

/** The sections of 「计费」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [{ id: "accounts", label: "账户", Component: Accounts, minRole: "viewer" }];

export function Billing(props: TabProps) {
  return <SectionHost label="计费" sections={SECTIONS} {...props} />;
}
