# 执行同步与交接协议 v1

## 1. 适用范围与信息来源

适用于共享同一个本地工作区的多个 LLM/人类协作者。不是分布式事务系统，没有自动拦截 Agent 工具调用。

优先级：用户最新明确指令 → 实际文件和验证证据 → 追加事件 → STATE/TASKS 快照 → 历史 handoff。不同来源冲突时保留冲突并核实，不能悄悄覆盖。

事件是执行历史；STATE/TASKS 是它的当前投影；DECISIONS 记录设计依据；handoff 是接手导航。它们不应各自发明不同进度。

## 2. 身份与 ID

每次会话创建唯一 `run_id`，建议 `UTC日期时间-agent-随机后缀`，actor 使用 `codex`、`claude`、`gemini` 或实际模型/操作者名。不得复用别人的身份。

任务使用稳定 ID（如 W001、H010）；每次执行使用独立 `execution_id`；事件使用全工作区唯一 UUID `event_id`。时间一律 UTC ISO 8601。

## 3. 短期元数据写入锁

协作者仅用以下原子操作尝试获取锁：

```bash
cd "$(git rev-parse --show-toplevel)/contrib/slm-harness"
mkdir -p .coordination
mkdir .coordination/write.lock
```

仅 `mkdir .coordination/write.lock` 成功才是获得锁。随后写 `.coordination/write.lock/owner.json`，含 actor、run_id、获取时间和本机可验证的 owner 信息。失败时不能写共享日志和快照；退避或先做不依赖认领的阅读，不覆盖已有 owner。

锁内只做：核对快照 revision → 检查任务/文件认领 → 追加事件 → 写新快照 → 释放锁。快照先写到同目录唯一临时文件，再 rename 替换。事件先持久化，快照带 `last_event_id`。

释放时核对 run_id，仅删除自己的 owner 文件并用 `rmdir` 删除锁目录，不递归删除不明内容。

不得只因超时就认为锁失效。发现空 owner、崩溃或遗留锁，先核对相关进程、任务和可联系的协作者，确认不再有写者后才能恢复；无法确认就标记等待协调。任务认领在 TASKS 中持续存在，不依赖短期写入锁是否释放。

此锁仅保护遵守协议、共用同一路径的协作者；不同克隆、不同机器或绕开协议的写入不受保护。

## 4. 执行事务

### 开始

1. 读取最新状态，确认 task 的 owner 和 writable paths。
2. 获取元数据锁。若另一活动任务存在重叠范围，停止认领并协调；只读访问不构成文件写冲突。
3. 追加 `execution_started`，更新任务为 `in_progress`、STATE 的活动 execution 和 revision。
4. 释放锁，运行命令或工具。长任务将日志写到唯一 artifacts 子目录。

### 结束

1. 核对退出码、实际改动和证据；不能仅依据最后一行输出。
2. 获取元数据锁，追加终结事件，刷新 STATE/TASKS。
3. 若后续要验证，任务保持 in_progress 或进入 review，不提前标 done。
4. 释放锁，继续下次执行；聊天更新可引用状态，不代替磁盘同步。

用于维护日志与快照的写操作不递归记账。一个明确标注的 workspace-maintenance 批次可以涵盖文件生成、内容校验与最终同步；事件中列出所有子步骤。

任何 future/工具调用被中断且结果未知，都保留 `running` 或 `unknown`，记录能恢复的 session/PID。新接手者先核对结果，禁止盲目重放可能有副作用的执行。

## 5. 事件格式

按 UTC 日期写 `events/YYYY-MM-DD.jsonl`，每行一个完整 JSON 对象。以下为结构示例，不是实际执行证据：

```json
{
  "schema_version": 1,
  "event_id": "unique-uuid",
  "timestamp": "2026-09-21T00:00:00Z",
  "actor": "agent-name",
  "run_id": "unique-run-id",
  "task_id": "H010",
  "execution_id": "unique-execution-id",
  "type": "execution_finished",
  "status": "succeeded",
  "summary": "具体结果，不能只写 done",
  "operations": ["命令或工具操作的脱敏摘要"],
  "files_changed": ["相对工作区的路径"],
  "evidence": ["artifacts/运行ID/日志文件"],
  "verification": {"exit_code": 0, "result": "具体通过/失败/未验证项"},
  "next_action": "下一条具体操作"
}
```

type 使用 `task_claimed`、`execution_started`、`execution_progress`、`execution_finished`、`execution_failed`、`task_released`、`decision_recorded`、`handoff`、`recovery`、`bootstrap_snapshot`。

status 使用 `running`、`succeeded`、`failed`、`cancelled`、`unknown`、`recorded`。无退出码的操作使用 null，不虚构 0。历史聊天事实用 `bootstrap_snapshot` 并注明来源，不能伪造历史 execution_started。

不保存完整私密推理；不复制敏感业务数据、密钥、HTTP 授权头和全量环境。日志不替代秘密管理。

## 6. 任务状态与验证

状态：`ready` → `in_progress` → `review` → `done`；另有 `blocked`、`cancelled`。

- ready：依赖满足，可认领。
- in_progress：有 owner、run ID、写范围和具体下一步。
- review：产物已存在，等待规定的设计审阅或独立验证。
- done：验收证据已关联；设计文件完成不代表实现完成。
- blocked：必须写明阻塞和解除条件，不能只写“等一下”。

测试失败仍可完成“调查”任务，但不能完成“修复并验证”任务。dirty diff、未提交更改、构建缓存、服务状态都要如实说明。

## 7. 交接和恢复

在 handoffs 中用模板创建不可覆盖的时间戳文件，STATE 指向最新文件。最终回复提供入口路径。

异常恢复顺序：核对锁和 owner → 读取事件末尾 → 找未闭合执行 → 核对进程/日志/文件 → 追加 recovery 事件 → 重建快照 → 再认领任务。

若事件追加成功但快照写失败，以日志和实际证据恢复快照，不重跑业务动作。JSONL 尾部损坏时保留原文件及副本，记录修复；不得静默丢弃无法解释的执行。

事件唯一标识和 revision 用于发现冲突，不提供跨主机共识。未来若采用多机器 Git 协作，必须指定唯一状态汇总者或集中协调服务，不能直接照搬 mkdir 锁。
