# JW-04-d2-a：可选文件检查点存储

日期：2026-09-04。开发基线：dev / `2a0d223`。前一批 d1 已按用户要求提交并推送远端 dev，main 未变。本批进入 d2 的第一步；完整 JW-04-d2 / AT-F4-04 尚未验收。

## 实现

- 新增独立 `crates/budget/file` / jingwei-budget-file，主 workspace 现在有 18 个 crate。不进入默认 facade 依赖图，不给 jingwei-budget 增加 IO/Tokio，也不添加全局 provider。
- 实现 BudgetCheckpointStore 的 load/CAS，文件外层格式独立数字版本 1，逐条验证物理顺序、连续 revision、绑定和检查点不变量；缺失文件不是空任务，损坏文件不自动修复。
- 用独立文件 handle 的非阻塞 OS 锁协调独立实例和进程，锁内比较、追加并 sync_all；最新镜像精确重试不重复追加，load/重试重新同步确认完整记录。
- 定义 Busy/Closed/Corrupt/LimitExceeded 错误；写入前失败与 write/sync 后不确定分开，不伪装回滚。
- 默认单条 8 MiB、日志 64 MiB、4 个在途 IO 作业，Tokio blocking worker 拥有已接受的工作和容量；调用者离开仍可由 close 排空，所有 clone 共享关闭状态。
- 指南与源码隔离，所有新增测试放入独立、publish=false 的 tests/model-protocol。新增 [ADR-0008](../adr/0008-file-budget-checkpoints.md) 和 [文件存储指南](../../guide/src/file-checkpoint-store.md)。

## 验证

Windows x64/MSVC + Rust 1.96.0 完整基线：`scripts/check-baseline.ps1 -Jobs 4`，见 [汇总](../../../results/baseline/20260904-215346-777/summary.json)。9/9 检查通过，包括主工作区 fmt/check/test/严格 Clippy/rustdoc、facade 默认特性检查，以及独立工程 fmt/test/严格 Clippy。

| 覆盖 | 结果 |
| --- | --- |
| 原有主工作区测试 | 14 通过 |
| 独立契约测试 | 198 通过；原有 181 保留，新增 17 个文件存储测试入口（含 1 个子进程工作入口） |
| 指南 doctest | 10 通过；新增 1 项文件适配器教学片段 |
| 合计 | 222 通过，0 失败 |

最终文档调整后再次运行独立工程 doctest，10/10 通过；11 份相关文档的本地文件链接检查通过（支持绝对路径与行号），Git 差异空白检查通过。

新增测试覆盖实际文件的 CAS/最新加载/精确重试、错误身份与版本、完整但无回执记录、损坏/半行/不连续版本、字节限制、缺失文件、不支持的调用环境、OS 锁、clone 关闭、未 poll 请求、丢弃调用者和关闭等待者后的受拥有作业排空。真正的子进程验证竞争写者唯一获胜、另一个新进程读取已有消耗、父进程锁可见，以及持锁测试进程被终止后锁释放。

没有注入实际 write/sync_all 失败，没有运行物理掉电/磁盘写满试验，没有在本轮重跑 Linux 或真实模型。这些测试不能替代完整 runtime 崩溃恢复验收。

两份 Cargo.lock 只增加本地新 crate 及其引用，没有改变第三方版本。新 crate 的 cargo package --list --allow-dirty --locked --offline 仅列出源码、README 和 Cargo 元数据（包含 Cargo 自动生成的锁文件），没有独立测试、指南或脚本；未做实际 .crate 解包消费。cargo tree 核对默认 facade 不依赖 jingwei-budget-file。

锁文件 SHA-256：主工作区 `98E25DEC5A6B84FF014AEA2E935855D9586FC85C782730A2BDFED20748B0B3BB`；独立测试 `C07D5470EB4F67C58D298CB69CD78D6AA5E40D5BB2D9CCCBF3215F37392702AF`。

## 下一步与异机接续

本批验证完成后，用户授权提交并推送 d2-a 到 dev，再进入 d2-b；实际提交以 Git 历史为准。提交包含本批源码、文档、锁文件及验证日志，target 不跟踪。

开发过程中发现 action-step.md 出现非本轮修改，已保留，没有自动修正或混入本批实现说明。

下一批 d2-b 首先设计持久 Task 执行所有权和运行前写前保护，接入 AgentRuntime admission 与收尾屏障，并核验 Session 日志/检查点一致性。文件 CAS 不提供持续执行租约，不能对两个相同 load 结果分别授权执行。增额审计和 runtime 故障窗口收口放在 d2-c；完整业务 Task、待答状态与工具幂等性仍归 JW-07。详见 [接续计划](JW-04-plan.md)。
