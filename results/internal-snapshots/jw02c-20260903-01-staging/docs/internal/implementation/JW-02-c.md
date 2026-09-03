# JW-02-c：可选单步动作与受控执行

日期：2026-09-03。状态：本批完成；关联 ADR-0003 与 F1.3/F1.4/F1.5/F1.8，不代表 F1 或完整 v0.1 验收结束。

## 本批实现

- 新增独立 jingwei-action（workspace 第 16 个 crate）。facade 用可选 actions feature 再导出；默认依赖图无 jingwei-action。没有新增全局 provider 或 runtime。
- ActionProtocol 是只接收数据/模型可见 schema 的策略接口，可替换请求约束、动作解析和反馈投影；不能取得原始 Tool、guard、审批器或事件写入器。
- NativeToolProtocol / JsonActionProtocol 统一 CallTool、Final、AskUser。一次只接受一个动作；多原生调用整步拒绝，不择一、不隐式排序执行。
- ActionStep 从受控 ToolGateway.schemas 获取已授权目录，可进一步缩小；先验证完整模型结果、候选和参数，再调用既有 ToolGateway。ToolRuntime 的 schema/grant/guard/审批/取消/超时/结果记录保持独立。
- 原生 AskUser 是保留的虚拟控制动作，并以控制结果关闭模型消息组，不伪造 ToolExecution。JSON 模式输出 oneOf 约束，嵌入工具参数并保留局部 $ref 语义。
- DecisionContext（TaskId/StepId）进入规范 ModelRequest.options.context；ToolCall.action 记录同一步和动作序号。运行时 ModelCallId/ToolCall ID 继续独立产生，provider ID 仅作来源关联。
- 工具成功得到 ToolCompleted，所有受控语义失败得到 Halted；不会因 retryable 自动重试或换工具。记录失败/取消用 ActionStepError 传播；反馈投影失败等执行后错误保留已确认 ToolExecution。
- next_messages 保存闭合的历史，不累积协议加入的 system 提示。JSON-only 反馈带明确数据标签，传输为 user role；不把提示词约定当作安全隔离。
- 公开指南新增单步章节，PRD/入口与环境说明同步。没有新增公开测试、示例工程或 private include。

## 验证

最终日志：results/baseline/20260903-230901-998。

- 9/9 工程检查通过：fmt、workspace check、facade no-default-features check、test、严格 clippy、doc，以及私有 fmt/test/clippy。
- 保留 14 项原有内嵌测试；私有测试从 52 项增至 72 项（新增 20 项动作测试），指南 doctest 从 4 项增至 5 项。总计 91 项全部通过。
- 两种动作模式的“工具→结果→下一次模型决策→最终回答”通过同一套 canonical model/tool runtime 验证；自定义 Agent 另通过 canonical AgentRuntime/SessionRuntime 组合验证 Final 和 WaitingForInput。
- 无有效权限/可见性、参数缺失或类型不符、额外动作字段、多动作、缺终态或截断、guard 拒绝及用户拒绝均不产生工具正文执行。
- 覆盖四个记录屏障、调用前/执行中取消、执行超时、错误反馈投影，以及已提交动作证据不丢失、不重放。
- 本批未删除任何既有测试。首次私有编译发现测试模块位置在 inner doc attribute 之前，修正后完整回归通过。
- 本机假模型、内存记录/存储和既有回环 HTTP 测试，无真实模型下载、远程推理或付费调用；没有将这些测试称为模型质量或端侧性能评测。
- results/package-lists/jw02c-20260903.json：16/16 包清单不含独立 tests/examples/fixtures/docs/scripts。实际 .crate 解包消费、跨平台与既有 14 个内嵌测试的私有迁移仍未完成。
- 独立 cargo tree -p jingwei --no-default-features 验证默认 facade 无 jingwei-action 依赖；git diff --check 通过。

锁文件 SHA-256：

- workspace：6FBA72ECB118A110BCC953F24D1E344A397F8DD418D5C2476684B4A68799E71F
- 私有工程：3040CA82FCF9BA15FDBA262D8F3E48243BD088A56D123A4DA32B040FA99263C8

## 隔离与恢复

测试仍放在 tests/model-protocol 独立 publish=false workspace。指南位于 docs/guide，不是 Cargo 构建输入。仅更新本地工作区，未 commit、push 或 publish。

本批恢复包目标为 results/internal-snapshots/jw02c-20260903-01.zip；解包逐文件 SHA-256 验证记录放在同目录 jw02c-20260903-01-verification.json。它包含选定源码、指南、内部测试/脚本/锁文件和本批最终日志，是同盘副本，不是异机备份或完整 Git 历史镜像；旧阶段快照保留。

## 未完成与下一批

- 本批 ActionStep 使用完整 generate；动作流式展示、修正和完整 Agent loop 未实现。
- AskUser 返回语义问题。示例测试的自定义 Agent 可将问题放入 Done.artifact 并结束 Turn，但官方待答 Task 存储与恢复仍属 F3/F6。
- 全局 CPU/内存上限、上下文 token 预算、累计任务预算和模型 admission 队列尚未交付；schema 编译/投影来自受信任宿主策略，不承诺插件沙箱。
- JSON-only 的观察消息不是新的用户意图；来源以规范事件为准。后续 ContextBuilder 将进一步提供工具结果投影与预算治理。
- 下一批按 JW-04 做任务预算和模型 admission，先建立有限循环所需的资源控制；真实模型配置仍需按既定下载授权边界准备，之后补 JW-03 真实验收。
