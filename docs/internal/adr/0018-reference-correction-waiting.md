# ADR-0018：有限纠错与显式待答续跑

日期：2026-09-14。状态：接受。范围 JW-06-c / F3.4–F3.5，接续 ADR-0016、0017。

参考 Agent 默认每 Turn 最多修正 2 次，允许 0–1024。仅对未执行工具、可明确分类的格式/参数错误纠错；下一步存在时先 consume_correction，再写 correction_v1，随后重新推理。共享 Task 累计预算与 max_steps 同时约束；报告失败、取消不退款，失败动作不伪装为成功步骤。固定诊断作为受保护 system 内容参与上下文预算，成功后移除，不回显原始输出或伪造工具结果。

模型 runtime 已在参数错误到达 ActionStep 前执行 Schema 校验。为避免将所有协议失败都当作可修正，增加 ModelGatewayError::SchemaRejected，仅用于 Complete 的已通过大小/形状检查但响应 Schema 失败的结果。保留有界、只读被拒响应用于精确分类，canonical ModelResult 仍为协议失败；Debug 不输出内容，结果持久化或预算结算失败仍优先返回对应错误。请求预检、stream 和其他模型失败不改变。官方协议解析和可见工具集共同决定是否修正；不可见工具直接停止。新增错误枚举分支是需要调用方更新穷尽匹配的 API 变化。

工具执行失败、权限/用户拒绝、已确认执行后的反馈错误、副作用不确定性、完成检查拒绝及取消不走普通修正。任何修正都不能增加授权或预算。

AskUser 保存 PendingQuestion 版本、Session/Task/Turn/Agent 与问题。ReferenceWaiting 从成功且排空的待答报告和原 TaskBudget 绑定进程内交接，检查身份、累计消耗、空闲状态，并拒绝 durable checkpoint 报告。resume 消耗非 Clone 句柄，匹配提问回合，将结构化答复连同原账本交给现有 AgentTurnRequest；不重新创建同名预算。

宿主负责答复身份、拒绝语义与单一待答所有权。该辅助类型不建立全局去重锁，不能替代持久化执行 lease；序列化的 PendingQuestion 不授予续跑权。跨进程任务恢复留至 JW-07，AT-F3 全部路径验收留至 JW-06-d。
