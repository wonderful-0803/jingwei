# ADR-0009：持久预算运行占用与规范日志边界

日期：2026-09-04。状态：采用；JW-04-d2-b。前置：[ADR-0007](0007-budget-checkpoints.md)、[ADR-0008](0008-file-budget-checkpoints.md)。

后续扩展：d2-c1 的 [ADR-0010](0010-audited-budget-grants.md) 将新导出升级为 V3，补充安全边界上的审计增额和日志核验规则。下文保留 d2-b 当时的版本与未交付边界，不表示新增功能已完成冻结解除。

## 框架决策

在 jingwei-budget 中增加 BudgetExecutionLease：宿主显式选择 BudgetCheckpointStore，取得不可 Clone 的执行占用句柄，再通过 AgentTurnRequest.with_durable_budget 转交 canonical AgentRuntime。核心依然没有具体 IO/Tokio 依赖；异步协调只调用存储 trait，不在账本锁内等待 IO。内存预算入口仍然是内存入口，不默默变为持久模式。新能力不依赖 ActionStep，也不引入业务 Task payload。

持久占用不是超时后可抢占的 OS 锁，而是通过 CAS 写入的冻结检查点。每次 acquire 生成独立 BudgetExecutionId，避免两个进程从相同快照构造相同镜像，进而把 ReplayedExact 同时误认为各自获得执行权。成功返回的唯一句柄是本次内存执行权；存储里的占用镜像不是执行凭证。

## 状态转移与版本

| 阶段 | 持久状态 | 可执行性 |
| --- | --- | --- |
| 显式初始化 | ready revision 1 | 仅表示新预算已保存；运行还需 acquire |
| acquire CAS 确认 | claimed revision 2，唯一 execution_id，recovery_frozen=true | 只有此次返回的租约持有者可交给 runtime |
| 执行及收尾 | 仍为 claimed revision 2 | 其他进程不能恢复、接管或自动清除占用 |
| 报告、终态、settle 均确认后 | ready revision 3，记录终态地址与累计预算 | 下一 Turn 须重新取得 revision 4 的占用 |
| 任意未完成窗口 | claimed / 损坏日志 / 明确不确定结果 | 拒绝自动恢复，保留证据供审计 |

新导出的 BudgetCheckpoint 使用独立数字版本 2。V1 的普通快照仍可读取并在下一次转移升级；V1 不允许携带 execution_id。V2 占用要求非空 ID、冻结标志、没有 run/pending；它保存上个安全边界的账目，不冒充本轮实时用量。TaskRunReport 事件与文件 JSONL 外层仍是版本 1，各自独立。旧实现会拒绝未知快照版本，不会把新占用误读为可执行账本。

acquire 不自动创建缺失任务，不接收模型/工具输出的账本，不开放冻结状态恢复。宿主期望身份、revision、游标和收紧后的额度必须匹配。存储 CAS 返回前不向调用方暴露租约；等待者离开或返回错误都不自动撤销持久占用。租约 Drop 会封存其内存账本的所有 clone，但不会取消外部 IO，也不会清除存储占用。

## Runtime 写入顺序

1. 宿主 acquire 确认占用后，转交 with_durable_budget。Runtime 同步准入、绑定预算并开始累计 Session 等待时间。占用 IO 属于执行前准备，不计入已有 active_time；适配器仍须有界且由宿主排空。
2. Session admission 提供历史后，在任何新 UserMessage/模型/工具请求之前核验物理地址连续、ID 唯一、Session 一致；检查点必须恰好位于历史末尾。无游标只允许空历史和零用量初始账本。
3. 续跑边界必须是同一 Turn 的 TaskRunReport + Done/Error。核对绑定、用量、限额、活动时间、排空标志及报告/终态语义；未确认能力事件或未配对的请求/结果计数也拒绝作为 ready 边界。额外历史或不符证据直接拒绝，不追加新 envelope；已取得的 Session lease 仍尝试 settle，结果保留在 RecoveryBoundary 错误中。
4. Agent/Model/Tool 沿用同一预算作用域。排空后依次确认 TaskRunReport、唯一终态和 Session settle；核对报告、终态回执和结算切片是同一组原始事件。
5. run.finish 后封存所有内存句柄，确认没有未完 run/pending；再用该占用的 revision 执行最终 CAS。检查点 CAS 完成前，runtime job 保持在途，completion 与 shutdown 继续等待。

两个存储不是原子双写。新 ready 检查点只会在 Session 屏障确认后发布；在此前任一位置崩溃，占用镜像都阻止使用旧额度重新执行。报告已经写入而最终检查点未确认时，不能删除报告或宣称正文没有执行。

TurnFinally 仍观察 Session 的终态/settlement，不代表后续检查点已经确认。成功的 AgentTurnReport.budget_checkpoint 给出最终确认镜像；普通内存 Turn 返回 None。持久化错误使用 AgentRuntimeError::Durability，同时保留原始 canonical outcome；未送达调用者的持久错误进入有界 detached failure 集合，shutdown 报告失败。最终 CAS 失败还保留 expected_revision、确切候选镜像和存储错误，供宿主核验/精确重试，不从运行报告猜测新快照。

## 部署与恢复限制

- Session provider 必须提供真实持久化回执和新鲜的规范历史，并有独立的单一写者边界。预算占用仅互斥同一 Task，不能替代不同 Task 共用 Session 时的跨进程锁。
- 现有 JSONL Session adapter 没有跨进程协调；本批进程实验只让同一 Task 串行使用同一 Session。多宿主/多 Task 共用 Session 仍需补齐 Session 所有权方案，不能宣称已支持。
- 过期缓存或新增历史会使边界检查拒绝；本批不把其他 Task 的尾部自动跳过。恢复必须明确使用新鲜 Session 历史。
- 首次创建仍是受信任宿主流程：独立确认任务/Session 没有历史，准备文件并保存初始镜像。acquire 返回 Missing 不授予重建已有任务的权限。
- 拒绝的 runtime admission、取消的 acquire、进程退出都可能保留占用。没有自动解冻、TTL 接管、工具重放或故障后退款；这是保守可用性边界。
- 占用期间的精确用量只能来自运行时/规范事件，不能把占用镜像中的旧账目当成本轮零消耗。审计核验和增额仍由 d2-c 实现；完整业务状态、AskUser payload 和工具幂等性仍归 JW-07。

## 验证边界

独立测试覆盖竞争 acquire 的唯一 ID、故障/取消无租约交付、错误日志和回执、Session/检查点顺序、确切失败镜像、detached job/shutdown。真实子进程完成 AskUser→下一 Turn，再在模型调用结果已确认后终止执行进程；下次运行被占用拒绝，日志原字节不变。

存储失败注入发生在测试 BudgetCheckpointStore 契约层，不等于实际文件 write/sync 故障。Linux、磁盘写满/掉电、未确认工具副作用、审计增额与冻结后的处理仍需后续验证。AT-F4-04 和 F6 不能据此整体勾选。
