import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";
import { Topups } from "./billing/Topups";

/** The sections of 「计费」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [{ id: "topups", label: "充值审批", Component: Topups, minRole: "viewer" }];

export function Billing(props: TabProps) {
  return <SectionHost label="计费" sections={SECTIONS} {...props} />;
}
