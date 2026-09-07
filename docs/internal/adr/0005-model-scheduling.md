# ADR-0005：有界模型调度与准入截止时间

日期：2026-09-04。状态：JW-04-b 的实施决策，实际验证结果另行记录。关联 F4.3/F4.6/F4.7，并为 JW-04-c 的共享预算接入提供边界。本批不表示 F4 整体验收完成。

## 配置与容量

- `jingwei::llm::ModelSchedulerConfig` 提供 `max_concurrency`、`max_queued`、`max_inflight`、`max_request_bytes`，默认分别为 4、32、64、8 MiB。
- `CanonicalLlmRuntimePlugin::with_scheduler_config` 接收配置；标准组合继续通过 `StandardCoreBundle::with_llm_runtime` 安装该插件，不新增全局 provider。
- 并发数和总在途数必须大于零，总在途数不得小于并发数，请求字节上限必须大于零；等待容量允许为零，表示没有可立即分配的槽时拒绝等待。无效配置在服务构造时失败。
- 同一 runtime 下的完整生成、流式生成和所有 model turns 共用调度器。准入在同一同步临界区内检查容量、接受作业并按 FIFO 分配槽；不承诺 Tokio 线程调度或 provider 正文实际开始的先后次序。
- `max_concurrency` 限制分配的执行槽，覆盖 Preparing 和 Executing；`max_queued` 限制 Queued；`max_inflight` 约束全部已接受且尚未完成收尾的作业，包括已撤队但仍等待规范记录的 Cleaning。不得仅释放执行槽就认定作业已完成。

## 请求体积边界

准入限制 `GenerationRequest` 和有效 `ModelRequestOptions` 各自序列化为 JSON 的字节数之和，不包含固定事件信封和运行时生成 ID 的开销。实现使用限量 writer 计数，不为了检查再生成完整大缓冲。

超限返回 `ModelRuntimeError::RequestTooLarge { limit_bytes }`，不接受工作、不调用 provider。该边界限制 runtime 接受的序列化请求规模，不限制调用者已经分配的输入、序列化 CPU、schema 编译或任意插件阻塞行为；不是整个进程的内存或 CPU 沙箱。

## 时间与 V1 规范记录

- 保留 `GenerationOptions.timeout: Option<Duration>` 和插件的 `with_default_timeout(Duration)`。canonical 默认变为有限的 600 秒；调用未指定时继承默认，指定时取调用值与 runtime 值中更严格者。
- 绝对 deadline 在 admission 固定，排队、请求记录、预检和执行共用同一额度，不在出队或调用 provider 时重置。构造时拒绝超出 executor 时钟范围的默认值；无效的有效调用时长返回 `InvalidTimeout`。零 timeout 立即超时且不启动 provider。
- `ModelRequest.options.timeout` 继续记录实际传给 raw adapter 的有效 Duration；同一有效选项原样传递，不在排队或请求提交后改成另一个“剩余 timeout”。外层绝对 deadline 提供更严格的执行约束，adapter 自身从收到请求起计算的相对期限不能放宽外层期限。
- 因既有字段的含义和线格式保持，ModelRequest/ModelResult 继续使用 version 1。本批不往该格式中隐式追加调度指标，不修改历史日志。后续若持久记录原始额度、执行参数或调度报告，需要单独明确事件及版本契约。

## 作业阶段、记录与收尾

| 阶段 | 含义与容量 |
| --- | --- |
| Queued | 已接受、等待执行槽；占等待容量及总在途容量 |
| Preparing | 已分配执行槽，等待请求记录或完成预检；占执行槽及总在途容量 |
| Executing | 正在驱动 provider，包括流式消费与背压；占执行槽及总在途容量 |
| Cleaning | 已不再排队或占执行槽，仍等待已接受的记录/收尾；继续占总在途容量 |

所有已接受作业立即启动自己的 ModelRequest append，包括 Queued 作业。记录进度与调度阶段分别观察，不能由 Queued 推断“没有规范请求”。请求确认后且已取得执行槽、未取消/超时、预检成功时，才允许调用 provider。

排队取消、超时或 stream consumer drop 要撤销等待资格并释放适用容量；同一个已经接受的 request append future 仍由作业持有并等待，不能丢弃后重新 append 或假称未提交。请求确认后写入相应失败结果；请求记录失败则保留原有 `ModelClosureFailure`。

provider 完成、失败或受控取消后释放执行槽，结果记录继续占总在途容量。ModelResult 确认前不交付成功完整响应或 Finished。记录无法确认时，报告 Recording 错误及已有证据；不让普通 timeout 覆盖规范收尾失败。

Turn finish 和 runtime shutdown 排空已接受作业。执行 deadline 不给任意持久化工作施加强制终止承诺；已经接受的记录继续收尾。超时或取消不能通过 drop future 伪装完成，也不承诺撤销外部副作用或终止任意进程内阻塞实现。

## 错误与观察

- 容量拒绝使用 `ModelRuntimeError::Overloaded { capacity }`，capacity 区分 `ModelOverloadKind::QueueFull` 和 `InflightFull`；未接受的请求不产生 provider 执行或伪造完成证据。
- 只有排队期限耗尽返回类型化 `ModelRuntimeError::QueueTimeout`，规范结果为 Timeout 分类、`model_queue_timeout` code。其他受控超时沿用 `LlmError::Timeout`。取消、runtime 停止、Turn 已关闭及记录失败继续保留各自分类。
- `LlmRuntime::scheduler_snapshot()` 返回可选只读快照。自定义 runtime 默认 None，表示未提供该观察能力，不伪装成零作业。快照只包含有限的当前在途作业、阶段计数、时长、记录进行中/已确认标记和停止原因，不含 prompt，不维护无界已完成历史。
- 快照是进程内观察，不是规范持久报告、取消凭证或恢复快照。清理尚未完成的工作保持可观察并计入总容量；空在途列表也不能恢复过去的统计或证明整个业务任务完成。

## 后续边界与验证目标

Task 共享预算仍在 JW-04-c 注入 Agent/Model/Tool 绑定。本批有限调度及单次 deadline 不自动消费 `TaskBudget`，不完成累计 token、工具额度或跨 Turn/重启预算验收；预算持久化仍归 JW-04-d，完整任务恢复归 JW-07。

验证应覆盖默认/无效配置、并发/等待/总容量峰值、FIFO 分配、请求体积拒绝、排队超时/取消、请求与结果记录阻塞、消费者丢弃、背压、零 timeout、绝对期限不重置、V1 实参一致、finish/shutdown 排空与快照的有界观察。指南同步说明默认值和兼容变化；本 ADR 不表示这些检查已经通过。
