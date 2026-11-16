# SQLite 安全依赖回补

`sqlx-sqlite-0.8.6/` 完整保留 crates.io 发布包的文件（含 MIT、Apache-2.0 许可证和 `.cargo_vcs_info.json`），只修改正规化的 `Cargo.toml` 中 `libsqlite3-sys` 依赖：`0.30.1` → `=0.39.1`。`Cargo.toml.orig` 和所有 Rust 源码保持发布包原始字节；独立补丁见 [sqlx-sqlite-0.8.6.patch](sqlx-sqlite-0.8.6.patch)。根 manifest 的 `[patch.crates-io]` 让所有 SQLx 使用同一个驱动，SQLx 本身仍为 0.8.6。

来源：

- 官方发布包：<https://static.crates.io/crates/sqlx-sqlite/sqlx-sqlite-0.8.6.crate>
- 发布包 SHA-256：`c2d12fe70b2c1b4401038055f90f151b78208de1f9f89a7dbfd41587a10c3eea`
- 发布包记录的源码提交：`bab1b022bd56a64f9a08b46b36b97c5cff19d77e`，目录 `sqlx-sqlite`。
- SQLite 绑定官方发布包：<https://static.crates.io/crates/libsqlite3-sys/libsqlite3-sys-0.39.1.crate>
- 绑定包 SHA-256：`14a5f01b87b92705d07b433c06f96cbc764637d81a0bffb5734ce17cf0f9a2b3`，由 Cargo.lock 校验。

原因与边界：

- <https://rustsec.org/advisories/RUSTSEC-2026-0459.html> 指出旧 bundled SQLite 在读取页面大小不一致的数据库与 WAL 文件时可能暴露未初始化内存；`libsqlite3-sys >=0.39.0` 修复。0.39.1 包含 SQLite 3.54.1，最低 Rust 版本为 1.88，低于本仓库工具链 1.93.1。
- 已发布 [SQLx 0.8.6](https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-sqlite/Cargo.toml#L53) 仅接受 `^0.30.1`，已发布 [0.9.0](https://github.com/launchbadge/sqlx/blob/v0.9.0/sqlx-sqlite/Cargo.toml#L56) 仅接受 `>=0.30.1,<0.38.0`，均不能直接解析已修复绑定版本。此回补不升级 SQLx，不引入其开发分支，也不改驱动功能代码。
- 固定为已审核的 0.39.1，避免 `cargo update` 自动选择未经验证的绑定版本。此 SQLite 回补仅升级 SQLite 的锁记录、切换驱动为本地来源，没有新增构建或运行依赖。
- 原有 `bundled`、`unlock_notify`、`pkg-config`、`vcpkg` 特征保持不变；0.39.1 bundled 构建仍启用 `SQLITE_ENABLE_COLUMN_METADATA`，驱动使用的列元数据与 unlock-notify 接口继续存在。以真实文件 SQLite 的 standalone store 和完整工作区/真实 Postgres 测试验证链接及行为。

后续发布的 SQLx 若同时接受已修复绑定、通过上述测试，可移除本地补丁；在此之前不要恢复旧漏洞豁免。
