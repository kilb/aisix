import { call, type Member, type MemberRole } from "./api";

export interface NewMemberAccount {
  email: string;
  password: string;
  role: MemberRole;
  display_name?: string;
}

// 只由当前会话决定组织，初始密码不写入浏览器存储。
export async function createMemberAccount(body: NewMemberAccount): Promise<Member> {
  return (await call<Member>("POST", "/members", body)).data;
}
