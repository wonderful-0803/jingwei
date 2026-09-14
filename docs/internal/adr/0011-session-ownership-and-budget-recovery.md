# ADR-0011：Session 写者所有权与原始预算候选恢复

状态：采用。日期：2026-09-14。接续 ADR-0009/0010 与 JW-04-d2-c2。

## 问题

预算文件 CAS 只锁一次存储操作，不能隔离 Session 历史缓存或旧执行者。遗留 claim 保存上个安全边界，不含本轮实际消耗；TaskRunReport 也不能重建完整 ID 水位。直接清 claim、使用 TTL 或仅凭闭合日志重建账本会重复授权。

## 决策

1. JSONL provider 为每 Session 获取永久侧文件的 OS 排他锁，首次 load/commit 获取，同一 provider 克隆共享，保留到全部句柄及后台 IO 释放。使用非阻塞获取；不在每次 append 后释放，避免另一个写者使缓存失效。不同 Session 不串行化。
2. 提供可替换的 SessionRecoveryOwnership 契约；默认 JsonlRecoveryOwnership 使用全新 provider 获取锁，读取及同步确认物理日志。自定义实现必须真实隔离合作的旧写者，不能把一个字符串当作隔离证据。
3. 只恢复原 Commit 错误保留的确切最终候选。核对原 execution_id/revision、quiescent successor、最新日志及报告边界；不退款、不改变限制，不重放工具。原候选已写入则仅返回核验结果。
4. 新恢复用 V4 原子保存候选与有界审计，绑定稳定操作 ID、来源/理由、所有权 ID、确认游标、候选报告和 ID 水位。保护审计前缀，后续执行和增额必须保留。上限 16 条，每文本 1024 字节，满额拒绝。
5. compare_exchange_guarded 接收所有权 Arc。默认拒绝未实现的存储；文件实现将 Arc 移入 blocking worker，取消 waiter 不提前释放锁。既有 compare_exchange 语义保持不变。宿主必须在 runtime 退出后释放全部 provider 克隆，shutdown 本身不偷取仍被持有的所有权。
6. 只读分类不写审计、不授权。不确认结果、缺少候选/ID 水位、日志不匹配或无法获取锁均继续冻结。历史 AlreadyApplied 只是回执。

## 边界与取舍

仅适用于可信目录、稳定文件身份、支持 OS 锁和同步的本地存储。侧锁不可删除/替换；绕过协议的进程、原始 fork 子进程中的任意业务代码和外部副作用不在隔离保证内。持锁文件显式 unlock 后再 close，避免多线程 fork 临时继承句柄拖延已结束操作的锁释放。

没有引入业务审批或分布式租约，也不声称完成 F6。恢复候选真实性依赖受信任宿主保存的原回执；结构校验不是签名。真正中断且没有候选的执行维持冻结。完整文件系统故障矩阵与 Task 业务恢复继续分批验收。
