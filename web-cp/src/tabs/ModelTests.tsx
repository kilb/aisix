import "./operations.css";
import "./ModelTests.css";
import { useEffect, useState } from "react";
import type { TabProps } from "../App";
import { DirectModelProbe } from "../components/DirectModelProbe";
import { ModelCapabilityAssessment } from "../components/ModelCapabilityAssessment";
import { PageHeader } from "../components/page";
import { probeDirectModel } from "../lib/directModelProbeApi";
import { useResource } from "../lib/useResource";
import { Button } from "../ui/Button";
import type { AccountDoc, ModelDoc } from "./models/catalog";

/** 目标来自完整资源集合；页内切换只改变可见面板，离开整页才结束未发队列。 */
export function ModelTests({ operator, initialSection, sectionVisit, onGoto }: TabProps) {
  const models = useResource<ModelDoc>("models");
  const accounts = useResource<AccountDoc>("provider_keys");
  const canAssess = operator.role === "admin";
  type Section = "current" | "history" | "assessment";
  const requested = initialSection === "history" ? "history" : initialSection === "assessment" && canAssess ? "assessment" : "current";
  const [section, setSection] = useState<Section>(requested);
  useEffect(() => { setSection(requested); }, [requested, sectionVisit]);
  const sections: Section[] = canAssess ? ["current", "history", "assessment"] : ["current", "history"];
  function select(next: Section): void {
    setSection(next);
    onGoto("model-tests", next);
  }
  return <div className="cp-operations cp-modeltests">
    <PageHeader eyebrow="MODELS · 模型测试" title="模型测试" dataLead="显式发起直连测试，查看进度与已保存历史。测试可能产生上游费用。" />
    <div className="model-tests-tabs flex flex-wrap" role="tablist" aria-label="模型测试视图">
      {(["current", "history", "assessment"] as const).map((value) => <Button
        key={value} type="button" role="tab" id={`model-tests-${value}-tab`}
        kind="quiet"
        aria-selected={section === value} aria-controls={`model-tests-${value}`}
        tabIndex={section === value ? 0 : -1}
        disabled={value === "assessment" && !canAssess}
        title={value === "assessment" && !canAssess ? "仅管理员可读取和发起能力对照" : undefined}
        onClick={() => select(value)}
        onKeyDown={(event) => {
          if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) {
            event.preventDefault();
            const index = sections.indexOf(value);
            const next = event.key === "Home" ? sections[0]! : event.key === "End" ? sections[sections.length - 1]! : sections[(index + (event.key === "ArrowRight" ? 1 : -1) + sections.length) % sections.length]!;
            select(next);
            document.getElementById(`model-tests-${next}-tab`)?.focus();
          }
        }}
      >{value === "current" ? "当前测试" : value === "history" ? "测试历史" : "能力对照"}</Button>)}
    </div>
    {!canAssess && <p className="hint">能力对照的计划、运行、历史与导出仅管理员可用。</p>}
    <div hidden={section === "assessment"}>
      <DirectModelProbe models={models.items} accounts={accounts.items} loadError={models.error !== null} canProbe={operator.role === "admin"} probe={probeDirectModel} section={section === "history" ? "history" : "current"} />
    </div>
    {section === "assessment" && canAssess && <ModelCapabilityAssessment models={models.items} accounts={accounts.items} loadError={models.error !== null || accounts.error !== null} />}
  </div>;
}
