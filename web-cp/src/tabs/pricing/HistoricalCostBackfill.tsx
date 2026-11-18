import { useEffect, useRef, useState } from "react";
import { Card, CardHeader } from "../../components/page";
import { ApiError, costRates } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { CardFooter } from "./kit";

const initial = { examined: 0, filled: 0, after: undefined as string | undefined };
type Phase = "idle" | "running" | "done" | "retry" | "restart" | "forbidden";

export function HistoricalCostBackfill({ accountId }: { accountId: string }) {
  const [phase, setPhase] = useState<Phase>("idle");
  const [progress, setProgress] = useState(initial);
  const [error, setError] = useState("");
  const active = useRef<AbortController | null>(null);
  useEffect(() => () => active.current?.abort(), []);

  async function run() {
    if (active.current) return;
    const controller = new AbortController();
    active.current = controller;
    let page = phase === "retry" ? progress : initial;
    setProgress(page);
    setPhase("running");
    setError("");
    try {
      let after = page.after;
      if (after === undefined) {
        const prepared = await costRates.backfill(accountId, { prepare: true }, controller.signal);
        if (controller.signal.aborted) return;
        if (!prepared.next_after || prepared.examined !== 0 || prepared.filled !== 0) {
          throw new ApiError("控制面未返回有效的补填游标", 200, "bad_response", [], [], null);
        }
        after = prepared.next_after;
        setProgress({ ...page, after });
      }
      do {
        const result = await costRates.backfill(accountId, { after }, controller.signal);
        if (controller.signal.aborted) return;
        after = result.next_after ?? undefined;
        page = {
          examined: page.examined + result.examined,
          filled: page.filled + result.filled,
          after,
        };
        setProgress(page);
      } while (after !== undefined);
      setPhase("done");
    } catch (e) {
      if (controller.signal.aborted) return;
      if (e instanceof ApiError && e.code === "cost_backfill_stale") {
        setError("本次补填的费率版本已失效。请按当前换算率重新开始，已补填的记录会保留。");
        setPhase("restart");
      } else {
        setError(errorText(e));
        setPhase(e instanceof ApiError && e.status === 403 ? "forbidden" : "retry");
      }
    } finally {
      active.current = null;
    }
  }

  return (
    <Card role="group" aria-label="历史成本补填">
      <CardHeader title="历史成本补填" description="按开始补填时生效的模型成本换算率（无覆盖时取账号默认），结合调用当时的美元基准价，补填该账号历史调用中缺失的成本。已知成本、租户扣款和余额保持原值。" />
      {phase !== "idle" && (
        <p role="status" className="text-caption tabular">
          {phase === "done" ? "补填完成。" : phase === "running" ? "正在补填。" : "补填已暂停。"}
          本次已确认检查 {fmtInt(progress.examined)} 条，补填 {fmtInt(progress.filled)} 条。
        </p>
      )}
      {error && <p role="alert" className="note crit">{error}</p>}
      {phase !== "forbidden" && (
        <CardFooter>
          <Button onClick={run} busy={phase === "running"} busyLabel="补填中…">
            {phase === "retry" ? "继续补填" : phase === "restart" ? "重新开始补填" : "补填历史成本"}
          </Button>
        </CardFooter>
      )}
    </Card>
  );
}
