# ADR 0021：关闭步骤回合后显式续跑

日期：2026-09-14。状态：接受，JW-07-c。前置：[ADR 0019](0019-versioned-task-state.md)、[ADR 0020](0020-file-task-state.md)。

## 决策

增加独立 jingwei-task-runtime，提供绑定单个 Task 的宿主 TaskCoordinator。调用者显式提供 AgentRuntime、同一排他 SessionPersistence、预算与状态存储、时钟、当前恢复策略和应用 payload reducer。默认 SDK 不安装此组件。PluginRegistry 向可信宿主公开已选择的 SessionPersistence 引用，避免协调器使用另一个竞争写者。

运行前核对 TaskSnapshot 的预期版本、身份、当前兼容性、完整日志和预算边界，要求 TaskResumePolicy 重新检查实际 Agent/策略/工具/schema 与当前操作者权限。然后通过既有 BudgetExecutionLease::acquire 获取唯一持久占用，复查状态和日志，再将 lease 整体移交 canonical runtime。没有从持久预算降级为普通 TaskBudget 的路径。网关的当前授权及审批继续生效。

有限参考 Agent 增加默认关闭的 checkpoint_each_step。成功工具步骤通过 Checkpointed 显式结束回合；运行时按既有顺序排空能力、写完整消息与预算报告、终态、settle、保存最终预算；协调器从完整确认历史和对应预算派生并保存 Task 状态。借助完整关闭的回合边界支持步骤检查点，不放宽原预算对活动执行、缺失结果或不完整终态的拒绝规则。

Checkpointed 同步加入 TurnOutcome、TurnDisposition、DoneStatus、TaskRunStop、ProjectedTurnState 和 TaskPhase。它不同于 Completed 和 WaitingForInput；自定义 Agent 也可以显式返回这一通用边界。现有终态含义不变，旧程序遇到新枚举值应拒绝解码，不能降级解释；当前开发版本直接扩展枚举，不额外改写现有记录外层版本。

TaskIntent 仅允许 Created→Start、Checkpointed→Continue、WaitingForInput→匹配提问回合的 Reply；Completed/Stopped 不自动继续。继续始终创建新 Turn，保留原 Task 累计预算，从规范历史投影获取先前工具结果；框架不重派发旧调用。新模型仍可提出新的相似动作，其副作用契约及幂等策略留在 JW-07-d。

## 所有权与失败

start 同步准入，Tokio 后台任务拥有完整校验、执行与状态发布；控制器或等待者 drop 不撤销工作。实例及 clone 单操作准入，close 停止新操作并排空；跨实例由预算 CAS 抑制重复执行。宿主先 close，再关闭并释放 runtime、registry、provider 和存储；新进程使用新所有者。

TaskRunResult 分别保留 canonical turn 结果和 state 发布结果，不能用后者覆盖前者。失败状态提交保留确切候选和底层确定性，允许精确重试但不重跑工具；预算已经前进而状态滞后时，下一次准入拒绝，宿主显式核对业务状态再修复。预算收尾失败保留原 runtime 错误及候选，任务状态不宣称可恢复，既有预算审计流程继续适用。

取消在 claim 确认后但执行准入前可能留下冻结占用；框架不清空 claim 或退款。运行中的取消转发原 runtime canceller，收尾后可保存 Stopped 边界；完成回合再取消等待不回滚已确认事实。宿主回调是可信代码，其永久阻塞不能由框架强制安全终止。

## 验证范围

覆盖步骤暂停/投影/预算/状态一致性，三个独立进程的工具步骤→提问→回复交接且工具正文只执行一次，错误答复回合、当前策略和工具授权拒绝、兼容性变化、并发竞争、累计预算收紧、状态和预算收尾失败、日志边界变化、取消与分离等待。仍不等同于任意崩溃恢复、掉电保证或完整 AT-F6；外部副作用幂等与全故障矩阵继续分批实施。
