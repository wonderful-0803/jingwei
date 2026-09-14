# JW-05-c：上下文预算与消息选择

日期：2026-09-14。开始基线 dev / cab12d7；提交账户 wonderful-0803。范围 F2.4/F2.5/F2.8，契约见 [ADR-0014](../adr/0014-context-budget.md)。

## 已交付

- 既有可选 context feature 增加 ContextBuilder、ContextTokenCounter、CanonicalContextBuilder 和 ByteHeuristicCounter，不新增 crate 或第三方依赖，两个 Cargo 锁文件不变。
- 完整请求计数覆盖 system、消息、原生工具/JSON schema、模板、输出预留与安全余量。硬模式要求目标绑定及可信输入/输出上界，启发式仅支持软模式；所有加法检查溢出。
- 从最旧未固定回合开始裁剪，保护当前全部消息、system、约束和固定历史；工具交换完整保留。每次重新计算完整候选，不依赖成本单调性。无效/未配对工作在裁剪前拒绝，最低必要上下文无法放入明确失败。
- 序列化构建报告包含来源/状态版本、取舍与固定项、预算/限制、逐次计数。DoesNotFit/CountLimit 携带最后候选报告；纯策略不改写原始历史或自动持久化，不扩大工具权限。

## 验证

[最终 Linux GNU 基线](../../../results/baseline/20260914-134722-873632-0lagw6ur/summary.json)：**9/9 通过，326 项 Rust 测试**（14 root、296 private、16 guide doctest），Rust 1.96.0，两套 workspace 锁定离线依赖。

新增 12 项契约测试：精确边界、system/schema/template 与预留、80 回合长历史及中文长工具结果、固定旧回合、最低必要内容超限、当前工具交换/未配对调用、软模式标签/硬模式拒绝、计数器目标不符/零值/溢出/错误、资源与尝试上限、无效投影/未知固定项、确定性和序列化往返、原生工具 schema 增长及 Named choice 保留、非单调模板成本重计数。精确测试使用私有字节模型，不声称真实 provider 已验证。

既有完整回复回归中新增真实 Session 投影到构建器的接入断言，涵盖最终正文、流式、空回复和等待输入。没有改动 AgentRuntime、SessionRuntime、provider 或 Task 预算执行代码。

指南教学片段编译通过；VitePress 根路径与 /jingwei/ 构建、Markdown 测试和产物校验全部通过，每份 18 页、711 个链接、3 个搜索查询、0 个公开源码文件。使用说明见[上下文预算](../../guide/src/context-budget.md)。

## 接续

下一批 JW-05-d：授权集合内的工具视图、受控结果精简/引用、身份保留、ActionStep 接入与 AT-F2 映射。本批没有提供真实模型 tokenizer、自动摘要、完整状态恢复或 F2 整体验收。报告由宿主保存，硬预算的实际模型/输出配置须由宿主核实。
