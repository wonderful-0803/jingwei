# JW-06-c：有限纠错与待答续跑

日期：2026-09-14。起点 dev / 2b5df40，账户 wonderful-0803。决策见 [ADR-0018](../adr/0018-reference-correction-waiting.md)。

## 交付

- 参考 Agent 默认每 Turn 最多修正 2 次，可禁用；格式与参数错误在工具执行前提供有界结构化反馈，修正先计入共享 Task corrections，再报告并进入下一次模型调用。
- Complete 响应 Schema 拒绝增加只读诊断分支，原始模型结果仍为协议失败，授权/Schema 校验不放宽。ActionStep 在通用 JSON Schema 校验前区分不可见工具；参考 Agent 不将工具拒绝或执行不确定性转成纠错。
- AskUser artifact 增加可持久化 PendingQuestion；ReferenceWaiting 校验成功待答报告、原预算身份和累计消耗，通过匹配提问回合的答复显式生成下一轮请求。拒绝同名零消耗新账本、旧消耗、停止/活动预算及持久化 checkpoint 降级。
- correction_v1 保存失败步骤关联及已扣修正次数；run_v1 corrections 对旧报告默认 0。指南更新默认次数、计费时机、错误兼容性及可编译续跑示例。

## 验证

[最终 Linux GNU 基线](../../../results/baseline/20260914-151608-801290-el7ktesv/summary.json)：9/9 通过，370 项 Rust 测试（14 root、335 private、21 guide doctest）。新增 7 项测试入口覆盖 Native/Json 参数纠错、无效 JSON 后恢复、本地/共享修正上限、禁用/无剩余步骤不扣费、不可见工具与拒绝不纠错、待答续跑保持累计预算、修正报告失败不继续；原等待测试改用新句柄验证成功续跑。两个模型协议测试更新为验证 SchemaRejected 诊断及 Debug 不泄漏，预检失败仍保留旧协议错误。

[首轮基线](../../../results/baseline/20260914-151455-450063-b5vyo9re/summary.json) 功能测试通过，Clippy 指出可合并条件及测试锁作用域问题；修正后全量通过，保留两轮日志。

VitePress 根路径和 /jingwei/ 构建、Markdown 测试及产物检查通过：每份 20 页、840 个链接、3 个搜索查询、0 公开源码文件。没有第三方依赖新增。

开发中发现参数错误先由 canonical 模型 runtime 拦截，补充 SchemaRejected 诊断而非跳过校验。调整新增测试的报告故障注入范围，使其覆盖 correction_v1。沙箱内全量测试的 11 个回环网络测试因监听端口权限失败，完整基线按既有授权方式运行；不改适配器或测试断言绕过网络测试。

## 接续

下一批 JW-06-d 完成 AT-F3 五条公共路径及停止/收尾验收映射。本批不提供全局待答防重放、自动用户认证或跨进程恢复，宿主必须保留单一续跑所有权；持久化任务继续使用既有 lease 流程。
