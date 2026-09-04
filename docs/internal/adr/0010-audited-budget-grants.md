# ADR-0010：安全边界上的可审计预算增额

日期：2026-09-04。状态：采用；JW-04-d2-c1。前置：[ADR-0009](0009-durable-budget-execution.md)。

## 框架边界

在 jingwei-budget 提供 `grant_budget`、BudgetGrantRequest/Record/Outcome/Error，使用已有 BudgetCheckpointStore，不新增具体 IO、业务审批流程、模型依赖或默认存储。BudgetOperationId 是宿主稳定操作标识，不是执行权、TaskId 或工具幂等键。宿主负责认证、授权、Session 单写者所有权，以及从可信来源独立确定身份、revision、末尾游标和规范历史；actor/source 字符串本身不是认证。

本批只对无运行占用、无冻结、无 run/pending 的检查点开放增额。确认关闭的预算耗尽可继续；未完成运行不是预算耗尽，不能借增额清除。存储缺失、错误、旧版本期望或更新的日志都不能初始化/重建任务。

## 原子审计与版本

新导出 BudgetCheckpoint 使用数字版本 3。V1 普通镜像与 V2 占用镜像可读；旧版本不能携带 grants。TaskRunReport 与文件外层 JSONL 版本仍为 1。旧客户端应拒绝 V3，不做降级解读。审计与新限制在同一快照、同一 revision CAS 中提交，既不重写旧 Session 事件，也不要求 Session/预算两个存储原子双写。

每条记录包含稳定 operation_id、actor、source、reason、原 revision、确认日志游标、旧/新绝对限制和原停止原因。成功增额后 stop 为 None；只有 None、被实际增大的 ResourceLimit 和 ActiveTime 可以走这条转移。Hard/Soft、绑定、ID 水位、累计扣费、实际/估计/未知用量、活动和清理时长全部不变；不得减少任何限制或突破宿主新上限。所有已消费资源必须被覆盖，活动时间须有正余量。

审计表跨封存、恢复、占用和后续 Turn 原样保留。目前最多 64 条；每个操作 ID/actor/source/reason 最多 1024 UTF-8 字节。达到上限返回 AuditFull，不自动裁剪旧 ID、复位任务或归档。可配置长期归档协议留待后续，不把无界审计塞进端侧运行时。传输/反序列化前的整体字节上限仍由 provider 负责。

存储在真实 CAS 临界区和文件链读取时调用 `validate_transition(previous)`：保留既有审计前缀，不允许无审计增额；新增记录只允许来自确切 quiescent 前态，且镜像只能改变版本、revision、限制、预算停止和追加的记录。该校验是结构约束，不能替代认证或真实 Session 证据。其他自定义 BudgetCheckpointStore 也必须更新，不得只校验 revision。

## 规范日志核验

增额前复用持久运行的物理日志/TaskRunReport/唯一终态核验。历史必须恰好截止检查点游标，不能跳过后续请求、不确认结果或另一 Task 的尾部。无游标只允许空日志和零消耗初始边界。

增额后再次运行时，旧 TaskRunReport 仍写着旧限制/停止原因。框架仅逆向应用“同一末尾游标”的审计记录，还原核验用的旧限制/停止；消耗、token 可信度、身份、模式和时间仍须与原报告一致。新 Turn 关闭后以新游标和真实报告为准，旧审计继续留存但不再改写该新边界。冻结快照可以保留审计与新的未决证据，不能因此变成可执行状态。

## 幂等、并发与不确定性

请求采用绝对新限制，不能在重试时重新计算增量。宿主应在第一次调用前可靠保留 operation_id、完整请求和原期望上下文。重试发现同 ID、同请求、同原 revision/游标时返回 AlreadyApplied，只是历史审计回执；即使后续有新运行或最新状态已冻结，也不增加额度、不写日志、不授予租约。相同 ID 不同意图返回 OperationConflict。新的操作不能靠更换 ID 绕过未核实的旧结果。

候选提交失败保留 expected_revision、确切 checkpoint 和存储确定性。存储对同一确切镜像的精确重试继续支持 ReplayedExact。并发不同增额或增额/运行占用在同一 Task CAS 上竞争，只允许一个转移；失败者核验最新状态，不能改 revision 自动覆盖。取消等待不能推断没写入，已接受 IO 仍按 provider 的所有权/排空协议处理。

增额函数不会返回 TaskBudget 或 BudgetExecutionLease。真正继续执行仍须重新 acquire，再由 canonical runtime 在 Session admission 内核验新鲜历史。跨进程 Session 单写者保护尚未提供，必须使用宿主已有的受控写者边界。

## 本批未覆盖

遗留 claim 的审计解除、活进程隔离/fencing、实际文件 write/sync 故障注入、更多 runtime 中断窗口仍归 d2-c 后续；本批不宣称 AT-F4-04/F4/F6 整体完成。完整任务 payload、AskUser 等待状态与工具幂等性仍归 JW-07。
