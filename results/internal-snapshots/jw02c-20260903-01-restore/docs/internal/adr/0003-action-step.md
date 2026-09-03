# ADR-0003：可选单步动作组件

2026-09-03。关联 F1.3/F1.4/F1.5/F1.8；本批不实现 F3 的完整循环与 F6 恢复。

- 新增 jingwei-action，可由 facade 的 actions feature 选择；无新全局 runtime/provider，不强制自定义 Agent 引入 schema 引擎。
- ActionProtocol 只负责构造约束、解析和反馈格式，不获得原始 Tool、授权器或事件写入能力。ActionStep 只接受受控 ModelGateway / ToolGateway。
- 内置 NativeToolProtocol 与 JsonActionProtocol，统一 CallTool / Final / AskUser。默认单动作；原生多调用明确拒绝且不执行任何一个，不隐式择一。
- Native 的 jingwei_ask_user 是内部控制动作候选，不能与真实工具同名；不是执行工具。Final 使用普通完整文本。JSON 模式为 tagged object，候选工具与参数 schema 同时进入 oneOf。
- 每次 run 新建 StepId，并由宿主提供 TaskId。DecisionContext 写入规范 ModelRequest.options，ToolCall.action 写入同一 task/step、动作序号和可选 provider ID；ModelResult/ToolResult 用原运行时 ID 配对。禁止把 provider ID 用作 runtime 调用 ID 或跨恢复幂等键。
- 用 ToolGateway.schemas 的已授予集合构造候选，宿主只可进一步缩小。调用 ToolGateway 前再次校验完整响应、动作形状、可见性和参数；实际 ToolRuntime 继续独立校验/guard/审批。
- JSON 反馈是明确标记的非可信工具结果数据，传输为 user-role 数据消息，不是新的用户指令。规范来源在 ToolExecution 中保留；不声称角色标签提供 prompt injection 隔离。原生路径仍严格关联 assistant/tool 调用组。
- 工具失败返回 Halted，不自动重试、改工具或继续推理；记录失败/取消为带步骤关联的错误，宿主仍需关闭原 turn scopes。Finished/Final 不是业务条件已验证。
- AskUser 返回问题，不持有等待中的 Future；自定义 Agent 可映射 WaitingForInput。持久化待答 Task 状态仍由后续参考 Agent/恢复里程碑实现。
- schema 校验不访问网络/文件；生成 schema 为嵌入的工具参数分配独立资源 ID，以保持局部 $ref 语义。输入来自受信任宿主，非进程沙箱或完整 CPU 预算。
- 本批新增代码/指南与私有测试同步；不下载模型、不发布、不提交 Git。
