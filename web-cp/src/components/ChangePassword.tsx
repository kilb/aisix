import { useId, useState } from "react";
import { ApiError, changePassword } from "../lib/api";
import { messageFor } from "../lib/errors";
import { Button } from "../ui/Button";
import { useRefusalFocus } from "../ui/Field";

/**
 * The signed-in operator's own password, opened from the header by every
 * role. A change ends the account's other sessions; this one stays.
 */
export function ChangePassword({ onClose }: { onClose: () => void }) {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [again, setAgain] = useState("");
  const [failure, setFailure] = useState<string | null>(null);
  const [nextProblem, setNextProblem] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const [busy, setBusy] = useState(false);
  const nextProblemId = useId();
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const againProblemId = useId();
  const mismatch = again !== "" && again !== next;

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (mismatch) return;
    setFailure(null);
    setNextProblem(null);
    setBusy(true);
    try {
      await changePassword(current, next);
      setCurrent("");
      setNext("");
      setAgain("");
      setDone(true);
    } catch (err) {
      if (err instanceof ApiError && err.code === "invalid_credentials") {
        // On this route the code means the current password, not the account.
        setFailure("当前口令不正确");
      } else if (err instanceof ApiError) {
        const atNext = err.details.filter((d) => d.path === "/new_password").map((d) => d.message);
        setNextProblem(atNext.length > 0 ? atNext.join("；") : null);
        setFailure(messageFor(err));
        focusRefusal();
      } else {
        setFailure(err instanceof Error ? err.message : String(err));
      }
    } finally {
      setBusy(false);
    }
  }

  if (done) {
    return (
      <div className="panel mb-3 flex flex-wrap items-center justify-between gap-3">
        <div className="note ok m-0 min-w-0 flex-1 text-body" role="status">
          口令已修改，此账号在其他浏览器或设备上的会话已全部退出。
        </div>
        <Button type="button" onClick={onClose}>
          关闭
        </Button>
      </div>
    );
  }

  return (
    <form ref={form} className="panel mb-3 max-w-none" aria-label="修改口令" onSubmit={(e) => void submit(e)}>
      <div className="grid max-w-none gap-x-7 gap-y-4 lg:grid-cols-[minmax(220px,1fr)_minmax(0,3fr)]">
        <div className="min-w-0">
          <h2 className="font-mono text-meta font-medium tracking-wide text-ink">修改口令</h2>
          <p className="hint mt-2 text-caption leading-relaxed text-ink-soft">修改后，此账号在其他浏览器或设备上的会话都会退出，当前会话保留。</p>
        </div>
        <div className="min-w-0">
          <div className="grid gap-x-4 md:grid-cols-3">
            <div className="min-w-0">
              <label className="f">
                <span>当前口令</span>
                <input
                  type="password"
                  autoComplete="current-password"
                  required
                  value={current}
                  onChange={(e) => setCurrent(e.target.value)}
                />
              </label>
            </div>
            <div className="min-w-0">
              <label className="f">
                <span>新口令</span>
                <input
                  type="password"
                  autoComplete="new-password"
                  required
                  value={next}
                  aria-invalid={nextProblem ? true : undefined}
                  aria-describedby={nextProblem ? nextProblemId : undefined}
                  onChange={(e) => setNext(e.target.value)}
                />
              </label>
              {nextProblem && (
                <p id={nextProblemId} className="field-problem">
                  {nextProblem}
                </p>
              )}
            </div>
            <div className="min-w-0">
              <label className="f">
                <span>确认新口令</span>
                <input
                  type="password"
                  autoComplete="new-password"
                  required
                  value={again}
                  aria-invalid={mismatch ? true : undefined}
                  aria-describedby={mismatch ? againProblemId : undefined}
                  onChange={(e) => setAgain(e.target.value)}
                />
              </label>
              {mismatch && (
                <p id={againProblemId} className="field-problem">
                  两次输入的新口令不一致
                </p>
              )}
            </div>
          </div>
          {failure && (
            <div className="note crit mt-0 text-body" role="alert">
              {failure}
            </div>
          )}
        </div>
      </div>
      <div className="-mx-[var(--pad)] -mb-[var(--pad)] mt-4 flex max-w-none flex-wrap justify-end gap-2 rounded-b-card border-t border-line bg-well px-[var(--pad)] py-3 text-ink-soft">
        <Button type="button" onClick={onClose}>
          取消
        </Button>
        <Button kind="primary" type="submit" disabled={mismatch} busy={busy} busyLabel="保存新口令">
          保存新口令
        </Button>
      </div>
    </form>
  );
}
