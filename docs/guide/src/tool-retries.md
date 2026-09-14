# 工具副作用与显式重试

JW-07-d 在现有 jingwei-tool / canonical ToolRuntime 中增加副作用契约、逻辑动作键和宿主重试计划。有限参考 Agent 不会自动重试失败工具；本章 API 供持有受控 ToolGateway 的可信宿主或自定义 Agent 显式使用。

## 声明工具的副作用

ToolMetadata::with_effect 接受 ToolEffect 和契约修订号。默认 Unknown；ReadOnly 表示不产生写副作用；Idempotent 表示工具承诺按同一个逻辑动作键去重；NonIdempotent 表示存在不可直接重试的副作用。修订号由宿主维护，输入或副作用契约变化时必须更新。

例如：`metadata.with_effect(ToolEffect::Idempotent, "orders-v1")`。声明不替代实现：具体工具必须持久保存去重状态、拒绝同键不同参数，并把外部写操作与去重记录协调好。仅在内存中缓存 call_id，不能兑现跨重启幂等承诺。

runtime 为每个新逻辑动作生成独立 operation key，同时生成本次调用的 call_id，并在执行正文前写入 ToolCall.operation。ToolBodyRequest::operation 提供相同的 TaskId、key 和契约；工具应使用 TaskId + key 作为自己的去重范围。不要使用 provider 返回的 ID 或会随尝试改变的 call_id 作为持久幂等键。

## 如何判断是否能重试

宿主在隔离旧写者、排空旧工作后，加载独立确认的完整 Session 历史，再调用 inspect_tool_recovery。该函数检查物理顺序、调用/结果配对、操作链、参数与契约连续性，且拒绝针对已有后续尝试的旧调用制定计划。

| 已确认事实 | 处理 |
| --- | --- |
| 成功结果，或宿主已核验完成 | 拒绝重试，不合成新的工具结果 |
| 权限、参数、guard 或审批阶段拒绝 | 工具正文未开始，可显式准备新的尝试；仍须重新通过当前检查 |
| 缺失结果、正文失败、超时、取消、预算中止或输出超限 | 外部结果可能不确定；不根据 retryable 标志推断未执行 |
| 结果不确定，契约是 ReadOnly / Idempotent | 允许宿主显式准备一次尝试，沿用逻辑动作键 |
| 结果不确定，契约是 Unknown / NonIdempotent | 返回 NeedsReview；只有可信宿主确认未执行后才能准备重试 |

ToolRetryReview 包含操作者、外部证据引用和核验结论 NotExecuted / Completed / Uncertain。它应来自宿主核验或可信业务流程，不能直接由模型输出填充。确认完成只阻止重试，不伪造规范日志或外部返回值；不确定结论不解除未知/非幂等工具的待核验状态。

## 一次显式重试

```rust,no_run
use jingwei::event::SessionEvent;
use jingwei::tool::{
    inspect_tool_recovery, prepare_tool_retry, ToolCallOptions,
    ToolExecution, ToolGateway, ToolRetryReview,
};

// confirmed_history 来自独占所有权下的最新确认日志；当前执行与预算
// 必须先走正常恢复/准入流程。此函数不获取或绕过执行 lease。
async fn retry_once(
    gateway: &dyn ToolGateway,
    confirmed_history: &[SessionEvent],
    latest_call_id: &str,
    host_review: Option<ToolRetryReview>,
) -> Result<ToolExecution, Box<dyn std::error::Error>> {
    let assessment = inspect_tool_recovery(confirmed_history, latest_call_id)?;
    let plan = prepare_tool_retry(confirmed_history, latest_call_id, host_review)?;
    Ok(gateway.call_with_options(
        &assessment.call.name,
        assessment.call.arguments,
        ToolCallOptions { retry: Some(plan), ..Default::default() },
    ).await?)
}
```

ToolRetryPlan 不可序列化；克隆共享一次性使用限制。runtime 在准入前核对会话、Task、工具、完全相同参数和当前契约，沿用原 key，生成新的 call_id，并把原调用/事件地址和宿主核验证据写入新的 ToolCall.operation.retry。计划一旦被消费，即使后续准入失败也不能再次使用；宿主须检查最新证据后显式重新准备。

计划不是权限，也不是跨进程锁。runtime 仍执行当前工具授权、参数校验、guard、审批、预算扣费、取消与收尾；每次尝试正常累计 tool_calls，不退款或清零之前已执行的消耗。不同进程仍需遵守 Session 所有权和预算 lease。自定义 ToolRuntime 也必须使用 take_operation 校验并消费计划，不能只复制其 key。

## 与任务恢复的关系

[受控任务续跑](task-recovery.md)只接受安全关闭的步骤或待答边界。工具重试计划不会清理活动 claim、补造缺失结果、打开 Stopped 任务或恢复预算；未完成的执行仍需先走既有核验流程。prepare_tool_retry 的成功只说明这些工具证据满足重试契约，不等同于系统现在可以执行。

旧 ToolCall 没有 operation 字段时仍可解码，但不能自动迁移出可信稳定键，重试规划返回 LegacyCall。历史上限为 100,000 个事件且序列化最多 16 MiB；操作标识、契约修订及核验文本最多 1024 字节。超限或损坏明确失败。

本批测试覆盖不同副作用分类、核验与已完成阻止、输入绑定、单次使用和真实 canonical runtime 的重新装配。幂等测试工具在写入后返回失败，重试先被新审批拒绝，再获批执行；三个尝试累计计费，但持久业务记录只有一份。这验证测试工具兑现了幂等契约，不代表任意外部工具具备 exactly-once。完整跨进程故障矩阵继续在 JW-07-e 验收。
