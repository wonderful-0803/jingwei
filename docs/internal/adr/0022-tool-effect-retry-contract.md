# ADR 0022：逻辑动作键与宿主显式重试

日期：2026-09-14。状态：接受，JW-07-d。范围为 PRD F6.6/F6.7 的工具契约，不自动恢复任务或外部副作用。

## 决策

ToolMetadata 增加 Unknown（默认）、ReadOnly、Idempotent、NonIdempotent 及宿主维护的契约修订。canonical ToolRuntime 为新动作生成逻辑 key，并在 ToolCall.operation 记录 TaskId、契约和重试审计，再传入 ToolBodyRequest。单次 call_id 仍每次新生成；provider 的 ID 不参与稳定键生成。具体工具负责兑现持久幂等性，框架不会把声明变成 exactly-once 保证。

新字段为可选装箱数据，保持旧日志可读，避免放大 ToolRecord 等值类型。没有操作身份的旧记录不能自动生成可信重试计划。工具调用 schema 不暴露宿主重试计划给模型；有限参考 Agent 不自动重试。

inspect_tool_recovery 要求独占所有权下的最新完整确认日志，验证物理地址、调用结果配对、操作链及输入/契约一致性。确认成功、已存在新尝试、损坏证据都不能按旧调用继续。正文失败、预算中止、取消、超时等不证明未执行；retryable 标记不授权重放。

prepare_tool_retry 仅为显式宿主请求生成一次性计划。只读/已声明幂等可对不确定结果准备尝试；未知/非幂等需可信 NotExecuted 核验，Completed 始终阻止重试。核验保存 actor/evidence/outcome，不伪造工具输出。参数和修订变化不能沿用键。

计划不可序列化，clone 共享单次消费标记；消费只发生在 runtime 核对 Session、Task、工具、参数与当前契约一致后。后续准入失败也不回收计划，需重新核验。新的调用保存前次 call_id、event_id 和核验证据，重新执行当前授权、参数/guard/审批与预算检查。该标记不是跨进程锁，也不授予执行权限。

## 验证与边界

定向验证副作用分类、失败标志、已完成阻止、旧记录/损坏/过期尝试、所有输入绑定及克隆消费。真实 canonical ToolRuntime 测试重新装配工具/runtime，工具第一次持久写入后报告失败，第二次被审批拒绝，第三次获批并通过原键去重；业务记录一份，预算累计三次尝试。

历史限制为 100,000 个事件和 16 MiB 序列化输入；操作/审计文本和契约修订限制为 1024 字节。宿主与自定义 runtime 必须保证可信确认日志、旧工作隔离、单一执行所有权及重试前证据新鲜度。计划不关闭活动预算 claim、补造缺失结果或打开 Stopped Task。跨进程完整故障矩阵与介质掉电验证不在本批承诺内，继续 JW-07-e。
