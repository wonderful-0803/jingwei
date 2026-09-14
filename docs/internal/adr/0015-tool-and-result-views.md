# ADR-0015：授权工具视图、结果引用与上下文动作接入

状态：已采纳，2026-09-14。范围 JW-05-d / F2.6、F2.7、F2.9；接续 ADR-0013/0014。

## 决策

工具选择与实际执行授权分开。ToolSelector 只能返回名称，select_tool_view 在 ToolGateway 授权快照内复核名称唯一性及成员关系，并从原集合提取未修改的 schema。GroupedToolSelector 提供确定性的显式名称/分组并集；输出按名称排序。默认限制授权数、可见数和输入字节，不允许自定义选择器提供新 schema 或扩大执行范围。

ToolResultPolicy 只决定原文前缀长度，不提供改写后的正文。受控函数从 ToolRecordedOutcome 提取正文，保留成功/失败状态及 category/code/retryable；按 UTF-8 边界生成预览，大结果保留完整来源引用，视图超限或存储失败明确返回错误。

提供显式 MemoryContentStore，宿主配置命名空间、条目数、总正文容量、单结果大小、页大小和最长 TTL。引用绑定来源事件、Session、Task、字节数和有效期，读时核验全部元数据及宿主作用域/时间；同一有效来源只有相同正文才可幂等。没有路径解析或自动注册读取工具。宿主若开放模型读取，须自行授予读取工具并从可信上下文注入 scope/time。内存引用不能跨进程恢复，不把引用当 bearer 授权。

ContextualActionStep 作为 jingwei-action 的 context 可选扩展，组合官方 Native/Json 协议。实际协议 System 和 schema 先生成，再由 CanonicalContextBuilder 计数/选择，随后通过 PreparedProtocol 发送完全相同的请求。内部 ActionStep 继续使用授权快照和参数校验，ToolRuntime 的 guard/approval 不受影响。输出 max_tokens 只能收紧，宿主仍负责提供可信 provider 上界。

工具执行后只修改模型继续对话的反馈，原始 ToolExecution/Session 日志不变。Native 保持 Tool 角色；Json 沿用受标记的 User 数据信封，额外标记 untrusted_tool_data，不能进入 system。next_current 仅包含当前回合和新增反馈，协议指令与历史不累积。已有历史投影不被此适配器改写。

## 失败与边界

视图/上下文构建失败发生在模型调用前。工具已执行而结果精简失败时，ContextActionError::ResultView 保留完整已执行报告，不能重放。底层 Action 错误仍保留不确定副作用证据与计数/工具报告；Task 完整恢复不在本批。

策略、计数器、存储与时钟是宿主扩展点，必须遵循其声明契约。选择器不能扩大名称集合，结果策略不能改写正文；但框架不为宿主任意代码提供执行隔离，也不证明模型永不受提示注入影响。执行权限由受控网关独立限制。

两个 Cargo 锁文件只增加 action 到既有 context 的可选关系，不增加第三方依赖。默认 facade、单独 actions 和单独 context 保持可用。验收见 [JW-05-d](../implementation/JW-05-d.md) 与 [F2 映射](../implementation/JW-05-acceptance.md)。
