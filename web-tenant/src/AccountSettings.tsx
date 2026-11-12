import { MIN_PASSWORD_CHARS, type SessionMember } from "./lib/api";
import { ROLE_LABEL } from "./Org";
import { Button } from "./ui/Button";
import { ChangePassword } from "./components/ChangePassword";
import { Card, CardHeader, DescList, PageHeader } from "./components/page";

/** 个人账号入口独立于组织的账务账户和组织启停状态。 */
export function AccountSettings({ member, onSignOut }: { member: SessionMember; onSignOut: () => void }) {
  return <>
    <PageHeader title="账号设置" description="查看你的登录资料，管理口令与当前会话。" />
    <div className="account-settings-grid">
      <Card className="account-profile">
        <CardHeader title="登录资料" description="此处为你当前使用的组织成员账号。" />
        <DescList items={[
          { label: "显示名称", value: member.display_name || "未设置" },
          { label: "邮箱", value: member.email },
          { label: "角色", value: ROLE_LABEL[member.role] ?? member.role },
        ]} className="account-profile-list tablet:grid-cols-1" />
      </Card>
      <ChangePassword minimumLength={MIN_PASSWORD_CHARS} />
      <Card className="account-session">
        <CardHeader title="当前会话" description="在共用设备上完成工作后，请退出登录。" actions={<Button onClick={onSignOut}>登出</Button>} />
      </Card>
    </div>
  </>;
}
