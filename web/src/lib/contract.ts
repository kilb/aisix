/**
 * 这份界面所期望的后端接口契约版本。
 *
 * 必须与 `crates/aisix-console/src/main.rs` 里的 `API_CONTRACT_VERSION`
 * 相等。Rust 侧那个是权威定义，这里是镜像；仓库里有一条测试钉住两者一致，
 * 所以任何一个提交都是自洽的 —— 运行时对不上就只可能是部署偏移。
 *
 * 改这个数字的时机和那边一样：接口真的变了。不要跟着版本号或构建时间走。
 */
export const EXPECTED_API_CONTRACT = 1;
