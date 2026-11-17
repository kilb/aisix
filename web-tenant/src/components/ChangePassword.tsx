import { useState } from "react";
import { ApiError, changePassword } from "../lib/api";
import { messageFor } from "../lib/errors";
import { Button } from "../ui/Button";
import { Field, useRefusalFocus } from "../ui/Field";
import { Card, CardHeader } from "./page";

/** 个人安全设置仅挂在账号页；修改后其他会话退出，当前会话保留。 */
export function ChangePassword({ onClose, minimumLength, username }: { onClose?: () => void; minimumLength?: number; username: string }) {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [again, setAgain] = useState("");
  const [failure, setFailure] = useState<string | null>(null);
  const [currentProblem, setCurrentProblem] = useState<string | null>(null);
  const [nextProblem, setNextProblem] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const [busy, setBusy] = useState(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const mismatch = again !== "" && again !== next;

  async function submit(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault();
    if (busy) return;
    const data = new FormData(e.currentTarget);
    const enteredCurrent = String(data.get("current_password") ?? "");
    const enteredNext = String(data.get("new_password") ?? "");
    const enteredAgain = String(data.get("confirm_password") ?? "");
    // 用提交时的控件值校验自动填充的口令，与实际请求保持一致。
    setCurrent(enteredCurrent);
    setNext(enteredNext);
    setAgain(enteredAgain);
    if (enteredAgain !== enteredNext) {
      focusRefusal();
      return;
    }
    setFailure(null);
    setCurrentProblem(null);
    setNextProblem(null);
    if (minimumLength && [...enteredNext].length < minimumLength) {
      setNextProblem(`新口令至少需要 ${minimumLength} 个字符`);
      focusRefusal();
      return;
    }
    setBusy(true);
    try {
      await changePassword(enteredCurrent, enteredNext);
      setCurrent("");
      setNext("");
      setAgain("");
      setDone(true);
    } catch (err) {
      if (err instanceof ApiError && err.code === "invalid_credentials") {
        setCurrentProblem("当前口令不正确");
        setFailure("当前口令不正确");
        focusRefusal();
      } else if (err instanceof ApiError) {
        const problems = err.details.filter((d) => d.path === "/new_password").map((d) => d.message);
        setNextProblem(problems.length > 0 ? problems.join("；") : null);
        setFailure(messageFor(err));
        focusRefusal();
      } else {
        setFailure(err instanceof Error ? err.message : String(err));
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card className="account-security">
      <CardHeader title="修改口令" description="修改后将退出其他浏览器和设备上的登录，当前会话保留。" />
      {done ? <>
        <div className="note ok m-0" role="status">口令已修改，此账号在其他浏览器或设备上的会话已全部退出。</div>
        <div><Button onClick={onClose ?? (() => setDone(false))}>{onClose ? "关闭" : "继续修改"}</Button></div>
      </> : <form id="tenant-change-password" name="tenant-change-password" method="post" ref={form} className="account-password-form tenant-form" aria-label="修改口令" onSubmit={(e) => void submit(e)}>
        <input type="hidden" id="password-username" name="username" autoComplete="username" value={username} />
        <Field label="当前口令" error={currentProblem}>
          {(aria) => <input {...aria} id="current-password" name="current_password" type="password" autoComplete="current-password" required value={current}
            onChange={(e) => setCurrent(e.target.value)} />}
        </Field>
        <Field label="新口令" error={nextProblem} hint={minimumLength ? `至少 ${minimumLength} 个字符` : undefined}>
          {(aria) => <input {...aria} id="new-password" name="new_password" type="password" autoComplete="new-password" required value={next}
            onChange={(e) => setNext(e.target.value)} />}
        </Field>
        <Field label="确认新口令" error={mismatch ? "两次输入的新口令不一致" : null}>
          {(aria) => <input {...aria} id="confirm-password" name="confirm_password" type="password" autoComplete="new-password" required value={again}
            onChange={(e) => setAgain(e.target.value)} />}
        </Field>
        {failure && <div className="note crit" role="alert">{failure}</div>}
        <div className="account-form-actions">
          {onClose && <Button onClick={onClose} disabled={busy}>取消</Button>}
          <Button kind="primary" type="submit" busy={busy} busyLabel="保存新口令">保存新口令</Button>
        </div>
      </form>}
    </Card>
  );
}
