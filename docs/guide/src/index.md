# Jingwei 开发指南

Jingwei 是面向端侧小模型的 Rust Agent 运行框架。它提供可组合的模型、工具、会话和运行时契约，由宿主决定业务目标、权限、模型与数据位置。

::: warning 开发版本
本指南对应开发中的 v0.1。已实现能力与尚未交付的恢复边界在各章说明，不代表框架已经发布到 crates.io 或完成全部验收。
:::

## 当前可用内容

现有框架可以装配自定义 Agent、受控模型调用、工具执行和会话日志。模型生成已经统一为文本、原生工具和 JSON Schema 三种路径；可选的单步动作组件把原生/JSON 动作安全交给 ToolRuntime。真实模型的结构化能力仍需宿主核实，当前验证使用私有假 provider 与本机回环 HTTP。

canonical 模型运行时提供有限执行槽、等待队列、总在途容量和准入截止时间。Agent、模型和工具共享 Task 内存预算，支持跨 Turn 累计、默认有限作用域和规范停止报告。

已提供底层预算快照、旧账本封存和未决状态冻结。显式持久模式通过 BudgetExecutionLease 取得唯一运行占用，canonical runtime 核验日志并提交最终检查点。宿主可在安全边界审计增额；冻结解除及完整 Task 状态恢复尚未交付。

可选的 `jingwei-budget-file` 提供有界本地文件检查点存储、跨进程 CAS 与精确重试，供宿主显式接入；存储操作锁不等同于运行时执行租约。

## 阅读顺序

1. 查看[开发环境与兼容性](development.md)，了解当前源码版本和 Rust 要求。
2. 阅读[模型生成](model-protocol.md)，接入文本/流式输出并理解结构化结果与工具权限的边界。
3. 阅读[模型调度与超时](model-scheduling.md)，配置容量、理解拒绝和观察清理积压。
4. 阅读[单步动作执行](action-step.md)，理解 CallTool、Final、AskUser 与规范证据。
5. 阅读[任务预算](task-budget.md)，把宿主 Task 交给 Agent，并理解预留、用量可信度和运行报告。
6. 阅读[预算快照](budget-checkpoints.md)，了解宿主接入的原语与尚未闭合的持久化边界。
7. 阅读[文件检查点存储](file-checkpoint-store.md)，了解显式接入、失败处理与关闭排空。
8. 阅读[持久预算运行](durable-budget.md)，理解正常续跑、异常冻结、回执与部署限制。
9. 阅读[宿主审计增额](budget-grants.md)，理解安全边界、原子审计和幂等重试。
10. 后续按实现进度补充完整装配入门、上下文、参考 Agent 及完整恢复教程。

本指南使用 VitePress 独立构建，阅读方式见[编写与发布指南](writing-guide.md)。类型和方法签名见 [API 参考](api-reference.md)。完整装配入门、上下文与参考 Agent 教程将随实现补齐。
