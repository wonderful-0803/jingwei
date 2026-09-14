# JW-07-d：工具副作用与幂等重试契约

日期：2026-09-14。基于 dev / 7c7b8bf；关联 [JW-07 计划](JW-07-plan.md)、[ADR 0022](../adr/0022-tool-effect-retry-contract.md)。

## 实现

- 在既有 core/tool/runtime 增加 ToolEffectContract、ToolOperation 和 ToolRetryAudit，不新增 crate 或依赖。
- 默认副作用 Unknown；工具显式声明只读、幂等或非幂等及修订。运行时在正文前记录稳定动作键，正文通过 ToolBodyRequest 取得同一身份；每次尝试保留独立调用 ID。
- 只读历史核验与显式重试规划拒绝完成、过期或损坏证据；结果不确定的未知/非幂等工具默认待核验。核验为已完成时阻止重试，不伪造成功结果。
- 重试绑定原 Session/Task/工具/参数/契约，clone 共享一次性消费；原键和审计跟随新调用，当前授权、审批和预算重新生效。原有 Agent 不自动补发失败工具。
- 新增指南与示例，更新阶段计划的验证规则以遵循用户“减少测试轮数”的要求。

## 针对性验证

未运行九项全量基线。执行 workspace 编译检查、相关 core/tool/runtime 静态检查；工具预算/重试集合 20 项通过（其中新增 5 项），新指南示例 1 项编译通过。静态检查提示新审计数据使枚举过大，已将可选 ToolOperation 装箱，并修正冗余 match guard；相关静态检查通过，装箱调整后仅复查 5 项新增重试用例，记录见 [retry-tests.log](../../../results/targeted/JW-07-d/retry-tests.log)。

集成用例中，幂等测试工具先持久写入业务记录再返回失败；重新装配 runtime 后先拒绝本次审批，再显式重新规划并获批重试。三次尝试使用不同 call_id、同一逻辑 key，当前审批再次调用，Task tool_calls 累计为 3，业务记录只有一份。

指南只在本地构建 /jingwei/ 产物一次，Markdown 与页面检查通过：23 页、1045 链接、3 项搜索查询、0 个公开源码文件。GitHub Pages 工作流仍自行检查并发布。

## 后续

该契约不清除冻结预算、不补造缺失结果、不自动打开停止任务，也不保证任意工具 exactly-once。宿主必须先恢复合法执行所有权，再显式使用 ToolGateway 执行计划。下一批 JW-07-e 集中完成 AT-F6 故障矩阵验收，避免每批反复运行全量检查。
