# JW-04-c：跨运行时预算执行与规范报告

日期：2026-09-04。状态：已实施并验证。开发基线：`dev` / `fdb2c3e`。关联 [ADR-0006](../adr/0006-runtime-budget.md)、F4.1/F4.2/F4.4–F4.9；预算持久恢复仍归 JW-04-d。

## 交付内容

- 宿主通过 `AgentTurnRequest::with_budget` 和 `Harness::start_turn_request` 传递同一 TaskBudget；canonical Agent 向模型和工具注入共享作用域。默认 Agent 与直接绑定的模型/工具入口均使用有限临时 Task，显式 Task 可跨 Turn 累计。
- `begin_admission` 在 Session 排队前开始计时，收到有效租约后 `bind_turn` 绑定真实 TurnId。错误 Session 租约不得收到用户消息或运行 Agent；已取得的租约仍通过 Error/settle 收尾。
- 模型每次接受计一步和一次模型请求，工具计一次调用；无模型步骤和修正通过窄 AgentBudget 接口计量。可信模型估算器来自宿主配置，Hard 模式要求两维 verified 上界；默认 Soft 模式明确使用估计。
- 模型/工具保存实际与未知消耗；执行前取消仅返还变量预留。排队、执行、消费者丢弃、停止和记录失败均保留作业所有权，不能因等待者离开或 Agent 捕获错误而恢复额度或伪装完成。
- 新增独立数字版本 1 的 TaskRunReport 事件，包含预算、停止原因、等待/执行耗时、确认与未确认事件计数、未决项和能力排空状态。Agent 无权提交该专用事件。
- `prepare_report` 关闭消费但保留 Task 租约；报告、唯一 Done/Error、Session settle 返回后再 `finish`。报告的失败、拒绝与异常回执分别保留 Failed/Rejected/Invalid 类型化证据，不能作为确认成功返回。

## 审查修复

交叉审查补齐了三组边界，并加入回归：

1. 模型实际用量超过预留时，预算失败仍保留已观察到的流式 partial 或完成内容，不能丢失已经输出的前缀。
2. 尚未 poll 的已接受工具任务被 executor 丢弃时，由 driver/JobGuard 共享幂等结算所有权；先释放未执行变量预留，再移除在途作业，保留尝试次数及闭包失败证据。已开始的未知用量保持保守计费。
3. 显式未绑定 Turn 的工具作用域拒绝后记录停止；Agent 验证 Session admission 身份及报告回执的 Session、Turn、事件种类与负载，异常证据保留在失败对象中。

## 验证记录

最终日志：[20260904-175424-454547-iy1wip3l](../../../results/baseline/20260904-175424-454547-iy1wip3l/summary.json)。`bash scripts/check-baseline.sh --jobs 4` 的 9 项检查全部通过：主工作区 fmt/check、默认 facade check、test、Clippy（拒绝 warnings）、rustdoc，以及独立工程 fmt/test/Clippy。使用锁定离线依赖；回环 HTTP 测试在允许本机端口监听的环境执行。

| 覆盖 | 结果 |
| --- | --- |
| 主工作区测试 | 14 通过 |
| 独立契约测试 | 160 通过；比 JW-04-b 新增 39 项 |
| 其中 Agent 预算集成 | 13 通过，含直接 ModelGateway 吞错、跨 Turn、Session 等待与报告故障 |
| 其中模型预算 | 9 通过，覆盖完整/流式、真实/未知用量、超额、身份、截止与记录屏障 |
| 其中工具预算 | 15 通过，覆盖权限、输出计费、超时/取消、记录故障与未启动任务丢弃 |
| 其中新增账本生命周期 | 2 通过，覆盖准入/报告租约与多观察者停止唤醒 |
| 指南 doctest | 8 通过 |
| 总计 | **182 通过，0 失败** |

既有 ActionStep 集成测试增加共享模型/工具/步骤计数、实际工具字节及确认事件计数断言。修改的指南/ADR/实施记录本地链接检查通过。第三方依赖版本未改变，锁文件仅更新内部 crate 的直接依赖关系。

首次完整记录 [20260904-175317-091018-oedn7rod](../../../results/baseline/20260904-175317-091018-oedn7rod/summary.json) 的全部测试已通过，独立工程 Clippy 指出新增故障测试中两处 MutexGuard 词法作用域问题；收紧测试锁作用域后保存上述最终 9/9 基线，保留原日志供核对。

## 未完成边界

预算仍为内存共享状态；同进程跨 Turn 不等于跨进程恢复。默认 token 策略是 Soft 估计，Hard 保证取决于宿主的可信上界证据。报告在自身 append、终态 append 和 Session settle 之前采样，不包含之后的持久化延迟；关闭消费后的报告准备阶段保留 Task 租约。

TaskRunReport 不是可执行恢复快照或审计增额授权。下一批 JW-04-d 继续实现版本化预算快照/重建、未决状态保留、审计增额及真正新进程恢复验证；完整任务状态恢复和工具幂等性继续归 JW-07。
