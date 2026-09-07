# ADR-0008：可选的本地文件预算检查点存储

日期：2026-09-04。状态：采用；实现批次 JW-04-d2-a。前置：[ADR-0007](0007-budget-checkpoints.md)。

后续修订：本文保留 d2-a 时的 V1 检查点描述。d2-b 增加 V2 检查点占用语义，文件记录外层仍为数字版本 1；接续契约见 [ADR-0009](0009-durable-budget-execution.md)。

## 决策与框架边界

新增独立 `jingwei-budget-file`，实现 `BudgetCheckpointStore`，由宿主显式选择。预算核心仍无 IO/Tokio，facade 默认依赖图不引入这个适配器；不新增全局 provider，不把业务 Task payload 混入预算检查点。

每个文件保存一个 Task/Session/Agent 绑定的追加式检查点链。宿主负责映射并预先准备持久的普通文件及目录项；适配器只打开既有文件，绝不自动创建、截断、重命名或修复。空的既有文件返回 None；缺失文件返回错误。历史上执行过的 Task 即使存储为空，也不能据此重新获得额度。

这避免在跨平台目录持久化尚未实现时承诺“新建文件已具备掉电持久性”。运行期间宿主必须保证文件名/底层文件稳定，禁止替换、unlink、截断或绕过锁写入。不覆盖恶意路径、外部篡改、NFS/分布式文件系统和硬件失效。

## 存储协议与锁

每次操作在 Tokio blocking pool 中打开独立 read/write handle，使用非阻塞排他 OS 文件锁覆盖完整读取、版本比较、追加和 sync。不同 store 实例和不同进程使用同一底层文件时共享这条协调边界；不会复用 cloned handle 或依赖仅进程内的 Mutex。

依据 Rust 的 [File::try_lock](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock) 和 [File::sync_all](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all) 契约实现。文件锁及同步语义须由部署文件系统支持；没有从这些 API 推导出未验证的物理掉电保证。Rust 1.96.0 构建验证该 API 可用。

记录格式为单行 JSON，加一个结尾换行：

```json
{"version":1,"expected_revision":0,"checkpoint":{"...":"ADR-0007 的完整检查点"}}
```

上例只展示外层结构，省略号不是有效检查点。外层版本独立于检查点内部版本，均为必填数字 1。记录严格拒绝未知字段。读取逐条核验检查点不变量、绑定一致性，以及从 revision 1 开始的连续链；不重排，不跳过空行或损坏尾部。版本不支持、重复/缺失 revision、半行和不合法检查点均拒绝。

CAS 使用真实最新 revision；只接受 expected + 1。同 revision 的竞争镜像不能覆盖既有值。只有当前最新镜像与输入检查点完全相等时才返回 ReplayedExact（数据结构相等，不要求 JSON 属性顺序一致）；更旧镜像即使曾提交也返回 Conflict。追加成功必须经过 sync_all；load 和精确重试也先 sync_all，允许把完整但未获确认的写入重新确认。锁只保护一次存储操作，不是 Task 的执行租约。

## 失败及生命周期

| 结果 | 本次行为与宿主责任 |
| --- | --- |
| Busy | 本实例作业满或 OS 锁冲突；本次没有写入，宿主自行决定有界退避 |
| Closed | 共享实例已停止接收；本次没有写入 |
| Invalid / Conflict / LimitExceeded | 本次不写入；修正调用或核对状态，不能跳过版本/放宽额度继续 |
| Corrupt | 日志无法验证；不修改文件，任务保持不可恢复，转人工/后续核验流程 |
| Storage / DefinitelyNotCommitted | 本次在文件写入前失败；不是任何早先写入未提交的证明 |
| Storage / Indeterminate | 已尝试 write/sync，或已接受作业异常退出；不假装回滚，不推进下一 revision，先核对原操作 |

部分写入后不回滚、不截尾；完整记录可以在下一次 load/精确重试时同步确认，半记录则 fail closed。不可把读失败当作 None。缺失或损坏尾部不会变成免费重跑授权。

默认有限值：记录 8 MiB（含换行）、整个日志 64 MiB、每个共享实例最多 4 个在途 IO 作业。读、编码、追加均检查字节限制，日志满即拒绝，本批不提供自动压缩。限制不代表整个进程 RSS 上限；不同实例不共享准入计数，宿主仍需控制实例数和输入对象大小。

future 首次 poll 时准入；成功准入后容量归 blocking worker 所有。调用者丢弃等待 future，不释放该容量、不取消写入；close 停止所有 clone 的准入并排空已接受作业。close 等待者离开仍保持关闭，可再次等待。status 仅报告 accepting/in_flight/max_io_jobs，不冒充未被调用者接收的写入回执；未知结果须另开 store 核对。宿主必须维持 Tokio runtime 至排空。

## 不在本批宣称的保证

- 没有跨进程执行所有权、运行前持久占用、运行时自动保存、日志游标真实性核验或增额审计。
- 多个宿主仍可能先 load 同一镜像，再分别 restore；单次文件 CAS 不能阻止这件事。后续协调器必须在执行前建立持久排他保护。
- JSONL Session adapter 的跨进程写协调并未因新增预算文件锁自动获得保证。
- 本批实际 Windows 子进程、尾部损坏和被丢弃等待者测试不是电源故障、磁盘写满或 sync_all 注入失败测试；这些故障验证仍需补齐。
- 完整 AT-F4-04 / F6 验收门槛不变，后续见 [JW-04 计划](../implementation/JW-04-plan.md)。
