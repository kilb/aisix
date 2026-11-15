# 退役 API 测试归档（2026-11-15）

基底：`4b6cb5142d5e8eaa6cfeee1ee8d04706f29763f0`。本归档执行已确认的轻隔离范围调整。
`scope-superseded` 表示退出本轮支持集合，不代表原测试通过。

`retired-source/` 保留两个 Evals 独立测试目标及 handler/worker 的原始完整字节，位置不在 Cargo 自动发现路径。
`originals/` 保留混合文件修改前的完整字节；`parts/` 逐项记录被移出的函数、矩阵行、追加 GA 覆盖和哈希。
`inventory32.json` 对原清单 32 个文件逐一分类；额外发现的控制面 Evals 目标另列在 `parts/cp-evals-curation.json`。
在同一基底上可用原文件恢复；后续修改过的文件应按记录逐块还原，避免覆盖新工作。

活跃 Files/Batches、Moderations/Provenance 和其他兄弟路径的断言保留。没有新增 ignore/skip/only，没有更改业务状态码或期望值。
持久枚举、codec、共享协议/URL 纯单元测试保持；未修改模型、配额或授权产品逻辑。
三条 GA Realtime 路由补齐 metrics 和请求解码覆盖。唯一新增产品分类是将真实计量的
`GET /v1/realtime/translations` 放入 LLM 请求集合；两个 client-secrets POST 继续仅计管理请求。
同时移除两个已卸载退役路径的 LLM 分类死项。

请求解码原精确 census 还缺 51 个 CCV/Skills/Voices 活跃 method/path（静态复算 128 对 77）；原相等断言保留，不能将真实 JSON 消费者作为无体请求豁免。

五个视频活跃合同测试仍使用已停用的 OpenAI fixture，原场景和断言逐字节保留，列为未解决阻断。
14 个混合 TypeScript 文件未改；它们由独立 Vitest/CI E2E 任务发现，本提交不宣称完成其范围拆分或通过。
具体边界见外部 `h1-ts-mixed-e2e-readonly.json`，哈希写入总清单。

本 agent 未运行 Cargo、服务或测试。已执行直接 rustfmt 检查、git diff 空白检查、原始归档字节及保留视频场景哈希检查。
完整编译、列举、运行与真实 WS/metrics 验证由唯一 root runner 执行；不能将这些静态检查称为运行 GREEN。
