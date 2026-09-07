# 开发环境与兼容性

## 当前源码快照

workspace 目前由 15 个 crate 组成，包版本暂为 0.1.0；这是开发中的 v0.1 功能里程碑，不表示这些新增功能已经发布到 crates.io。

Rust 开发版本固定为 1.96.0，最低支持声明同步为 1.96。此前的 1.85 声明与源码使用的语法不符；本轮基于真实构建结果收敛声明，未承诺更旧工具链。

`rust-toolchain.toml` 会指定开发版本及 Rustfmt/Clippy。Windows x64 使用 MSVC 构建工具和 Windows SDK；目前只完成本机 Windows x64/MSVC 验证，不能据此宣称所有端侧平台已验证。

## 框架源码检查

准备依赖后，在 workspace 根目录运行：

```text
cargo check --workspace --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo doc --workspace --all-features --no-deps
```

生成的 API 文档入口在 target/doc/jingwei/index.html。指南 Markdown 位于独立的 docs/guide 区域，不是任何 crate 的编译输入。

内部契约测试、独立示例、故障注入及测试数据不随公共交付提供；框架源码和公开指南不依赖这些私有文件。指南教学片段由内部工程编译验证，而不是让公开文档构建依赖测试套件。

## 扩展接口的兼容原则

新增 Llm/ModelGateway 能力查询具有默认实现；旧 provider 无需增加方法即可编译。没有能力声明时，新协议预检会报告 Unknown，现有文本调用路径保持可用。

开发阶段每次接口变更同时维护契约说明、旧文本回归和指南片段；真实结构化适配器、跨平台、独立 Cargo 包消费及完整指南站的验收仍在后续阶段进行。
