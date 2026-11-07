import { useId, useState } from "react";
import { ApiError, changePassword } from "../lib/api";
import { messageFor } from "../lib/errors";

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
      } else {
        setFailure(err instanceof Error ? err.message : String(err));
      }
    } finally {
      setBusy(false);
    }
  }

  if (done) {
    return (
      <div className="panel">
        <div className="note" role="status">
          口令已修改，此账号在其他浏览器或设备上的会话已全部退出。
        </div>
        <div className="r">
          <button type="button" className="ghost" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    );
  }

  return (
    <form className="panel" aria-label="修改口令" onSubmit={(e) => void submit(e)}>
      <h2>修改口令</h2>
      <p className="hint">修改后，此账号在其他浏览器或设备上的会话都会退出，当前会话保留。</p>
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
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      <div className="r">
        <button type="submit" className="act" disabled={busy || mismatch}>
          保存新口令
        </button>
        <button type="button" className="ghost" onClick={onClose}>
          取消
        </button>
      </div>
    </form>
  );
}
