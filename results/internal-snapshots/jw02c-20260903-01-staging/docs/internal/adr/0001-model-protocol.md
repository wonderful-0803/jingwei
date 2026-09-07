# ADR-0001：分阶段引入结构化模型契约

状态：2026-09-03 实施第一批；关联 JW-01、JW-02、F1.1/F1.2/F1.5/F1.6。

兼容/双轨和协议模块归属决策已被 ADR-0002 替代。以下保留为历史决策记录，不是当前 API 指南。

## 决策

1. Rust 开发版本固定 1.96.0，支持下限声明为已验证的 1.96，不宣称支持未经验证的更旧版本。包版本暂不变，也不发布。
2. 不把旧 `ChatMessage { role, content }` 和已有规范 ModelRequest/ModelResult 原地改造成新协议。新增 jingwei-llm::protocol，保留旧文本入口与序列化，转换发生信息损失时返回错误。
3. ModelMessage 使用角色对应的枚举变体；assistant 工具调用与 tool 结果显式关联。ProviderToolCallId 只关联模型上下文中的一次调用组，不能转换成受信任 runtime 身份或恢复幂等键。不同已闭合调用组允许复用 provider ID。
4. 生成约束区分文本、原生工具和 JSON Schema。能力按生成路径、完整/流式分别声明 Supported/Unsupported/Unknown；未声明能力的现有第三方 provider 默认 Unknown。未知不是支持，不静默降级。
5. 协议预检只校验结构、消息关联、候选工具和能力声明。JSON Schema 根节点形状检查不等于 schema 编译或参数校验；授权、参数 schema 验证、预算和执行仍由受控 runtime 完成。
6. 结果显式区分 Stop、ToolCalls、Length、ContentFiltered、Unknown、其他结束原因。只有明确完整且结构一致的结果能转换成上下文消息；usage 缺失保留 None，不推断为零。错误/取消仍走现有错误通道。
7. 能力查询为无 IO 的元数据方法，canonical gateway 转发所选 provider 的声明。第一批不修改推理请求体，不宣称现有适配器已经实现原生工具或 JSON Schema。

## 依赖与后续边界

- core：身份、规范事件。新增 TaskId、StepId 词汇；不加入业务状态机或 IO。
- llm → core/session：模型协议、结构预检和模型 seam；不引入工具执行器或 schema 引擎依赖。
- adapter / canonical model runtime → llm：随后接入结构化调用与事件，不绕过现有 admission、取消、记录与 drain。
- 参考 Agent / ContextBuilder：后续通过构造参数注入策略，不迫使每个策略成为 provider。只能使用受控 gateway。
- Session 是事件归属，Turn 是一次运行和终态边界，Task 可跨 Turn；Step 关联一次决策，重试不得自动复用外部副作用的幂等身份。
- 恢复以新 Turn 开始；预算跨恢复是否继承由受信任宿主显式选择。不恢复任意 Future，不自动重试不确定的非幂等工具。
- 新结构化规范事件必须采用明确的版本化负载；读取旧事件保留旧语义，未知版本拒绝，不用新类型反序列化旧 payload 来补造缺失 usage/终止证据。本批未增加或修改持久事件格式。版本化事件、状态和预算细节仍需后续 ADR，JW-01 不因此全部完成。

## 私有验证与实施顺序

新增契约测试放入根 tests/model-protocol 独立 Cargo workspace，依赖公共 crate 路径，不加入主 workspace，也不在公共源码中 include 私有文件。既有 18 项内嵌测试先保留；后续通过私有验证副本注入原单元测试的方案评估迁移，不扩大公共 API。迁移前必须固化可恢复快照。

本地测试源、锁文件和脚本通过内部快照保留，未配置远端备份；远端/异机恢复仍为发布前阻断项，不声称 Git 忽略提供备份。用户要求尽快开发，因此已通过的构建基线允许 JW-01/JW-02 开始；模型选型、完整指南站、跨平台/发布包验收按原门槛继续，不能把这次协议测试当作 F1 全部完成。
