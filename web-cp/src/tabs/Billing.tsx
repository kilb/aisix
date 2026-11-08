import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";

/** The sections of 「计费」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [];

export function Billing(props: TabProps) {
  return <SectionHost label="计费" sections={SECTIONS} {...props} />;
}
