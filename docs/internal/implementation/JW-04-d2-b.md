# JW-04-d2-b：持久预算运行占用与运行时接入

日期：2026-09-04。基线：dev / `96f8302`。按用户要求，已先将 d2-a 提交并推送 dev；远端 main 保持 `fd1afc7`。本批实现 [ADR-0009](../adr/0009-durable-budget-execution.md)，d2-c / AT-F4-04 / F6 尚未完成。

## 交付

- jingwei-budget 新增独立 execution 模块，仍无具体 IO/Tokio 依赖，不增加 crate 或默认 provider。BudgetExecutionLease 使用唯一 BudgetExecutionId，通过实际存储 CAS 确认冻结占用后才返回，不能 Clone。
- 新导出的检查点为数字版本 2，V1 普通镜像仍可读；V1 不得携带占用。新增占用保留旧安全边界，不能被 restore_checkpoint 恢复为可执行账本。
- AgentTurnRequest.with_durable_budget 显式移交租约；canonical runtime 在正文之前核验规范历史与检查点末尾，报告/终态/settle 后封存内存，再提交最终检查点。内存入口不改变为隐式持久化。
- 校验能力报告、确认事件配对与未确认计数，拒绝未决/不完整的关闭边界；不把 Session 与检查点当作原子双写。
- 成功的 AgentTurnReport 返回 budget_checkpoint 确认镜像。Durability 错误保留原始 Session outcome，CAS 失败保留原版本及确切候选镜像供精确核验。丢弃 completion 不丢失 IO 所有权；shutdown 等待最终提交并报告未送达的持久失败。
- 租约 Drop 封存所有内存 clone，但不释放持久占用，不假装撤销外部工作。拒绝准入、进程中断及不确定提交不提供无审计自动恢复。
- 新指南位于 [durable-budget.md](../../guide/src/durable-budget.md)，源码不依赖指南或独立测试。主 workspace 仍为 18 个 crate。

## 验证

最终 Windows x64/MSVC + Rust 1.96.0 基线：`scripts/check-baseline.ps1 -Jobs 4`，见 [汇总](../../../results/baseline/20260904-225132-990/summary.json)。9/9 检查通过，覆盖主工作区 fmt/check/test/严格 Clippy/rustdoc、facade 默认特性检查及独立工程 fmt/test/严格 Clippy。

| 覆盖 | 结果 |
| --- | --- |
| 主工作区原有测试 | 14 通过 |
| 独立契约测试 | 220 通过；原有 198 保留，新增 22 个持久运行测试入口（含 1 个子进程工作入口） |
| 指南 doctest | 11 通过；新增 1 个持久运行教学片段 |
| 总计 | 245 通过，0 失败 |

此前两轮完整基线同样保留：results/baseline/20260904-224238-975 为 242 项，results/baseline/20260904-224931-343 为 244 项，均 9/9 检查通过。随后补强了准入前遗留占用的 detached shutdown 失败上报，以最终轮次为准。

最终文档调整后再次运行 doctest，11/11 通过；16 份相关文档的本地文件链接检查通过，源码/文档 Git 差异空白检查通过。jingwei-budget、jingwei-agent、jingwei-agent-runtime 的 cargo package --list 清单不含独立测试/指南/脚本；原有内嵌测试迁移和实际 .crate 解包消费仍待发布验收。没有新增跟踪 target 产物。

验证包括：并发 acquire 独立 ID 与唯一获胜、V1 升级/错误占用格式、缺失/旧状态、失败/取消不交付执行权、正常 AskUser 后续跑、模型用量和活动时间保留、错误日志/回执不执行或不清除占用、报告/settle/检查点失败证据、失败镜像精确重试、未完成 run 拒绝关闭、detached completion/shutdown。

真实子进程使用文件检查点存储和官方 Session JSONL：先 AskUser，再在新进程完成下一 Turn；之后在模型调用结果已确认、正文仍等待时终止该测试进程。另一个新进程拒绝恢复，检查点和日志字节保持不变，无模型/工具重放。另一对真实子进程验证同一 ready revision 只有一个占用获胜。

故障注入在 BudgetCheckpointStore / Session 契约层，不是物理 write/sync_all 故障或掉电试验。没有在本轮运行 Linux、真实模型、未确认工具副作用故障矩阵；Session 多 Task 跨进程写者仍需协调设计。首轮定向测试中修正了测试助手的 Debug 断言和 Session 文件名定位，未以删除覆盖来绕过失败。

锁文件：主工作区未变（SHA-256 `98E25DEC5A6B84FF014AEA2E935855D9586FC85C782730A2BDFED20748B0B3BB`）；独立工程只加入已有 jingwei-journal-jsonl 的路径依赖，没有第三方版本变化（SHA-256 `7E7E8DDD488CE77A5A957F23CDACBA3621C04E8FC0ACE092FE0B8DC3C6666B08`）。

## 接续与提交边界

本批 d2-b 在本地尚未提交/推送。远端已确认的是 d2-a `96f83021ad66466609322a9d223f4c95f525b204`。action-step.md 的非本轮修改持续保留，没有混入前一提交或本批实现；target 不跟踪。

下一步 d2-c：设计审计核验/增额状态转移与幂等身份，明确日志证据、原占用、操作者来源、理由及旧/新上限；补实际文件失败和未确认工具等崩溃窗口。既有占用没有 TTL，不开放简单清除接口。新鲜 Session 历史和单一写者是当前持久模式的部署前提，后续需明确跨进程方案。完整任务 payload、待答状态和工具幂等性仍归 JW-07，完整计划见 [JW-04-plan](JW-04-plan.md)。
