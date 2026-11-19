# 租户门户认证

在管理后台的「平台管理 → 认证设置」保存配置。各项功能独立开启，密钥保存后只能替换或清除。门户保留口令登录。新建租户仍进入现有审核流程。

| 功能 | 配置与操作 |
| --- | --- |
| 邮箱验证码 | 填写发信接口、发件邮箱和 API Key。开启后，自助注册需要邮箱验证码。 |
| 口令找回 | 默认关闭。核对现有成员的登记邮箱由本人控制后，单独开启此项，并启用邮箱验证码。成员从登录页选择「忘记口令」，通过邮件验证码设置新口令。重置成功后，该成员的现有会话失效。 |
| Cloudflare Turnstile | 在 Cloudflare 创建 Widget，将门户主机名加入允许列表，填入 Site Key 和 Secret Key。开启后，公开登录、注册和验证码发送需要通过人机校验。 |
| Passkey | 保存门户的 HTTPS 源地址后开启。成员用口令登录，在「账号设置 → 登录方式」添加 Passkey，之后可输入账号邮箱并用该 Passkey 登录。变更门户主机名后，应在新站点重新添加凭据。 |
| GitHub | 创建 GitHub OAuth App。复制后台显示的回调地址到 OAuth App，并填入 Client ID 和 Client Secret。成员先在「账号设置 → 登录方式」绑定 GitHub，再通过登录页使用。 |
| Google | 配置 Google Auth Platform 的授权页面并创建 Web 应用客户端。将后台显示的回调地址加入 Authorized redirect URIs，填入 Client ID 和 Client Secret。测试模式下将验收者加入测试用户。成员先在「账号设置 → 登录方式」绑定 Google，再通过登录页使用。 |

[Turnstile 配置](https://developers.cloudflare.com/turnstile/get-started/) · [WebAuthn](https://www.w3.org/TR/webauthn-3/) · [GitHub OAuth App](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/creating-an-oauth-app) · [Google OAuth Web 应用](https://developers.google.com/identity/protocols/oauth2/web-server)

## 发信接口

发信接口应接受公网 HTTPS 的 `POST`，使用 `Authorization: Bearer <API Key>` 验证请求，接收以下 JSON 字段。

```json
{
  "from": "verification@example.com",
  "to": "user@example.net",
  "subject": "Routio 注册验证码",
  "text": "验证码邮件正文"
}
```

平台将 HTTP 2xx 视为发信服务已接收邮件。发信商使用其他请求格式时，可以由一个 HTTPS 转发服务转换格式。发件邮箱按选用服务的发件域名规则配置。同一接口也处理找回口令和组织用量提醒，邮件主题与正文随用途变化。验证码有效期为 10 分钟，使用后失效。

## 配置上线

先升级全部控制面实例和两个网页构建，再启用相应功能。若门户由反向代理或 CDN 设置内容安全策略，开启 Turnstile 前，在 `script-src` 和 `frame-src` 中允许 `https://challenges.cloudflare.com`。对 OAuth 回调路径，访问日志记录路径即可，省略查询参数。使用路径前缀部署门户时，把完整入口配置为 `CP_PORTAL_URL`，并使用后台显示的完整回调地址。 [Turnstile 内容安全策略](https://developers.cloudflare.com/turnstile/reference/content-security-policy/)

验收可从无痕窗口开始。发送一封验证码邮件并注册测试租户，分别验证通过和未通过人机校验的登录，再在账号设置添加 Passkey、绑定第三方账号并重新登录。
