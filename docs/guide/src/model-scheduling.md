# 模型调度与超时

canonical LlmRuntime 对完整生成和流式生成使用同一个有限调度器。它限制执行槽、等待作业、包含清理工作的总在途数及请求大小，并从准入时开始计算单次截止时间。同一 runtime 下的不同 Agent 和 Turn 共用这些限制。

调度器限制同时占用的容量，[任务预算](task-budget.md)限制累计用量。canonical ModelGateway 同时遵守两者，与 Agent/Tool 共用显式传入的 TaskBudget；无显式绑定时也有有限临时作用域。跨 Turn 累计要求宿主复用同一 Task。

## 配置调度器

`ModelSchedulerConfig` 从 `jingwei::llm` 导出，插件来自 `jingwei-llm-runtime`。下例只构造配置，不启动 runtime、不访问网络，应用需依赖这两个 crate。

```rust
use std::time::Duration;

use jingwei::llm::ModelSchedulerConfig;
use jingwei_llm_runtime::CanonicalLlmRuntimePlugin;

let config = ModelSchedulerConfig {
    max_concurrency: 2,
    max_queued: 8,
    max_inflight: 16,
    max_request_bytes: 1024 * 1024,
};
let _plugin = CanonicalLlmRuntimePlugin::new()
    .with_scheduler_config(config)
    .with_default_timeout(Duration::from_secs(120));
```

应用使用 `StandardCoreBundle` 时，将该插件传给 `with_llm_runtime(plugin)`，再通过 `install_into(builder)` 提交标准候选集合；仍需由宿主装配模型 provider 等依赖。也可沿普通 Harness 插件路径安装。

| 配置 | 默认值 | 含义 |
| --- | --- | --- |
| `max_concurrency` | 4 | 同时分配的执行槽，包括请求记录/预检阶段与 provider 执行阶段 |
| `max_queued` | 32 | 等待执行槽的作业数，允许为 0 |
| `max_inflight` | 64 | 所有已接受、尚未完成记录和收尾的作业数 |
| `max_request_bytes` | 8 MiB | 模型请求与有效记录选项的 JSON 字节数之和 |
| 插件默认 timeout | 600 秒 | 未指定调用 timeout 时采用的有限上限；显式调用只能收紧 |

并发数、总在途数和字节上限必须大于零，总在途数不得小于并发数。无效配置在服务构造时失败。等待容量为零时，没有可立即分配的槽就拒绝等待；不会无限创建后台等待任务。

例如，模型已经返回但 ModelResult 写入仍阻塞时，该作业会释放执行槽并继续占用总在途容量。新调用可能因总容量已满被拒绝，即使还有执行槽；这样清理积压也保持有限。

## 从准入起计算截止时间

`GenerationOptions.timeout = None` 表示继承 runtime 的有限默认；显式值取 `min(调用 timeout, runtime 默认 timeout)`。绝对 deadline 在 admission 固定，排队、请求记录等待、预检和执行消耗同一额度，出队不会重新取得完整 timeout。

零 timeout 会立即超时且不启动 provider。超出时钟范围的 runtime 默认值导致构造失败；不能表示为有效 deadline 的调用返回 `InvalidTimeout`。

规范 `ModelRequest.options.timeout` 与实际传给 raw adapter 的有效 Duration 保持一致。runtime 不在排队后把实参偷偷改成另一个剩余值；它用独立的外层绝对 deadline 约束执行。例如，调用有效额度 10 秒、排队用了 7 秒，执行最多继续到原先的 10 秒期限，不能因为 adapter 收到 timeout=10 秒而再运行 10 秒。

请求记录本身如果超过期限，runtime 保留已接受的记录工作并等待确认；不会随后启动 provider。结果记录和清理也可能超过执行期限。因此 timeout 表示停止新的执行和受控推理的期限，不保证调用等待者、Turn finish 或 shutdown 在同一时刻返回，更不保证强制终止任意阻塞插件。

## 观察排队、执行和清理

宿主可通过所选 `LlmRuntime::scheduler_snapshot()` 获取当前快照。自定义 runtime 默认返回 None，表示没有提供调度观察；不能把它当作所有计数为零。

| 阶段 | 如何理解 |
| --- | --- |
| `Queued` | 已接受，正在等待执行槽 |
| `Preparing` | 已分配槽，等待规范请求记录或进行预检 |
| `Executing` | 驱动 provider，流式背压也可能使作业停留于此 |
| `Cleaning` | 已退出等待/执行槽，但还有记录或收尾工作未完成 |

`ModelSchedulerSnapshot` 提供 `accepting`、`inflight`、`queued/preparing/executing/cleaning` 和有限的 `jobs` 列表。每个 `ModelJobReport` 含 call_id、phase、原始有效 timeout、remaining_time、各阶段时长、recording、request_recorded/result_recorded 及 stop_reason，不含 prompt。所有 accepted 作业都会立即启动 ModelRequest append，因此 Queued 也可能正在记录请求；阶段不等同于记录是否持久，需结合确认标记判断。

FIFO 表示同步准入时的执行槽分配顺序，不承诺不同 Tokio 线程或不同请求记录速度下，provider 正文恰好按同一顺序开始。

快照不保留无界已完成历史，也不会后台持久化调度报告；作业完全收尾后会离开在途列表。需要排查记录失败时，应保留调用错误中的规范尝试证据，不能依赖稍后快照重建整个任务。

## 处理拒绝和超时

受控入口通过 `ModelGatewayError` 返回错误；下列调度错误位于其中的 Runtime 分支。

| 错误 | 处理含义 |
| --- | --- |
| `Overloaded { capacity: QueueFull }` | 等待容量已满，本次没有接受工作 |
| `Overloaded { capacity: InflightFull }` | 总在途容量已满，可能包含记录阻塞或清理积压 |
| `RequestTooLarge { limit_bytes }` | 请求序列化规模超过配置，没有接受工作 |
| `QueueTimeout` | 已接受但在排队阶段耗尽期限；规范失败保留 Timeout 分类及 `model_queue_timeout` code |
| `InvalidTimeout` | 有效 timeout 无法构成可用截止时间 |
| `Budget(error)` | Task/run 预算停止、身份或计量错误；保留类型化原因 |

QueueFull 和 InflightFull 是 `ModelOverloadKind` 的变体。非排队阶段的受控超时继续返回 `LlmError::Timeout`。宿主取消、runtime 停止、Turn 已关闭和 Recording 失败仍分别报告，不应统一解释成可自动重试。

已接受的排队作业超时、被取消或丢弃流消费者后会撤队，清理继续拥有同一个已接受的 request append future。请求确认后才记录失败结果；请求记录失败则返回带证据的 Recording 错误。清理完成前仍占总在途容量。不要因一次 timeout 或等待者离开，就把已有请求记录当作不存在并自动重放。

provider 返回后，ModelResult 确认前不会交付成功的完整响应或 Finished。记录失败保留 `ModelClosureFailure`，不被普通 timeout 掩盖。宿主仍须显式关闭 Turn 并调用 shutdown，排空已经接受的工作。

## 体积限制和当前边界

`max_request_bytes` 统计 `GenerationRequest` 和有效 `ModelRequestOptions` 分别序列化为 JSON 的字节数之和，包含消息、工具/schema、关联和调用选项，不包含固定事件信封及运行时生成 ID 的开销。实现通过限量 writer 计数，不为检查额外分配完整 JSON 缓冲。它与 `GenerationLimits` 的响应收集上限是两种不同限制。

调用者传入的对象已经存在于内存；请求序列化、schema 处理和任意插件代码仍可能消耗 CPU 或阻塞。请求大小和作业数量有界，不等于整个进程的内存/CPU 有硬上限。

本轮默认行为较早期源码发生变化：canonical runtime 从无默认期限变为默认 600 秒，并开始把排队和请求记录等待纳入同一截止时间。规范 ModelRequest/ModelResult 仍为 version 1，timeout 字段保留 adapter 实参语义，历史日志不会改写。

Task 活动时长和模型单次期限同时约束工作，预算停止后仍须完成已接受的记录和结算。AgentRuntime 在能力排空后记录独立的 TaskRunReport；这与仅观察在途作业的 scheduler_snapshot 用途不同。预算持久化恢复仍待实现，当前调度与内存累计不代表 F4 已整体验收，也不提供完整 Agent 循环或任务恢复。
