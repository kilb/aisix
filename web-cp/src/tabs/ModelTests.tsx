import { useEffect, useState } from "react";
import type { TabProps } from "../App";
import { DirectModelProbe } from "../components/DirectModelProbe";
import { PageHeader } from "../components/page";
import { probeDirectModel } from "../lib/directModelProbeApi";
import { useResource } from "../lib/useResource";
import { Button } from "../ui/Button";
import type { AccountDoc, ModelDoc } from "./models/catalog";

/** 目标来自完整资源集合；页内切换只改变可见面板，离开整页才结束未发队列。 */
export function ModelTests({ operator, initialSection, sectionVisit, onGoto }: TabProps) {
  const models = useResource<ModelDoc>("models");
  const accounts = useResource<AccountDoc>("provider_keys");
  const [section, setSection] = useState<"current" | "history">(initialSection === "history" ? "history" : "current");
  useEffect(() => { setSection(initialSection === "history" ? "history" : "current"); }, [initialSection, sectionVisit]);
  function select(next: "current" | "history"): void {
    setSection(next);
    onGoto("model-tests", next);
  }
  return <>
    <PageHeader eyebrow="MODELS · 模型测试" title="模型测试" dataLead="显式发起直连测试，查看进度与已保存历史。测试可能产生上游费用。" />
    <div className="mb-4 flex flex-wrap gap-2" role="tablist" aria-label="模型测试视图">
      {(["current", "history"] as const).map((value) => <Button
        key={value} type="button" role="tab" id={`model-tests-${value}-tab`}
        kind={section === value ? "primary" : "secondary"}
        aria-selected={section === value} aria-controls={`model-tests-${value}`}
        tabIndex={section === value ? 0 : -1}
        onClick={() => select(value)}
        onKeyDown={(event) => {
          if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) {
            event.preventDefault();
            const next = event.key === "Home" ? "current" : event.key === "End" ? "history" : value === "current" ? "history" : "current";
            select(next);
            document.getElementById(`model-tests-${next}-tab`)?.focus();
          }
        }}
      >{value === "current" ? "当前测试" : "测试历史"}</Button>)}
    </div>
    <DirectModelProbe models={models.items} accounts={accounts.items} loadError={models.error !== null} canProbe={operator.role === "admin"} probe={probeDirectModel} section={section} />
  </>;
}
