# JW-04-b：有界模型调度与准入截止时间

日期：2026-09-04。状态：本批完成；基于 `dev` / `f3fad08`，开始前已快进检查远端，无新增上游提交。关联 [ADR-0005](../adr/0005-model-scheduling.md) 与 [JW-04 计划](JW-04-plan.md)。本批交付 F4 的模型调度部分，不代表 Task 累计预算、F4 或 F6 整体验收通过。

## 本批实现

- `jingwei-llm` 增加 `ModelSchedulerConfig` 和调度观察词汇。canonical 插件通过 `with_scheduler_config` 配置；标准组合沿用 `StandardCoreBundle::with_llm_runtime`，不新增 crate 或全局 provider。
- 完整生成、流式生成及同一 runtime 的所有 Turn 共享同一调度状态。同步准入原子检查容量并登记 FIFO 顺序，不依赖后台任务恰好先被轮询。
- 默认执行槽 4、等待容量 32、未结算总容量 64；等待容量允许为零。已分配但仍在记录/预检的 Preparing 与实际 Executing 共用执行槽，Queued 占等待容量，四个阶段均占总在途容量。
- provider 结束后释放执行槽，ModelResult 写入和其他清理仍由 JobGuard 持有总容量。取消、超时或流消费者丢弃期间，已接受的请求记录保持同一个 future 的所有权；不会通过丢弃、重试记录来伪装清理完成。
- 超时在同步 admission 固定为绝对 deadline，覆盖排队、请求记录、预检和执行。canonical 默认从无期限变为 600 秒；调用仍按 `min(call, runtime)` 收紧。零期限不启动 provider，宿主超出时钟范围的配置在构造时拒绝。
- 规范 V1 ModelRequest 的有效 timeout 与 raw adapter 实参保持一致，排队后不改写为剩余 timeout；外层绝对 deadline 保证剩余执行时间不会重置。事件格式不变，历史日志不改写。
- 增加 QueueFull/InflightFull 类型化过载、RequestTooLarge、QueueTimeout 和 InvalidTimeout。排队超时规范结果保留 Timeout 分类及 `model_queue_timeout` code；AgentRuntime 同步映射新错误，记录失败仍保留原有 closure failure 证据。
- 默认请求上限 8 MiB，统计输入与有效记录选项的 JSON 字节数之和，包括关联上下文。使用有界 writer，避免为体积检查分配完整 JSON 缓冲；不承诺限制调用者原始分配、schema CPU 或任意阻塞插件。
- `LlmRuntime::scheduler_snapshot()` 返回配置、是否接受新工作、阶段计数和有界在途列表。作业报告不含 prompt，包含 call_id、原始/剩余期限、排队/准备/执行/清理时长、记录阶段和确认标记。自定义 runtime 默认 None，已完成作业不在这里积累历史。
- 保留 complete 等待者丢弃后继续执行的所有权语义；stream 消费者丢弃则取消驱动。Graceful finish 排空已接受队列，Cancel/shutdown 取消并等待规范收尾，调用方丢弃清理等待者不会释放尚未结算容量。

## 验证与审查

最终运行 `bash scripts/check-baseline.sh --jobs 4`，日志见 [最终基线](../../../results/baseline/20260904-171349-543484-x_yekwhy/summary.json)。补齐最后两处边界断言前的完整基线也保留在 `results/baseline/20260904-171043-417940-r8k9et_3`。

- 9/9 检查全部通过：主 workspace fmt/check、facade no-default-features check、test、严格 clippy、rustdoc，以及独立工程 fmt/test/严格 clippy。
- 原有 14 项内嵌测试和 102 项独立测试全部保留；新增 19 项调度测试，独立测试共 121 项。指南 doctest 从 6 项增至 7 项，总计 **142 项全部通过**。
- 调度测试使用暂停的 Tokio 单调时钟、显式时间推进和 provider/recorder 门闩，覆盖跨 Turn、两种生成模式、实际执行峰值、FIFO、队列满、结果写入阻塞下的总容量、排队剩余期限和请求记录等待。
- 验证排队取消时请求记录仍被拥有且可复用等待容量、流消费者丢弃、慢消费者背压、complete 等待者丢弃、finish/shutdown 等待记录，以及失败证据保留。
- 验证零 timeout 已接受调用记录 Timeout、provider 零调用、容量可复用；原额度 5 秒的请求排队 3 秒后，raw adapter 与规范记录仍为 5 秒，但执行仅剩 2 秒。
- 验证请求体积覆盖消息和关联字段，无效容量/宿主期限在构造时拒绝，超大的调用期限被宿主有限值收紧。未进行真实模型推理、设备性能评测或其他平台的新构建。
- 独立代码审查核对了锁顺序、FIFO 通知、幂等容量释放及记录屏障；修正最终 deadline 检查与锁竞争之间的窗口，并把流式输入复制移至最后的 provider 入口检查之前，防止准备工作延后实际调用。
- 两份锁文件仅增加 runtime 对已有 serde 的直接依赖边；第三方版本不变。独立测试启用 Tokio test-util。源码、脚本、锁文件和文档通过 `git diff --cached --check`；原始 baseline 日志排除在空白格式检查之外，保留 Cargo 输出。

[模型调度教程](../../guide/src/model-scheduling.md)、既有模型/预算指南、README 和开发环境说明已同步。开发提交使用 `dev` 和 `wonderful-0803 <57706373+wonderful-0803@users.noreply.github.com>`；推送前通过 GitHub API 核对登录账户。

## 下一批 JW-04-c

把共享 Task 预算作用域贯通宿主、AgentRuntime、ModelTurnBinding/ToolTurnBinding 和两类 gateway。必须覆盖自定义 Agent、直接绑定 runtime、ActionStep 及框架内部模型工作，关联字段不能选择或提高额度。

接着定义无 Task 的默认有限作用域、单调用限制、预算触发的取消/排空和规范报告；即使 Agent 捕获预算错误并返回 Completed，也不能隐藏真实停止原因。预算结算和所需报告必须先于唯一 Done/Error 与 Session settle。

当前调度器尚未自动消费 `TaskBudget`，快照也不是规范持久报告或恢复凭证。预算跨进程恢复、版本化快照及审计增额仍归 JW-04-d，完整 Task 恢复仍归 JW-07。
