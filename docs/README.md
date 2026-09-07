# 开发指南目录

本目录用于 Jingwei 开发指南，与框架 Rust 源码和 Cargo 包保持隔离。现有教程已接入 VitePress 静态站，可从 [指南概览](guide/src/index.md) 阅读正文，或按[站点工程说明](guide/README.md)构建预览；完整 v0.1 内容仍随功能开发补齐。

## 目录职责

| 区域 | 用途 | 交付方式 |
| --- | --- | --- |
| workspace 的 crates 区域 | 框架源码与各 crate 的清单 | 纳入 dev；按各包边界发布 |
| 本目录的 guide 子目录 | 指南 Markdown、指南构建配置和必要文档资源 | 纳入 dev，独立构建指南站，不加入 Cargo workspace members |
| 本目录的 internal 子目录 | 设计决策和实施记录 | 纳入 dev；不进入 Cargo 包 |
| 指南构建输出 | 生成的 HTML、搜索索引和缓存 | 可重建，不进入 Cargo 包；target 目录始终不跟踪 |
| 内部验证工程 | 独立示例、测试、fixtures、故障注入和原始评测资料 | 纳入 dev；不进入 Cargo 包，不作为指南或框架的构建依赖 |

2026-09-03 起，按用户确认的开发分支策略，dev 可保存除 target 目录外的全部开发资产。
这替代了先前“仅指南入 Git、内部资料被忽略”的规则，不改变正式发布与 Cargo 包的隔离要求。
internal/“内部”表示资料用途，不表示 GitHub 访问权限；开发分支不是隐私边界。
后续面向使用者的指南内容仍统一放入 guide 子目录。

## 内容边界

- 指南保留学习 API 所需的说明和自包含教学代码片段，不提供独立示例项目或测试套件。
- 教学片段由独立验证工程验证；发布指南不依赖 dev 专用测试、示例、数据与报告。
- API reference 来自 Rust 源码中的公共 rustdoc 注释；指南站不是框架的编译输入。
- 指南构建与 crate 构建互不依赖，删除指南目录不应影响框架消费者构建。
- crates.io 打包内容应逐包检查，不能将“Git 未跟踪”当作已经验证包内容的替代。

指南固定使用 VitePress 1.6.4、Vite 6.4.3 安全依赖覆盖、Node.js 24.19.0 和 npm 11.17.0，依赖锁文件独立维护。在 docs/guide 执行 npm ci、npm run docs:build，产物位于 docs/guide/target/site；/jingwei/ 子路径版本使用 docs:build:pages，产物位于 target/pages。节点依赖 node_modules 可重建并单独忽略，所有站点产物与缓存进入 target，不改变 Cargo 发布边界。

本轮没有部署在线站点。后续公开时只发布选定的静态产物，或独立交付指南源码；不能将整个 docs 目录（含 internal）作为站点来源或上传目录。
