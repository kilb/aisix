import type { CooldownSettings } from "../lib/api";
import { useState } from "react";
import { Button } from "./Button";

export function CooldownEditor({ value, disabled, onChange, onValidityChange, error }: {
  value: CooldownSettings | null;
  disabled: boolean;
  onChange: (next: CooldownSettings | null) => void;
  error?: string | null;
  onValidityChange?: (problem: string | null) => void;
}) {
  const enabled = value?.enabled === true;
  const [invalid, setInvalid] = useState<Partial<Record<"default_seconds" | "max_seconds", string>>>({});
  const problem = Object.keys(invalid).length > 0 ? "冷却秒数必须是可以精确表示的整数（不超过 9007199254740991）；默认秒数不小于 0，上限秒数不小于 1。" : null;
  function number(field: "default_seconds" | "max_seconds", text: string) {
    const parsed = Number(text);
    const valid = text === "" || (Number.isSafeInteger(parsed) && parsed >= (field === "max_seconds" ? 1 : 0));
    const next = { ...invalid };
    if (valid) delete next[field];
    else next[field] = text;
    setInvalid(next);
    onValidityChange?.(Object.keys(next).length === 0 ? null : "请修正冷却秒数后再保存");
    if (valid) onChange({ ...value, [field]: text === "" ? undefined : parsed });
  }
  return (
    <div className="rounded-cell border border-line bg-well/40 p-3">
      <label className="check min-h-11 text-caption"><input type="checkbox" checked={enabled} disabled={disabled} onChange={(e) => { setInvalid({}); onValidityChange?.(null); onChange({ ...value, enabled: e.target.checked }); }} />故障冷却</label>
      <div className="grid gap-3 @md:grid-cols-2">
        <label className="field text-caption">默认冷却秒数<input type="number" min={0} max={Number.MAX_SAFE_INTEGER} step={1} disabled={disabled || !enabled} value={invalid.default_seconds ?? value?.default_seconds ?? ""} placeholder="30" onChange={(e) => number("default_seconds", e.target.value)} /></label>
        <label className="field text-caption">Retry-After 上限秒数<input type="number" min={1} max={Number.MAX_SAFE_INTEGER} step={1} disabled={disabled || !enabled} value={invalid.max_seconds ?? value?.max_seconds ?? ""} placeholder="600" onChange={(e) => number("max_seconds", e.target.value)} /></label>
      </div>
      <p className="hint mt-2 text-caption">显式开启才生效，留空用 30 秒与 600 秒。仅记住当前网关进程中这个账号对此模型的故障；有其他健康目标时暂避它，全部目标都在冷却时仍会尝试。</p>
      {value && !disabled && <Button kind="quiet" size="sm" type="button" onClick={() => { setInvalid({}); onValidityChange?.(null); onChange(null); }}>清除冷却配置</Button>}
      <details className="mt-2 text-caption text-ink-soft"><summary>高级规则</summary><p className="mt-2">默认在 401、408、429、500、502、503、504、超时和传输失败后冷却，并尊重上游 Retry-After。上限仅限制 Retry-After，不限制默认冷却时长。已有高级配置会原样保留。后台健康检查的“全部不可用”策略与此独立。</p>{value && <pre className="mt-2 overflow-auto text-meta">{JSON.stringify(value, null, 2)}</pre>}</details>
      {error && <p className="field-problem" role="alert">{error}</p>}
      {problem && <p className="field-problem" role="alert">{problem}</p>}
    </div>
  );
}
