# ADR-0014：有界上下文选择与计数契约

状态：已采纳，2026-09-14。接续 ADR-0013，范围 JW-05-c / F2.4、F2.5、F2.8。

## 决策

在既有可选 jingwei-context 中增加同步 ContextBuilder / ContextTokenCounter，默认实现不依赖 IO 或 runtime。输入为已终止历史投影、单独的当前对话、宿主 system、输出约束、固定回合、状态版本和目标模型/模板身份。输出完整 GenerationRequest 及可序列化报告，宿主负责保存和实际推理。

默认按最旧的未固定历史 Turn 整体删除。保护全部 system、当前对话、constraint 和 pinned_turns，不更改消息内容/角色或工具集合。每组及完整输入在裁剪前校验，未完成工具调用明确失败，不能通过删除它来伪装可继续推理。当前对话允许已确认工具往返；历史 Open 回合拒绝，避免与 current 混淆。最低必要上下文超限返回 DoesNotFit，不放宽约束。

复用 core 的 TokenBudgetMode 与 TokenBoundEvidence。计数器对完整候选提供 system、messages、constraint、template 的互不重复成本，覆盖实际 provider 渲染；每次删除后重新计数，不假设计数可加减或单调。Hard 要求匹配目标身份的可信上界以及宿主可执行的输出预留上界；精确计数可用 VerifiedUpperBound 表达。标记本身不验证 tokenizer 或 provider 行为。

内置 ByteHeuristicCounter 为序列化 UTF-8 字节数除以 4 向上取整，外加模板估计，始终返回 Estimate。只允许软预算，软模式也不返回已知估算超限的成功请求。算术溢出、空身份/方法、零输入计数、计数错误或证据不满足均拒绝。

默认输入限制 16 MiB、4096 个组/回合、8192 条消息与 128 次完整计数。CountLimit 与 DoesNotFit 保留最后已计数候选报告；资源约束不提供自定义宿主计数器的时间/内存隔离。窗口及输出预留必须为正，安全余量允许零。

## 报告与边界

报告保存选择算法/投影版本、来源 Session/事件尾部/数量、宿主状态版本、每个历史回合来源事件及保留/固定标记、预算/限制和逐次计数。原始投影保留旧日志缺失、失败回合和省略原因。报告可序列化，但不隐式写 Session，不具有恢复、权限或账本授权效力。

上下文窗口与 Task 累计预算职责不同；不替换 ModelBudgetEstimator。实际调用必须使用计数时的目标、模板、消息、约束和可执行输出上限，改变输入需重新构建。工具授权视图、结果精简和 ActionStep 自动接入留到 JW-05-d。

## 验证

12 项契约测试覆盖真实序列化的私有字节模型精确计数、schema 增长、长历史及工具结果、完整组/必要项保护、非单调模板成本、硬软模式、错误、资源上限和确定性。该私有模型不代表任何真实 provider 的 tokenizer 已验收。真实 Agent Session 历史也进入构建器验证，详见 [JW-05-c](../implementation/JW-05-c.md)。
