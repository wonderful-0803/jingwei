# JW-04-a：共享任务预算账本与 Linux 基线

日期：2026-09-04。状态：本批完成；基于 `dev` / `70add1f`。关联 [ADR-0004](../adr/0004-task-budget.md) 与 [JW-04 计划](JW-04-plan.md)。本批提供 F4 的账本前置能力，不代表完整 F4 或 F6 验收通过。

## 本批实现

- 新增 `jingwei-budget`，workspace 从 16 个 crate 增至 17 个。纯数据放在 `jingwei-core::budget`，账本通过 `jingwei::budget` 访问；账本仅依赖 core 和 thiserror，不执行 IO，也不引入 Tokio 或新 provider。
- 宿主创建固定 Task/Session/Agent 身份的 `TaskBudget`，一次只能开启一个 `BudgetRun`。共享 `BudgetScope` 在同一把锁内检查并预留所有维度，旧 run 的 scope 不能消费后续 run 的额度。
- 宿主、Task、run 限制取严；Task 消耗跨 run 累计。步骤、模型请求、工具调用和修正尝试在预留成功时永久消费；token 与工具输出字节先预留再结算，预留拒绝不部分扣费。
- 硬 token 模式要求可信上界声明；软模式拒绝未知零估计。实际值、保守计费值、估计证据和未知次数分别保存。实际或估计超出预留时保留原值并停止准入；算术溢出保留未决项及原始 `failed_usage`。
- reservation 不可 Clone。仅执行前明确取消可以释放变量预留，次数不退；未结算句柄丢弃保留 pending 并冻结任务。停止、时钟异常或 run 丢弃后，已持有 reservation 的所有者仍能结算。
- 注入单调时钟，累计活动墙钟时间，排除 run 之间的闲置时间，区分截止之后的清理时间。run 丢弃后，有 pending 时继续计清理时间，最后一次结算后停止计时。
- `finish` 可在 pending 清理后重试，成功报告保留刚关闭 run 的证据；随后 Task 报告仅保留累计值。报告可序列化但不是持久快照，不能反序列化出执行权限。
- 新增 [预算教程](../../guide/src/task-budget.md)，纳入独立测试工程的指南 doctest；README、指南入口与导航同步。
- 新增 `scripts/check-baseline.sh`，与 PowerShell 入口保持相同的 9 项检查，支持显式 jobs、逐项 stdout/stderr 和 JSON 汇总。使用固定 Rust 1.96.0，不修改项目工具链声明。

## 验证与证据

功能修改前，安装 Rust 1.96.0/rustfmt/clippy 并下载两个 workspace 的锁定依赖。首次受限沙箱检查仅因禁止监听本机回环测试端口失败，记录保留在 `results/baseline/20260904-161616-047558-jl5zgp3_`；允许回环端口后，修改前 9 项检查全部通过，记录在 `results/baseline/20260904-161734-118898-mrpevu7b`。

最终验证：`bash scripts/check-baseline.sh --jobs 4`，日志在 [最终汇总](../../../results/baseline/20260904-162533-339031-hzg4nqbf/summary.json)。

- 9/9 检查通过：主 workspace fmt/check、facade no-default-features check、test、严格 clippy、rustdoc；独立工程 fmt/test/严格 clippy。
- 保留原有 14 项内嵌测试与 72 项独立测试；新增 30 项预算契约测试。独立工程共 102 项测试，指南 doctest 从 5 项增至 6 项；总计 122 项全部通过。
- 新测试覆盖 16 线程争抢最后额度、多维原子拒绝、取消次数不退、actual/estimated/unknown、超额和溢出证据、single active run、旧 scope、跨 run 剩余额度、假时钟回退与 deadline/cleanup。
- 独立代码审查发现 run 丢弃后未决清理时间漏记，已修复，并用 t=0 开始、t=1 丢弃、t=11 结算、t=21 观察的确定性测试验证：活动 1 秒、清理 10 秒，结算后不再增长。
- `cargo package --package jingwei-budget --list --allow-dirty --locked --offline` 核对新包清单：仅源文件、README 和 Cargo 生成的元数据/manifest/lock，无独立测试、内部文档或脚本。这不是实际打包安装或发布验收。
- 两份锁文件仅增加新本地 crate 与 facade 依赖边；第三方依赖版本保持原样。源码、脚本、锁文件和文档的 `git diff --cached --check` 通过；检查时排除 `results/baseline/**`，其中原始测试 stdout 保留 Cargo 输出的末尾空行，不改写日志来消除格式提示。

开发提交使用 `dev` 和 `wonderful-0803 <57706373+wonderful-0803@users.noreply.github.com>`。本机没有该账户的 HTTPS 推送凭据；本批不声称已同步远端。

## 下一批

JW-04-b 实现模型执行并发和有限等待队列，区分上游执行槽与已接受但尚未完成记录的总容量；排队、记录等待和执行共用准入时建立的绝对截止时间，并保持请求/结果提交屏障。

JW-04-c 才把同一预算作用域贯通 Agent/Model/Tool 的正式调用入口、默认有限作用域、单调用限制与停止报告；JW-04-d 才实现版本化持久快照、审计增额和真实跨进程恢复。当前账本不自动中止 future，现有 gateway 尚未自动消费它；没有把内存报告或 Arc 共享当作持久恢复证据。
