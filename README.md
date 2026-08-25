# Jingwei

Jingwei 是一个面向 Rust 应用的显式组装式 Agent harness。它提供稳定的 Agent、会话、模型、工具与插件词汇，以及一个负责生命周期的 `Harness`；应用程序自行选择 Agent、模型 provider 和持久化位置。

v0 的重点是可组合、可审计的进程内运行时，而不是一套隐式配置、自动选型或业务应用框架。

## 开始前

- 使用仓库固定的 Rust 工具链：Rust `1.96.0`。
- 将所有 Jingwei crate 固定到**同一个已发布 Git tag**。不要让它们分别跟随分支或不同提交。
- `jingwei-openai` 仅在应用需要 OpenAI-compatible 模型 provider 时使用；添加该 crate 本身不会发起模型请求。

下例中的 `<release-tag>` 应替换为维护者实际发布的不可变 tag；在 tag 出现在仓库发布记录前，不应把它作为应用依赖。

## 通过 Git tag 使用

在应用的 `Cargo.toml` 中添加四个公开的直接依赖：

```toml
[dependencies]
jingwei = { git = "https://github.com/wonderful-0803/jingwei", tag = "<release-tag>" }
jingwei-standard = { git = "https://github.com/wonderful-0803/jingwei", tag = "<release-tag>" }
jingwei-journal-jsonl = { git = "https://github.com/wonderful-0803/jingwei", tag = "<release-tag>" }
jingwei-openai = { git = "https://github.com/wonderful-0803/jingwei", tag = "<release-tag>" }

# 应用自己的异步执行器。
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

四个 crate 的职责边界如下：

| Crate | 职责 |
| --- | --- |
| `jingwei` | 面向应用的窄 facade：`Harness`、Agent/Session/Plugin 词汇和稳定 re-export。它不会隐式安装 provider。 |
| `jingwei-standard` | 四个官方 canonical runtime 的候选集合；它只安装候选项，不选择或激活模型、持久化或工具 provider。 |
| `jingwei-journal-jsonl` | 明确选择的 append-only JSONL `SessionPersistence` provider。 |
| `jingwei-openai` | 明确选择的 OpenAI-compatible 原始 LLM provider，适用于 `/chat/completions` 端点。 |

`tokio` 由宿主应用拥有；Jingwei 不创建或接管应用的 async runtime。

## 最小 Echo Agent（不请求模型）

下面的完整示例只激活 JSONL persistence、canonical SessionRuntime 和 canonical AgentRuntime。`EchoAgent` 没有声明模型能力，也不会调用 `ctx.model()`，因此不会产生网络或模型请求。

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
        PluginDescriptor::new("example.echo-agent")
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

    // `shutdown` 是显式的 async 生命周期步骤；即使本轮失败也必须执行它。
    let shutdown = harness.shutdown().await;
    let report = turn?;
    shutdown?;

    println!("{}", report.final_text());
    Ok(())
}
```

输出为 `echo: hello, Jingwei`。JSONL 事件数据会写入 `jingwei-data`；该目录由应用选择和管理。

## 可选：接入 OpenAI-compatible provider

仅在 Agent 需要模型能力时，才添加 provider 并显式选择它和 canonical LLM runtime。下列片段替换或加入上例中 `.build()` 之前的 builder 链：

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

该配置只构造和选择 provider，不会自行请求端点。真正的模型调用只能由一个明确声明 LLM 能力、并在 turn 中调用 `AgentContext::model()` 的 Agent 发起。`OpenAiConfig` 接受 HTTP(S) base URL；例如上例会请求 `<base-url>/chat/completions`。API key 是可选的，应用应自行从适当的 secret source 提供它。

## 生命周期、JSONL 与并发边界

- `Harness::build().await` 只在选中 provider 的依赖闭包完成构造和启动后返回。调用方必须在不再使用它时完成 `Harness::shutdown().await`；Drop、取消或遗弃 future 不能替代异步清理。
- `SessionRuntime` 是会话事件的唯一写入权威。相同 `SessionId` 的 turn 在同一 runtime 内按 mailbox 串行化；不同 session 可以独立推进。
- JSONL adapter 按完整换行记录追加，并在每次持久化后执行 flush 与文件 `sync_data`。损坏或截断的尾部会报错关闭，而不会被静默修复。
- 并发写入仅覆盖同一 `JsonlSessionPersistence` 实例及其 clone。多个 adapter 实例或多个进程若指向同一数据根目录，不受支持；需要跨进程协调时请在应用层提供单写入者或外部协调机制。
- JSONL 文件是会话记录，不是通用数据库、跨进程锁或目录项掉电持久化保证。应用应为数据保留、备份、访问控制和路径生命周期负责。

## v0 边界

v0 不承诺以下能力：

- 自动选择模型、provider、持久化位置或配置来源；这些选择始终由应用显式完成。
- 二进制分发、安装器、Docker 镜像、远程 secret manager 或二进制签名。
- 动态插件加载、稳定 ABI、通用模型驱动工具循环、路由或 replay 驱动执行。
- 多进程 JSONL 协调、历史日志迁移、自动恢复或损坏日志修复。

如果一个应用需要这些能力，应在自身边界实现，或等待后续具有单独兼容性契约的 Jingwei 版本。

## License

Jingwei is dual-licensed under either of:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.
