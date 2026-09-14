# ADR-0013：确定性对话投影

状态：已采纳，2026-09-14。接续 ADR-0012；实现范围 JW-05-b / F2.1、F2.2。

## 决策

增加独立、默认关闭的 jingwei-context，通过 facade context feature 访问。ConversationProjector 是同步纯计算接口，CanonicalConversationProjector 算法版本为 1；复用 core 的 ModelMessage 和 SessionEvent、session 的物理日志校验，不依赖 runtime、provider 或 IO adapter。

输入为完整物理 Session 快照。先检查数量和序列化字节上限，再核验 Session/seq/event 身份；每个 Turn 必须连续，终态之后不允许事件，回复唯一且具有 MessageId。输出包含消息组、来源事件、Turn 状态和省略原因，并保存实际配置与来源尾部。所有输出順序来自物理序列；哈希容器仅用于查找，不影响输出。

成功和等待输入以 AssistantMessage 为正文，delta 不再拼接。模型请求/结果仅作审计，不能把提议当执行。实际工具调用按调用顺序与同 Turn 结果配成完整组；合成 provider ID 来自唯一物理 seq，跨 Turn 重用 runtime ID 不会混淆。结果保留完整成功/失败信封，tool 身份不提升到 system。此投影是执行历史表示，不能作为授权或重试依据。

失败/取消保留已确认工具交换，省略完整回复，报告 Turn 状态。未配对调用拒绝，不能静默丢失不确定副作用。只允许用户消息构成的最后一个 Open Turn。旧成功回合缺少完整消息默认拒绝；显式 Omit 只省略未知正文并报告，不从 delta 推断、不改写旧日志。

默认资源限制为十万事件、16 MiB 序列化输入、五万输出消息，超限整体失败，不裁半个组。没有 token 预算或压缩，后续选择器必须尊重组完整性。输入是宿主提供的快照，类型与来源报告本身不证明持久化真实性。

## 取舍与验证

采用受控工具执行记录而非重放 provider 候选，兼容原生工具、JSON 动作和直接工具执行；因此不保存 provider 原始调用 ID 作为投影关联键。原始事件仍在日志中供审计。

完整快照及保守拒绝规则暂不支持增量投影、部分 Turn 恢复或未终止能力运行的上下文构建。后续可添加版本化策略，不能悄悄改变版本 1 含义。

私有契约覆盖确定性、并行结果逆序、跨 Turn ID 重用、候选未执行、旧日志、失败与不确定工作、身份错误和资源边界，并接入真实 Agent/新进程 JSONL 历史测试。验收见 [JW-05-b](../implementation/JW-05-b.md)。
