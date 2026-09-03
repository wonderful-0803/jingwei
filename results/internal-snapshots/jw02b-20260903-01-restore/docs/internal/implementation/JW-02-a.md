# JW-02-a：结构化模型契约首批实现

日期：2026-09-03。状态：本批完成；JW-01、JW-02、F1 整体仍未完成。

历史记录：旧接口兼容决策已被用户取消，后续以 ADR-0002 / JW-02-b 的唯一生成接口为准。

## 已实现

- Rust 1.96.0 固定开发版本、1.96 支持下限，15 个 crate 统一继承。
- 角色专属消息、完整工具提议、provider 调用组关联、无损旧文本转换。
- Text / NativeTools / JsonSchema 请求约束；按路径和完整/流式分别进行能力预检。
- 完成原因、可缺失 token 用量、完整消息投影、候选工具和选择规则的结构检查。
- 原始 provider 与受控 gateway 的无 IO 能力查询；已有兼容 HTTP 适配器仅声明文本支持。
- TaskId/StepId 身份词汇；不把 provider ID 当成 runtime 身份或幂等键。
- 公共指南 Markdown 与内部编译验证；独立私有测试 workspace，不给公共源码添加私有 include。

## 验证证据

最终日志：results/baseline/20260903-214732-347。fmt、workspace check/test、严格 Clippy、rustdoc、私有工程 fmt/test/严格 Clippy 全部通过。

18 项原有测试、25 项新契约测试、3 个指南 doctest，共 46 项执行通过。新测试涵盖消息关联/重复/缺失/乱序、provider ID 作用域、能力 Unknown/Unsupported 和路径分离、转换信息损失、截断/未知结束原因、usage 缺失、候选工具选择、旧 provider 源码兼容、gateway 能力转发和旧请求的规范记录/收尾。

15 个 Cargo 包清单未含独立私有/指南目录；不因此宣称包中不存在旧内嵌测试，未执行实际包发布或解包构建。

本地恢复快照及哈希详见 M0-BASELINE.md 第 9 节；未配置异机恢复，未推送任何资产。

## 本批没有完成的内容

- GenerationRequest 尚未接入 Llm/ModelGateway 的结构化推理方法；旧 complete/stream 仍是文本接口。
- 没有修改模型 HTTP 请求/响应格式，没有修复或宣称覆盖 SSE/UTF-8 流式边界。
- shape/preflight 不编译 JSON Schema、不验证参数 schema、不授权工具；不能绕过 ToolRuntime。
- 新规范事件的版本负载、预算、任务状态/检查点、Action 解码器及参考 Agent 尚待实现。
- 无真实模型评测、完整指南站、跨平台或发布完成声明。

对应验收仅覆盖 AT-F1-03 的声明预检、AT-F1-04 的终止元数据契约、AT-F1-05 的旧文本兼容与部分文档。AT-F1-01 双 provider 端到端和 AT-F1-02 schema/动作执行安全完整闭环仍未验收。

## 下一批

JW-02-b：版本化结构化请求/结果事件、受控模型入口和通用动作解析/校验，保留现有 admission、取消、超时、记录失败及 drain 语义；随后 JW-03 接入真实适配器和结构化流式。
