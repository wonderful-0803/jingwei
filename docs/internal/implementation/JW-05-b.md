# JW-05-b：确定性对话投影

日期：2026-09-14。开始基线 dev / 02cc358，提交账户 wonderful-0803。接续 [JW-05-a](JW-05-a.md)，范围 F2.1/F2.2；契约见 [ADR-0013](../adr/0013-conversation-projection.md)。

## 已交付

- 独立可选 jingwei-context 与 facade context feature，默认关闭；公开 ConversationProjector、CanonicalConversationProjector、配置、类型化错误和来源报告。
- 完整物理日志验证；用户、完整回复与工具交换确定性分组，delta 省略。模型候选不视为实际执行，工具结果逆序完成仍按调用顺序输出，合成调用 ID 避免跨 Turn 冲突。
- 失败/取消保留已确认工具结果及失败信封，排除完整回复并报告 Turn 状态。未配对、跨 Turn、重复、终止后事件等拒绝；旧日志默认拒绝缺失正文，显式 Omit 保留缺失标记。
- 来源事件、尾部、算法版本和实际配置可审计；输入事件数、序列化字节、输出消息数有界。无 IO、无日志改写、无新第三方依赖；不提升工具内容身份。

## 验证

[最终 Linux GNU 基线](../../../results/baseline/20260914-115720-834369-iy7am_bx/summary.json)：**9/9 通过，313 项 Rust 测试**（14 root、284 private、15 guide doctest），Rust 1.96.0，锁定离线依赖。

新增 10 项投影契约测试，覆盖相同输入重复投影、序列化往返、来源与输入不变、空回复、等待输入、并行逆序、ID 重用、未执行提议、失败/取消、旧日志、未确认工作、物理/语义损坏以及资源边界。既有 Agent 完整消息与新进程 JSONL 测试增加真实历史投影断言。指南片段的首次编译发现 SessionId 导入路径错误，修正为公开 id 模块后全部通过。

默认 facade 依赖树不包含 context。两个锁文件只新增本地 jingwei-context 包及可选依赖引用，没有新增第三方包。

VitePress 根路径与 /jingwei/ 两次构建、Markdown 测试和产物检查全部通过：每份 17 页、653 个链接、3 个搜索查询，公开源码文件数 0。指南见[确定性对话投影](../../guide/src/conversation-projection.md)。

## 接续

下一批 [JW-05-c](JW-05-plan.md)：可替换计数器、有界上下文选择、完整调用组保护和构建报告。当前消息/字节上限不是 token 预算；尚未完成上下文压缩、授权工具视图、ActionStep 自动接入或 F2 整体验收。
