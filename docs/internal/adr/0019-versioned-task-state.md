# ADR-0019：版本化任务快照与既有预算边界

日期：2026-09-14。状态：接受。范围 JW-07-a / F6.1、F6.2 接口及 F6.3/F6.5 的读取侧基础。

新增独立可选 jingwei-task，facade 以 task-state feature / task 模块导出，默认关闭。复用 BudgetIdentity 作为 TaskIdentity，复用现有事件、StepId 和 BudgetEventCursor，不创建第二套执行账本。TaskSnapshot V1 私有字段、显式 capture/validate/verify_evidence，Serde 只搬运数据；安全使用必须独立核对身份、预期状态版本、当前兼容性、预算和完整确认日志。

快照包含自身递增版本、事件尾游标、预算检查点版本及累计 charged、按首次出现顺序记录的已结算决策 ID、回合结束相位及应用 payload。Completed 只是规范回合已完成，不证明业务条件成立；已结算决策包括模型拒绝结果，不表示工具成功或可重放。Created、WaitingForInput、Stopped 不自动获得执行权。

TaskCompatibility 显式固定 Agent/策略修订、工具契约修订和 payload 命名空间/schema 版本。框架校验声明一致性和数据边界，业务数据语义由应用验证。状态存储不允许隐式改变这些声明；迁移需后续显式协议。参考 Agent 尚未接入持久 TaskState，后续只能管理自身命名空间。

BudgetCheckpoint 增加只读 requires_recovery 和 verify_history，复用已有边界验证实现。TaskSnapshot 首批只接受空日志或完整关闭回合的精确日志尾，且拒绝冻结/活动执行镜像。日志评估验证地址、请求/结果配对、回合顺序和报告计数；缺失结果返回带原始事件地址的 unresolved 数据，不自动执行或补造结果。

TaskStateStore 定义 load/compare_exchange，0 表示不存在，写入在同一临界区核对版本、身份、兼容性及游标/预算/步骤单调性。MemoryTaskStateStore 提供有界进程内实现；完全相同的候选重试返回 AlreadyPresent，不增加版本。CAS 成功只证明状态存储更新，不证明预算/日志已落盘，也不授予恢复所有权。

每个状态最大 512 KiB、4096 决策、128 payload 命名空间、1024 工具声明；payload 深度/节点数、全日志事件数和内存任务槽数均有限。自定义 payload schema 的业务校验及持久存储故障错误契约随后扩展。

一致性顺序为确认日志 → 独立预算检查点提交 → 引用相同边界的任务快照。没有跨三者的原子事务假设。任务快照滞后、版本不兼容或有未结算工作时停止，并保留读取证据。首批不支持跨进程恢复、步骤中间安全检查点或工具幂等重试，AT-F6 全量验收留至后续批次。
