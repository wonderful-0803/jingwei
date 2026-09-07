# JW-02-b：唯一模型生成接口

日期：2026-09-03。状态：本批替换与本机验证完成；不代表 F1 或 v0.1 全部交付。

## 决策与实现

依据用户“尚未上线、无需兼容”要求和 ADR-0002，删除旧 complete/complete_stream、ChatMessage、Role、LlmCompletion、字符串 delta 流及转换层，不添加弃用别名。

- core::model 定义后端无关消息、约束、能力、响应、片段、版本与有界收集器；通过 jingwei::llm 再导出。
- Llm 和 ModelGateway 统一使用 generate/generate_stream。Text 与 NativeTools/JsonSchema 共用协议。
- Canonical runtime 在推理前检查能力并编译 schema，推理后验证输出/参数。外部 schema 引用禁止网络和文件获取。
- 规范模型请求/结果要求 version: 1。记录有效选项、完整响应或失败片段；旧无版本负载/未知版本不读取，不自动迁移或删除开发日志。
- 流式收集检查最终响应与已发出的片段一致；受控 Finished 在结果记录确认之后交付。取消、超时、消费者丢弃、背压与记录失败保留闭合语义。
- 兼容 HTTP 适配器覆盖文本、完整原生工具调用、JSON Schema 映射、工具历史关联、typed SSE、usage 与 finish_reason。默认只启用文本，结构化/stream usage 由宿主显式配置，无探测、无降级。
- 限制默认输出 8 MiB、32 调用；规范响应大小通过有界计数 writer 检查，不为计数再分配完整副本。schema 编译/验证仍是同步操作，非整个进程内存或 CPU 预算。
- 模型事件增大触发严格 Clippy 的内部 Command 大枚举检查，Session coordinator 的 Append 负载改为内部 Box，不变更公开 Session 接口。
- 指南说明了受控/原始边界、配置方式、破坏性替换与已知限制。适配器字段映射已通过 OpenAI Docs 对照官方 Chat Completions、function calling、structured outputs 文档核验，来源链接保留在指南。

## 验证证据

最终完整记录：results/baseline/20260903-222711-594。

- 8/8 检查通过：公共 fmt/check/test/clippy/doc，私有 fmt/test/clippy；check/test/clippy/doc 使用 locked/offline。
- 14 项保留的原有内嵌测试 + 52 项私有契约/回环集成测试 + 4 项指南 doctest，共 70 项通过。
- 原有 18 项内嵌测试中的两个旧模型 wire 用例和两个旧 SSE 用例已由私有套件中的版本化精确序列化、文本 delta/DONE、中文跨字节解析等覆盖替换；不再保留过期格式断言。其余 14 项测试仍在公共源码，不能宣称既有测试全部完成私有迁移。
- 私有覆盖包括 Text、并行工具参数分片、schema 与外部引用、无元数据、未知/截断结束、缺失/伪造终态、取消与超时、慢消费者、记录失败，以及 HTTP 适配器通过规范 gateway 的工具流完整链路。
- 所有 HTTP 测试只使用随机端口的 127.0.0.1；未连接真实推理服务，未下载模型、读取云 API key 或发起收费推理。
- 15/15 Cargo 包清单检查通过，无独立指南、测试、示例或内部脚本目录。结果见 results/package-lists/jw02b-20260903.json。实际 .crate 解包构建尚未验收。
- Git diff --check 通过；代码中无旧模型接口引用，其他运行时的内部 JobGuard::complete 是收尾方法，不是旧模型 API。

锁文件 SHA-256：

- workspace：7007FC6A67E163C9EBECD545D402509149EFE41A916DA26A3699F8CAF7B49052
- 私有验证：EE2218AE1B635F64DBEDB3724DCA1D804474F14CC684F25A67CB677BD981FEFE

## 隔离与恢复

新增测试保持在独立、publish=false 的 tests/model-protocol；指南在 docs/guide，与 crate 构建隔离。仅更新工作区，未 commit、push 或 publish。

旧实现可从原有 jw02a-20260903-01.zip 恢复；本批选定源码/内部资产将保存到 results/internal-snapshots/jw02b-20260903-01.zip，并通过解包后哈希比对检查。恢复清单另见同目录的 jw02b-20260903-01-verification.json。这些是同盘副本，不是异机备份或完整 Git 历史镜像。

## 后续边界

下一步继续 F1 的通用动作解析/校验与受控工具执行闭环。真实本地模型双模式、上下文预算、模型 admission 队列、任务预算、参考 Agent、恢复、完整指南站和发包验收仍按 PRD 分步实施；不把本批 70 个离线/回环用例当作真实模型质量与端侧性能证据。
