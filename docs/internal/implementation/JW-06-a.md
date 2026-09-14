# JW-06-a：有限参考 Agent 的基础驱动

日期：2026-09-14。开始基线 dev / e8e0d04，账户 wonderful-0803。批次范围见 [JW-06 计划](JW-06-plan.md)，决策见 [ADR-0016](../adr/0016-bounded-reference-agent.md)。

## 已交付

- 新增独立可选 jingwei-reference-agent，facade reference-agent 默认关闭；实现现有 Agent 接口并由宿主显式注册，复用既有投影、上下文与 ContextualActionStep。
- 每 Turn 默认 8 步，每步单动作；保护当前请求、system 和固定历史，工具成功继续，失败不重试。Task 共享账本由宿主持有，模型推理自动计步，驱动不重复扣费。
- Final 标明模型声明、business_verified=false；AskUser 正常关闭为 WaitingForInput，待答 artifact 可持久化，宿主携答复复用原 Task 继续。
- body 步骤/运行报告保存来源与停止原因，报告大小/写入失败停止，原始执行日志保留。取消/预算/内部错误可能不留下最终 body 报告，canonical TaskRunReport 与终态仍负责实际结算证据。
- 显式的内容 TTL/时钟与缺失预算/模型检查，不授予工具或自动安装读取工具。无工具网关可完成回答/提问。

## 验证

[最终 Linux GNU 基线](../../../results/baseline/20260914-143127-693712-ide04zs5/summary.json)：**9/9 通过，355 项 Rust 测试**（14 root、322 private、19 guide doctest），Rust 1.96.0，锁定离线依赖。

新增 13 项测试入口，通过真实 canonical Agent/Session/模型/工具 runtime 运行 Native 和 Json 路径：无工具回答、两次依赖工具调用、有限步数停止、非法动作及工具失败、AskUser 后同 Task 续跑、预算先于本地步数停止、取消排空、报告大小/写入拒绝、缺少共享预算、时钟回拨/上下文拒绝、无效配置，以及自定义 Agent 直接替换。验证模型计步无重复、报告先于完整回复、失败不伪造成功回复。

开发中修正了测试使用控制器取消入口的错误（应使用 canceller），并补齐工具测试插件的 TOOL_RUNTIME 能力声明，没有改变框架授权规则。首次完整基线的既有 exclusive_os_lock 测试在 drop 后再获取时报 Busy；并行子进程可延长继承文件描述的存活，测试改为显式 unlock 后关闭句柄，继续保留持锁 Busy/未写入/解锁后写入三项断言。针对性测试和完整基线随后通过；失败日志保留在 [首次基线](../../../results/baseline/20260914-142904-861793-6nh8qgo7/summary.json)。生产存储实现未修改。

默认 facade 依赖树不包含 reference-agent/action/context；两个锁文件仅新增本地包及其既有依赖，没有第三方包新增。

指南新增一个可编译注册示例，VitePress 根路径与 /jingwei/ 构建、Markdown 测试和产物检查通过：每份 20 页、838 个链接、3 个搜索查询、0 个公开源码文件。见[有限参考 Agent](../../guide/src/reference-agent.md)。

## 接续与限制

本批仅交付 F3 基础驱动，不声称 AT-F3 全部通过。下一批 JW-06-b 增加完成检查与无进展检测；有限纠错、更完整待答契约和停止路径验收随后推进。真实模型、跨平台、完整任务恢复和业务成功条件仍需各自验收。
