# JW-05-d：工具视图、结果引用与上下文动作接入

日期：2026-09-14。开始基线 dev / da3c1fa，账户 wonderful-0803。范围 F2.6/F2.7/F2.9，契约见 [ADR-0015](../adr/0015-tool-and-result-views.md)，F2 总体覆盖见[验收映射](JW-05-acceptance.md)。

## 已交付

- 确定性 GroupedToolSelector 和独立子集复核；自定义策略只返回已授予工具名称，schema 不变，数量/字节有界，空集合明确保留。
- ToolResultPolicy 的原文前缀选择与受控视图，保留成功/失败和错误事实，UTF-8 安全，精简正文保留完整引用。
- 有界 MemoryContentStore：不可变来源、重复写幂等、容量/单项/分页/TTL、Session/Task 作用域、元数据核验、过期拒绝与主动清理。无文件路径或自动工具权限，进程退出后内存引用不可用。
- 可选 ContextualActionStep：官方 Native/Json 协议完整指令与 schema 先计数，原样发送；继续受控执行后只转换反馈。报告保留计数、工具选择、结果视图和原始执行证据，next_current 避免历史/协议指令累积。视图失败不重放已执行工具。
- 不修改 AgentRuntime、SessionRuntime、provider 或原始日志，不增加第三方依赖。action 的 context feature 默认关闭，facade 同时启用 actions/context 时自动接入。

## 验证

[最终 Linux GNU 基线](../../../results/baseline/20260914-140246-187036-dcw2ft6o/summary.json)：**9/9 通过，341 项 Rust 测试**（14 root、309 private、18 guide doctest），Rust 1.96.0，两套 workspace 锁定离线依赖。

新增 6 项视图/引用契约和 7 项受控 Action 集成测试，覆盖排序/分组、恶意选择、空集合/容量、UTF-8 预览与分页、作用域/过期/篡改、幂等冲突、失败事实、Native/Json 两步推理、实际预算请求与日志保留、不可见/未授权工具拒绝、工具提示注入、已执行精简失败证据、推理前预算失败及 guard 拒绝保留。指南新增 2 个编译片段。

额外验证 facade 单独 actions 与单独 context 均编译成功，确认可选组合边界。两个锁文件只新增一个本地依赖关系。

VitePress 根路径与 /jingwei/ 构建、Markdown 测试和产物检查均通过：每份 19 页、774 个链接、3 个搜索查询、0 个公开源码文件。使用说明见[工具视图与动作接入](../../guide/src/tool-views.md)。

## 后续

JW-05 a–d 本地契约实现完成。下一阶段 JW-06 有限参考 Agent，先落地显式装配、有限步骤驱动与停止路径，再推进修正、AskUser/完成检查与无进展策略。生产计数器、完整恢复、跨平台和真实模型基线不以本批结果替代。
