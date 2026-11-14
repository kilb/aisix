# 用户澄清后的修正计划（2026-11-14）

实际需求是每次 LLM 上游尝试的输入/输出字节与文本长度，用于核验同次 Token 用量。以下步骤覆盖后续旧的客户端交换计划；旧实现尚不满足新口径。

1. 明确并绑定上游边界与版本 → 验证旧 `client_exchange` 数据不被误标，正文大小与文本长度分开。
2. 在最终上游请求及原始响应处采集，携带每次 attempt 的独立句柄 → 验证协议转换、护栏、重试和并发不会串计。
3. 按原用量事件时点附带快照，取消客户端完成依赖 → 验证完整/中断/缓存/未派发、流式首帧前失败及异步终态。
4. 同步控制面接收与展示，约束旧补注兼容路径 → 验证实际数据库、旧边界隔离与计价等价。
5. 重新完成真实后端界面验收、受影响 Rust 套件和独立审计 → 确认完整合同后再进入已有授权的制品构建与平滑部署流程。

---

# Usage IO Audit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. 按用户要求独立子任务并行，Root 独占 Cargo。

**Goal:** 保存客观输入输出长度，在控制面提供与报告用量独立的文本 Token 参考。

**Architecture:** aisix-core 共享可选测量合同；原用量事件按既有时点发布，HTTP 结束后仅在原事件 ID 上补充长度。CP 按认证 DP 归属安全合并元数据，不进入价格引擎。

**Tech Stack:** Rust、现有 Axum/serde、PostgreSQL JSONB、React、Playwright。

**Spec:** ../specs/2026-11-14-usage-io-audit-design.md

## Global Constraints

- 采用 `client_exchange`；version=1；零和缺失严格区分。
- 参考方法 utf8_bytes_div4_v1，仅未校准参考，不修改计费。
- 不保留原文、不依赖 exporter capture，不放宽内存测试。
- 无删除/推送/主分支提交；生产部署使用已授权的并行切换，不中断旧连接。

## Review Focus

- 非 ASCII 与 JSON escape：8 bytes/3 scalars 必须严格相等。
- SSE terminal 快照与 delta 重复：同一文字只计一次。
- 取消与未 poll body：终态事件恰好一次，部分和未知不变零。
- WS 并发轮次：逻辑文字独立，禁止合成 HTTP 字节。
- 上游估算/缓存来源：展示保持 provenance，价格不受审计字段影响。

## Task 1: DP 共享计量和生命周期

Files: aisix-core/src/usage_io.rs、aisix-obs/src/usage.rs；proxy 的新 usage_io.rs、attribution.rs、usage_attr.rs、lib.rs 与协议接线；必要测试。

Interfaces: 产生 spec 中 UsageIoAudit/TextMeasurement，UsageEvent.io_audit: Option<UsageIoAudit>。CP 可复用 Deserialize 类型，不改变既有字段。

- [ ] 写字段 presence、Unicode、协议槽、SSE 去重、终态/body/取消与 WS 回归；保存 tests-first 证据。
- [ ] Root 执行 RED 或隔离变异，确认计量/接线缺失会失败。
- [ ] 实现 bounded scalar observers；HTTP 真正结束后通过 opt-in 独立有界队列发送 metadata-only 补充。普通账单队列容量、worker、健康、计数和 exporter 保持；重试中间行不附整次交换。
- [ ] Root 执行组件/实际服务相关测试、原内存测试与计费等价。

## Task 2: CP 查询与两个前端详情

Files: control-plane/src/billing/calls.rs、必要 usage DTO/测试；web-cp 调用详情、web-tenant 用量详情及共享展示/类型。

Interfaces: 消费 UsageEvent.io_audit，投影可选测量；API 返回独立 `io_audit` 与版本化粗估展示，不改账单用量。

- [ ] 写旧记录缺失/真实零/Unicode/部分/usage_estimated/缓存口径的测试。
- [ ] 实现整数除法参考，详情可折叠展示完整边界与限制；沿用紧凑布局。
- [ ] 实现同源 DP 元数据补充接收与持久先到暂存；普通行先计价，随后只改 io_audit。重放、先后顺序、并发与外 DP 不能新建用量或改变 Token/价格。server worker 补充不计入普通用量投递/丢失/健康证明。
- [ ] typecheck/unit/build；Root 更新实际 QA backend 后用真实 intake 和 Playwright 验证 presence、数值与历史未知。

## Task 3: 整体验证与部署

- [ ] 处理当前全 workspace 实际失败，冻结最终源；fmt、CI clippy、实际 workspace all-features 与独立审计。
- [ ] 构建匹配生产 CPU/ABI 的带哈希原生二进制与两个前端；保留版本说明。
- [ ] 非清理式备份、CP-first 迁移、独立 DP 候选、Nginx graceful 切换；验证健康与 UI，旧连接自然排空。
- [ ] 当前版本部署通过后开始模型能力检测工具的设计与实现，报告不得把能力评分当身份密码学证明。
