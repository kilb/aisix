import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";

/** The sections of 「价格」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [];

export function Pricing(props: TabProps) {
  return <SectionHost label="价格" sections={SECTIONS} {...props} />;
}
