import type { Operator } from "../lib/api";
import { Button } from "../ui/Button";
import { ChangePassword } from "./ChangePassword";
import { Card, CardHeader, DescList, PageHeader } from "./page";

/** 个人登录资料与平台人员管理分开，所有已登录角色均可维护自己的口令。 */
export function AccountSettings({ operator, roleLabel, onSignOut }: {
  operator: Operator;
  roleLabel: string;
  onSignOut: () => void;
}) {
  return <>
    <PageHeader title="账号设置" description="查看你的登录资料，管理口令与当前会话。" />
    <div className="account-settings-grid">
      <Card className="account-profile">
        <CardHeader title="登录资料" description="此处为你当前使用的运维账号。" />
        <DescList items={[
          { label: "显示名称", value: operator.display_name || "未设置" },
          { label: "邮箱", value: operator.email },
          { label: "角色", value: roleLabel },
        ]} className="account-profile-list tablet:grid-cols-1" />
      </Card>
      <ChangePassword />
      <Card className="account-session">
        <CardHeader title="当前会话" description="在共用设备上完成工作后，请退出登录。" actions={<Button onClick={onSignOut}>登出</Button>} />
      </Card>
    </div>
  </>;
}
