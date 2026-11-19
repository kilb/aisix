import { useEffect, useState, type FormEvent } from "react";
import { Card, CardFooter, CardHeader, CardMessage, span } from "./components/page";
import { ApiError, call } from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";
import { isCnyAmount, microCnyToInput } from "./lib/money";
import { Button } from "./ui/Button";
import { Loading } from "./ui/DataTable";
import { Field, useRefusalFocus } from "./ui/Field";
import { useToast } from "./ui/Toast";

interface AlertSettings {
  low_balance_micro_cny: number | null;
  budget_percent: number | null;
  spend_spike_percent: number | null;
  email: string | null;
  webhook_configured: boolean;
}

const endpoint = "/billing/alerts";

export function BudgetAlerts() {
  const [settings, setSettings] = useState<AlertSettings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let live = true;
    void call<AlertSettings>("GET", endpoint).then(
      ({ data }) => { if (live) setSettings(data); },
      (error: unknown) => {
        if (live) setLoadError(error instanceof ApiError ? messageFor(error) : String(error));
      },
    );
    return () => { live = false; };
  }, [attempt]);

  return (
    <Card role="region" aria-label="预算通知" className={span.d12}>
      <CardHeader title="预算通知" />
      {loadError ? (
        <div role="alert">
          <CardMessage onRetry={() => { setLoadError(null); setAttempt((value) => value + 1); }}>
            {loadError}
          </CardMessage>
        </div>
      ) : settings === null ? <Loading /> : <AlertsForm initial={settings} />}
    </Card>
  );
}

function AlertsForm({ initial }: { initial: AlertSettings }) {
  const [balance, setBalance] = useState(initial.low_balance_micro_cny === null ? "" : microCnyToInput(initial.low_balance_micro_cny));
  const [budget, setBudget] = useState(initial.budget_percent?.toString() ?? "");
  const [spike, setSpike] = useState(initial.spend_spike_percent?.toString() ?? "");
  const [email, setEmail] = useState(initial.email ?? "");
  const [webhook, setWebhook] = useState("");
  const [webhookConfigured, setWebhookConfigured] = useState(initial.webhook_configured);
  const [removeWebhook, setRemoveWebhook] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [fields, setFields] = useState<Record<string, string>>({});
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const toast = useToast();

  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const amount = balance.trim();
    setError(null);
    setFields({});
    if (amount && !isCnyAmount(amount, { allowZero: true })) {
      setFields({ "/low_balance_micro_cny": "请输入有效金额，最多两位小数" });
      focusRefusal();
      return;
    }
    const [yuan = "", fen = ""] = amount.split(".");
    setBusy(true);
    try {
      const { data } = await call<AlertSettings>("PUT", endpoint, {
        low_balance_micro_cny: amount ? Number(yuan + fen.padEnd(2, "0")) * 10_000 : null,
        budget_percent: budget === "" ? null : Number(budget),
        spend_spike_percent: spike === "" ? null : Number(spike),
        email: email.trim() || null,
        ...(removeWebhook ? { webhook_url: null } : webhook.trim() ? { webhook_url: webhook.trim() } : {}),
      });
      setBalance(data.low_balance_micro_cny === null ? "" : microCnyToInput(data.low_balance_micro_cny));
      setEmail(data.email ?? "");
      setWebhookConfigured(data.webhook_configured);
      setWebhook("");
      setRemoveWebhook(false);
      toast("通知设置已保存");
    } catch (e) {
      if (e instanceof ApiError && e.details.length > 0) {
        setFields(Object.fromEntries(e.details.map(({ path, message }) => [path, message])));
      } else {
        setError(e instanceof ApiError ? formMessageFor(e) : String(e));
      }
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  return (
    <form method="post" className="tenant-form grid min-w-0 gap-4" ref={form} onSubmit={(event) => void save(event)}>
      <p className="text-caption text-fg-3">填写阈值启用相应提醒，留空关闭。提醒通过配置的邮箱或 Webhook 发送。</p>
      <fieldset disabled={busy} className="grid min-w-0 gap-4 tablet:grid-cols-3">
        <Field label="可用额度低于或等于（元）" hint="余额加信用额度" error={fields["/low_balance_micro_cny"]}>
          {(aria) => <input inputMode="decimal" value={balance} onChange={(event) => setBalance(event.target.value)} {...aria} />}
        </Field>
        <Field label="组织预算使用率（%）" hint="1–100，任一平台设置的组织预算达到该比例时触发" error={fields["/budget_percent"]}>
          {(aria) => <input type="number" min={1} max={100} step={1} value={budget} onChange={(event) => setBudget(event.target.value)} {...aria} />}
        </Field>
        <Field label="消费突增阈值（%）" hint="101–1000，今日已结算消费与过去 7 个 UTC 自然日的日均消费相比" error={fields["/spend_spike_percent"]}>
          {(aria) => <input type="number" min={101} max={1000} step={1} value={spike} onChange={(event) => setSpike(event.target.value)} {...aria} />}
        </Field>
      </fieldset>
      <fieldset disabled={busy} className="grid min-w-0 gap-4 tablet:grid-cols-2">
        <Field label="收件邮箱" hint="填写本组织所有者或管理员的邮箱，由平台发信服务投递" error={fields["/email"]}>
          {(aria) => <input type="email" autoComplete="off" value={email} onChange={(event) => setEmail(event.target.value)} {...aria} />}
        </Field>
        <div className="grid gap-2">
          <Field label="Webhook 地址" hint={webhookConfigured ? "新地址将替换原地址，留空保留" : "使用默认端口的公网 HTTPS 地址"} error={fields["/webhook_url"]}>
            {(aria) => <input type="password" autoComplete="new-password" value={webhook} disabled={removeWebhook} onChange={(event) => setWebhook(event.target.value)} {...aria} />}
          </Field>
          {webhookConfigured && (
            <div className="flex flex-wrap items-center gap-x-4 gap-y-2 text-caption text-fg-3">
              <span>已配置 Webhook</span>
              <label className="inline-flex items-center gap-2">
                <input type="checkbox" checked={removeWebhook} onChange={(event) => setRemoveWebhook(event.target.checked)} />
                移除已配置的 Webhook
              </label>
            </div>
          )}
        </div>
      </fieldset>
      {error && <div className="note crit" role="alert">{error}</div>}
      <CardFooter className="tenant-form-actions">
        <Button kind="primary" type="submit" busy={busy} busyLabel="保存中…">保存通知设置</Button>
      </CardFooter>
    </form>
  );
}
