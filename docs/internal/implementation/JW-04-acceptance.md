# JW-04 预算与调度验收证据

核对日期：2026-09-14，接续 d2-c3。此表按 PRD 原验收编号区分已验证机制和仍未实现的产品路径，不把预算子域测试等同于 F6 完成。

所有测试名称相对于 `tests/model-protocol/src`。当前 Linux 最终基线见 [summary.json](../../../results/baseline/20260914-111749-129351-au6rsu34/summary.json)，9/9 通过，14 root + 264 private + 13 guide doctest，共 291 项。

| 验收 | 已验证证据 | 尚未闭合的范围 |
| --- | --- | --- |
| AT-F4-01 并发、队列与原子预算 | `scheduler::the_provider_execution_peak_never_exceeds_the_configured_limit`、`a_full_waiting_queue_rejects_without_recording_or_entering_the_provider`、`inflight_capacity_includes_result_recording_after_the_execution_slot_is_released`；`budget::concurrent_requests_cannot_all_reserve_the_last_allowance` | 机制的确定性并发测试通过；目标端侧设备持续压力和真实模型表现留在 JW-08 |
| AT-F4-02 有限退出 | `model_budget::actual_and_unknown_usage_are_settled_independently_for_both_model_modes`、`budget_stop_cancels_running_and_queued_work_without_resetting_usage`；`scheduler::a_queued_deadline_expires_without_entering_the_provider`、`dropping_a_queued_stream_cancels_it_before_provider_entry`；`agent_budget::same_task_keeps_consumed_steps_and_catching_budget_error_cannot_complete` | 预算、未知用量、取消、排队截止已验证；官方循环中模型持续要求继续动作的端到端验收依赖 JW-06 |
| AT-F4-03 所有受控入口计量 | `agent_budget::direct_model_gateway_charges_shared_task_and_swallowed_budget_error_cannot_complete`、`correction_budget_is_charged_and_swallowed_exhaustion_cannot_complete`；`tool_budget::grant_schema_guard_and_approval_denials_consume_attempt_but_refund_output`；ActionStep 的权限/记录屏障测试在 `actions.rs` | 自定义 Agent、直接 gateway、单步动作和修正计数机制已验证；官方参考 Agent 及完整修正策略尚待 JW-06，不能标记整项通过 |
| AT-F4-04 消耗保留与审计增额 | `agent_budget::durable::new_processes_continue_budget_then_refuse_to_replay_interrupted_model_turn`；`grants::grant_and_retry_survive_new_processes_and_intervening_execution`；`recovery::disk_recovery_is_readable_in_a_fresh_process`；`crashes::death_after_settle_recovers_saved_candidate_without_replaying_body` | 正常跨进程续跑、宿主增额和保留原始候选的恢复路径通过；没有候选/结果的中断维持冻结，完整业务任务恢复属于 JW-07/F6 |

## 实际存储故障与中断窗口

| 窗口 | 注入层级与测试 | 断言 |
| --- | --- | --- |
| 首次写入零字节失败 | `checkpoint_file::faults` 的 budget/session kernel_write 测试；测试子进程内 RLIMIT_FSIZE 触发内核 EFBIG | 适配器返回不确定，不声称回滚；重新打开并核验后可精确重试 |
| 写入 17 字节后失败 | 同上，真实 write 部分成功后由内核返回 EFBIG | 完整前缀保留，尾部破损；load 和 CAS/append 拒绝，不自动截断或跳过 |
| sync 调用前失败 | `budget_sync_errors_never_acknowledge_a_visible_candidate`、`session_sync_errors_and_direct_retry_require_confirmation`；私有 libc 边界返回 EIO | 完整可见行不能被确认成功，原操作保留不确定性 |
| 真正 sync 成功后回执失败 | 同一组测试的 sync-after 模式，先执行真实 fsync/fdatasync 再返回 EIO | 核验后精确重试不重复写入，不把回执失败理解为未执行 |
| 完整 write 后、sync 前进程退出 | `complete_write_then_process_exit_requires_restart_confirmation`，测试子进程直接退出，不运行 Rust 析构 | 新可执行进程重新同步确认与精确重试；不模拟磁盘掉电 |
| claim 已确认、Session 尚未准入 | `crashes::death_after_claim_before_session_admission_never_authorizes_replay`，终止受控子进程 | 没有 Session 新事件也不释放 claim，预算文件不变 |
| 模型已完成、本轮尚未关闭 | 既有 `new_processes_continue_budget_then_refuse_to_replay_interrupted_model_turn` | 保持冻结，不重放模型请求或重置旧额度 |
| settle 完成、最终检查点失败后进程退出 | `crashes::death_after_settle_recovers_saved_candidate_without_replaying_body`；最终提交在契约层拒绝，保存原 Commit 候选，再终止子进程 | 新进程持锁核验恢复，重复恢复只返回回执，Session 字节不变，已消耗一步及全部报告字段保留 |
| 恢复等待者取消，后台文件 IO 已接收 | 既有 `recovery::cancelled_file_commit_retains_owner_until_accepted_worker_finishes` | Session 所有权一直保留到后台工作完成 |

同步错误由私有 libc 包装器注入，写入错误由真实内核文件大小限制产生；两者都调用生产文件适配器，没有加入生产环境故障开关。不得把同步错误注入称为真实磁盘故障，也不得把进程退出称为硬件掉电。

## 仍保留的验收义务

- 真正设备掉电、ENOSPC/介质错误、网络文件系统、目录项持久化及非合作写者不在本批保证内；部署仍要求可信本地目录、稳定文件身份、有效文件锁与同步语义。
- 任意工具副作用和未确认结果不能自动重放；缺少原始候选及 ID 水位时继续冻结。
- 官方参考 Agent、完整修正策略、Task payload、待答状态恢复仍未交付，不以本表取消 PRD 中相应要求。
- 后续进入 JW-05：先规范持久化 `final_text` 和完整 assistant 消息，再建立对话投影及上下文预算。JW-06 交付后补齐 AT-F4-02/03 的官方策略路径，JW-07 收口 F6。
