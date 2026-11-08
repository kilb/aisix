import type { TabProps } from "../App";
import { SectionHost, type Section } from "../components/SectionHost";
import { Catalogue } from "./pricing/Catalogue";
import { Charges } from "./pricing/Charges";

/** The sections of 「价格」, in order. Each task appends its own entry. */
export const SECTIONS: Section[] = [{ id: "catalogue", label: "价格目录", Component: Catalogue, minRole: "viewer" },
  { id: "charges", label: "计费明细", Component: Charges, minRole: "viewer" },
];

export function Pricing(props: TabProps) {
  return <SectionHost label="价格" sections={SECTIONS} {...props} />;
}
