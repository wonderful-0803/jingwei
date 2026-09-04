# API 参考

指南讲解使用方式与边界，完整类型、方法签名和 rustdoc 注释由同一份 Rust 源码生成。当前 v0.1 尚未正式发布，不将 docs.rs 的其他版本或缺失页面作为当前 API 的参考。

## 生成当前源码的参考文档

在仓库根目录运行：

```sh
cargo doc --workspace --all-features --no-deps --locked
```

使用浏览器打开生成的 `target/doc/jingwei/index.html`。指南站与 rustdoc 独立构建，阅读静态指南不需要 Rust 工具链；生成 API 文档才需要 Rust 和对应锁定依赖。

## 从哪个模块开始

| 需求 | 公共入口 | 使用指南 |
| --- | --- | --- |
| 组合框架与生命周期 | `jingwei::runtime`、`jingwei::plugin` | [开发环境](development.md) |
| 自定义 Agent 与回合 | `jingwei::agent` | [任务预算](task-budget.md) |
| 文本、原生工具、JSON Schema 生成 | `jingwei::llm` | [模型协议](model-protocol.md) |
| 单步 CallTool / Final / AskUser | `jingwei::action`，需启用 `actions` feature | [单步动作](action-step.md) |
| 共享账本、检查点与宿主增额 | `jingwei::budget` | [持久预算](durable-budget.md)、[审计增额](budget-grants.md) |
| 工具、会话与规范事件 | `jingwei::tool`、`jingwei::session`、`jingwei::event` | [模型调度](model-scheduling.md)、[运行报告](task-budget.md) |
| 本地文件检查点适配器 | 单独依赖 `jingwei-budget-file` | [文件检查点](file-checkpoint-store.md) |

默认 façade 不启用可选 action 组件，也不包含文件检查点适配器。只引入应用实际需要的组件，不把指南目录或验证工程加入 Cargo 依赖。

## 源码与版本

开发源码位于 [GitHub dev 分支](https://github.com/wonderful-0803/jingwei/tree/dev)。实际集成应固定到同一个已取得的提交；分支会继续变化，本地尚未推送的代码也不会出现在远端。

后续正式发布时，再把对应版本的 rustdoc 链接加入本页。不要将本指南可构建理解为完整 v0.1、全部目标设备或故障恢复已验收。
