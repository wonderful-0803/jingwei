# Jingwei

> 为 Rust 应用提供可组合、可审计、显式装配的 Agent runtime 基础设施。

Jingwei 提供稳定的 Agent、会话、模型、工具与插件词汇，以及负责生命周期的
`Harness`。应用保有 Agent 实现、provider 选择、持久化位置和异步运行时的控制权。

v0版本目标是让进程内 Agent 组合清楚、可检查，而不是用隐式配置替代应用决策。

当前工作区正在开发 v0.1。新增统一模型协议、可选 `jingwei-action` 单步动作组件与共享内存预算账本，
入口见[开发指南](docs/guide/src/index.md)、[单步动作执行](docs/guide/src/action-step.md)与[任务预算](docs/guide/src/task-budget.md)。
canonical 模型 runtime 已提供共享并发限制、有限等待队列和从准入开始的截止时间，见[模型调度](docs/guide/src/model-scheduling.md)。
Agent、模型与工具运行时已接入共享 Task 预算，并生成规范运行报告；预算持久恢复仍待实现。
JW-04-d1 增加[预算快照与恢复原语](docs/guide/src/budget-checkpoints.md)，但不自动持久化，也不完成运行时跨进程恢复或增额审计。
这些开发功能不包含在下方固定的旧提交中；使用新功能须基于同一个实际取得的源码快照，
不能把当前进度理解为已发布到 crates.io 的完整 v0.1。

## 开发分支

`dev` 用于持续开发 v0.1，纳入源码、开发指南、PRD、测试、脚本、锁文件及开发记录；
所有层级的 `target/` 编译产物均不纳入。开发分支不是私有存储，推送前须检查凭据和敏感数据。
`main` 不随本次开发快照更新，也不在本次创建发布 tag 或发布 Cargo 包。

```text
git clone --branch dev https://github.com/wonderful-0803/jingwei.git
cd jingwei
cargo check --workspace --all-targets --all-features --locked
```

开发指南保持在独立的 `docs/guide`，测试工程保持在 `tests/model-protocol`。
开发资产入 Git 不改变 Cargo 包的排除规则；正式交付仍需单独核验包内容。

Linux 环境可运行 `bash scripts/check-baseline.sh`（需 Python 3），执行两个 workspace 的
9 项离线检查并保存日志到 `results/baseline`。首次执行前准备指定工具链，并对主 workspace
和 `tests/model-protocol/Cargo.toml` 分别执行 `cargo fetch --locked`；协议测试需允许本机回环端口。

## Why Jingwei?

“Jingwei”取自《山海经》中精卫填海的意象：持续而明确地完成手上的一小步。
这里借用的是“坚持”的含义，而不是对神话作技术承诺。Jingwei 把这种取向落在
基础设施上：每个 provider、runtime 和生命周期步骤都由应用显式组合，因此可以
被审计、替换和长期维护。

## 快速开始

使用仓库指定的 Rust `1.96.0` 工具链。当前公开仓库没有发布 tag，因此请把同一组
Jingwei crate 固定到同一个不可变提交，而不要让它们分别跟随分支：

```toml
[dependencies]
jingwei = { git = "https://github.com/wonderful-0803/jingwei", rev = "3f5f2a8b66567abf5f69508b084e8921d09e5ed5" }
jingwei-standard = { git = "https://github.com/wonderful-0803/jingwei", rev = "3f5f2a8b66567abf5f69508b084e8921d09e5ed5" }
jingwei-journal-jsonl = { git = "https://github.com/wonderful-0803/jingwei", rev = "3f5f2a8b66567abf5f69508b084e8921d09e5ed5" }

# 宿主应用拥有自己的 async runtime。
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

当你要升级时，应将所有 Jingwei crate 一起切换到另一个经确认的相同提交。未来若采用
不可变 tag，也应让所有 Jingwei crate 指向同一个 tag。

以下程序实现一个没有模型能力的 Echo Agent。它只选择 JSONL persistence、canonical
SessionRuntime 与 canonical AgentRuntime，因此不会产生网络或模型请求。

```rust
use std::sync::Arc;

use jingwei::{
    agent::{
        Agent, AgentContext, AgentError, AgentFuture, AgentTurnInput, AgentTurnOutput,
        TurnOutcome,
    },
    id::SessionId,
    plugin::{MountContext, MountError, Plugin, PluginDescriptor},
    HarnessBuilder,
};
use jingwei_journal_jsonl::JsonlSessionPersistencePlugin;
use jingwei_standard::StandardCoreBundle;

struct EchoAgent;

impl Agent for EchoAgent {
    fn run_turn<'a>(
        &'a self,
        input: AgentTurnInput<'a>,
        _ctx: &'a dyn AgentContext,
    ) -> AgentFuture<'a, Result<AgentTurnOutput, AgentError>> {
        Box::pin(async move {
            Ok(AgentTurnOutput {
                final_text: format!("echo: {}", input.user_message),
                outcome: TurnOutcome::Completed,
                artifact: None,
            })
        })
    }
}

struct EchoAgentPlugin;

impl Plugin for EchoAgentPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("getting-started.echo-agent")
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_agent("echo", Arc::new(EchoAgent))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let harness = StandardCoreBundle::new()
        .install_into(HarnessBuilder::new())
        .plugin(JsonlSessionPersistencePlugin::new("jingwei-data"))
        .plugin(EchoAgentPlugin)
        .select_persistence("jsonl")
        .select_session_runtime("canonical")
        .select_agent_runtime("canonical")
        .build()
        .await?;

    let session_id = SessionId::new();
    let turn = harness.run_turn(&session_id, "echo", "hello, Jingwei").await;

    // 即使本轮失败，也要完成显式异步清理。
    let shutdown = harness.shutdown().await;
    let report = turn?;
    shutdown?;

    println!("{}", report.final_text());
    Ok(())
}
```

输出为 `echo: hello, Jingwei`。JSONL 数据写入 `jingwei-data`，其路径、保留策略与
访问控制均由应用负责。

## 组成与包边界

| Crate | 责任 |
| --- | --- |
| `jingwei` | 面向应用的窄 façade：`Harness`、Agent/Session/Plugin 词汇和稳定 re-export；不隐式安装 provider。 |
| `jingwei-standard` | 安装四个 canonical runtime 候选项；不会选择或激活模型、持久化或工具 provider。 |
| `jingwei-journal-jsonl` | 由应用选择的 append-only JSONL `SessionPersistence` provider。 |
| `jingwei-openai` | 可选的 OpenAI-compatible 原始 LLM provider，适用于 `/chat/completions` 端点。 |
| `jingwei-action` | 可选单步动作组件：原生/JSON 协议、CallTool/Final/AskUser、受控执行及证据关联；通过 façade 的 `actions` feature 启用。 |
| `jingwei-budget` | 轻量共享 Task 预算账本：原子预留、保守结算、跨 run 累计与时间报告；通过 `jingwei::budget` 访问，不负责执行调度或持久恢复。 |
| `jingwei-llm-runtime` | 完整/流式模型调用共享的有限调度、超时、取消和规范记录屏障；总在途限制覆盖尚未完成的记录和清理。 |

`tokio`、Agent 实现、provider 配置和数据目录都属于宿主应用。若应用需要工具能力，
应显式添加对应的 tool provider 与 runtime 选择；安装 `jingwei-standard` 本身不会启用工具循环。

## 可选：OpenAI-compatible provider

仅当 Agent 明确需要模型能力时，再添加该依赖，并固定为与其余 Jingwei crate 相同的提交：

```toml
[dependencies]
jingwei-openai = { git = "https://github.com/wonderful-0803/jingwei", rev = "3f5f2a8b66567abf5f69508b084e8921d09e5ed5" }
```

然后在上面的 builder 链中加入并显式选择 provider 与 canonical LLM runtime：

```rust
use jingwei_openai::{OpenAiConfig, OpenAiLlmPlugin, OPENAI_PROVIDER_KEY};

let openai = OpenAiConfig::new(
    "http://127.0.0.1:8000/v1",
    "your-model-name",
)?
.with_api_key(std::env::var("OPENAI_API_KEY").ok())
.with_system_proxy(false);

let harness = StandardCoreBundle::new()
    .install_into(HarnessBuilder::new())
    .plugin(JsonlSessionPersistencePlugin::new("jingwei-data"))
    .plugin(OpenAiLlmPlugin::new(openai))
    .plugin(EchoAgentPlugin)
    .select_persistence("jsonl")
    .select_session_runtime("canonical")
    .select_agent_runtime("canonical")
    .select_llm(OPENAI_PROVIDER_KEY)
    .select_llm_runtime("canonical")
    .build()
    .await?;
```

构造和选择该 provider 不会自行请求端点。只有明确声明 LLM 能力、并在 turn 中调用
`AgentContext::model()` 的 Agent 才会发起模型调用。应用应从合适的 secret source 提供 API key。

## 生命周期与边界

- `Harness::build().await` 在选中 provider 的依赖闭包完成构造和启动后返回；调用方必须在不再使用
  runtime 时执行 `Harness::shutdown().await`。Drop、取消或遗弃 future 不能替代异步清理。
- 同一 `SessionId` 的 turn 在一个 runtime 内串行化；不同会话可以独立推进。
- JSONL adapter 支持同一实例及其 clone 的进程内协调，不支持多个 adapter 实例或多个进程同时写入同一数据根目录。
- v0 不自动选择模型、provider、持久化位置或配置来源，也不承诺动态插件加载、稳定 ABI、通用模型驱动
  工具循环或跨进程 JSONL 协调。

## License

Jingwei is dual-licensed under either of:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.
