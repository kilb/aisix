import type { Operator } from "../lib/api";
import { Button } from "../ui/Button";
import { ChangePassword } from "./ChangePassword";
import { Card, CardHeader, PageHeader } from "./page";
import "./AccountSettings.css";

/** 个人登录资料与平台人员管理分开，所有已登录角色均可维护自己的口令。 */
export function AccountSettings({ operator, roleLabel, onSignOut }: {
  operator: Operator;
  roleLabel: string;
  onSignOut: () => void;
}) {
  return <div className="cp-account-settings">
    <PageHeader title="账号设置" description="查看你的登录资料，管理口令与当前会话。" />
    <Card className="account-settings-sheet">
      <section className="account-profile" aria-label="登录资料">
        <CardHeader title="登录资料" description="此处为你当前使用的运维账号。" />
        <dl className="account-profile-list">
          <div><dt>显示名称</dt><dd>{operator.display_name || "未设置"}</dd></div>
          <div><dt>邮箱</dt><dd>{operator.email}</dd></div>
          <div><dt>角色</dt><dd>{roleLabel}</dd></div>
        </dl>
      </section>
      <ChangePassword />
      <section className="account-session" aria-label="当前会话">
        <CardHeader title="当前会话" description="在共用设备上完成工作后，请退出登录。" actions={<Button onClick={onSignOut}>登出</Button>} />
      </section>
    </Card>
  </div>;
}
