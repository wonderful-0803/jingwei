# ADR-0002：直接替换模型接口

2026-09-03，用户明确：尚未上线，不要求兼容旧文本接口。替代 ADR-0001 中的兼容/双轨决策。

- 删除 ChatMessage、Role、LlmCompletion、字符串 delta 流和旧 complete/complete_stream 签名，不增加兼容别名。
- 单一 GenerationRequest/GenerationResponse 协议，Text 是普通模式，NativeTools/JsonSchema 是显式生成约束；原始 Llm 与受控 ModelGateway 统一 generate/generate_stream。
- 共享词汇移入 core::model 供 Session 模型事件引用；llm 直接再导出。请求与结果模型记录要求 version=1，旧/未知模型记录拒绝读取，不改写或删除现有日志。
- 流只发布 typed delta 与一个 Finished；受控 gateway 的 Finished 必须在规范结果提交后才交付宿主。缺失结束事件、结束值与 delta 不一致均失败。取消、超时、背压、丢弃消费者和记录失败继续走原有 drain 路径。
- 普通文本也携带结束原因和可选 usage。截断/未知原因允许作为已结束推理报告返回，但不能转换成可执行完整消息；不完整工具参数保留为独立 fragment。
- 兼容 HTTP 适配器只默认启用文本，宿主显式声明服务端的 NativeTools/JsonSchema/流式 usage 支持。schema 传输不等于全关键字强制约束；运行时独立验证参数与结构化结果，工具执行仍独立授权。
- 输出收集限制为受信任调用选项，默认响应正文上限 8 MiB、32 个工具调用；不是输入 token 计数、任务级预算或模型性能承诺。
- 删除的旧 wire 测试用新协议记录与生命周期测试替代；不删覆盖来让改动通过。开发指南与私有测试一并迁移。
