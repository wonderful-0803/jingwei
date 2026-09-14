# 工具视图、结果引用与动作接入

本章的纯工具视图和内容存储位于 `jingwei::context`。同时启用 facade 的 `actions` 与 `context` feature，可使用 `jingwei::action::ContextualActionStep`；单独使用 jingwei-action 时需显式启用它的 context feature。默认 facade 仍不启用这些组件。

## 授权集合内选择工具

ToolSelector 接收 ToolGateway 已授予的 schema 快照，只返回工具名称。select_tool_view 对返回值再次检查：名称必须属于授权集合，不得重复；实际 schema 从原始集合提取，策略不能替换描述或参数约束。

GroupedToolSelector 默认按名称排序选择全部工具，也可对显式 names 与 active_groups 的成员取并集。分组是宿主元数据，不是授权。空 names 配合空 active_groups 产生空工具视图；未知名称/分组、重复或超限均返回错误，不静默扩大范围。

默认上限为 1024 个授权 schema、64 个可见工具、4 MiB 序列化输入。策略只控制可见性；执行时仍经过 ActionStep 参数校验、ToolGateway 授权、guard 和 approval。

## 将大结果变成可读取的引用

ToolResultPolicy 只能选择原文全部保留或保留一个 UTF-8 前缀长度，不能替换文本、成功/失败状态或消息角色。BoundedResultPolicy 默认完整保留不超过 4096 字节的正文，更长正文保留 512 字节预览，并保存完整原文引用；调用方还需设置最大预览字节数。

```rust
use jingwei::context::*;
use jingwei::event::ToolRecordedOutcome;
use jingwei::id::{EventId, SessionId, TaskId};

let store = MemoryContentStore::new(ContentStoreConfig {
    store_id: "host-memory-1".into(),
    max_entries: 128,
    max_bytes: 1024 * 1024,
    max_content_bytes: 64 * 1024,
    max_page_bytes: 4096,
    max_ttl_ms: 60_000,
})?;
let scope = ContentScope {
    session_id: SessionId::from("session-1"),
    task_id: TaskId::from("task-1"),
    expires_at_ms: 60_000,
};
let outcome = ToolRecordedOutcome::Succeeded { output: "工具数据".repeat(1000) };
let view = project_tool_result(
    &outcome, &EventId::from("confirmed-result-event"), &scope, 1,
    &BoundedResultPolicy::default(), &store, 4096,
)?;
let ToolOutcomeView::Succeeded { output } = &view.outcome else { unreachable!() };
assert!(output.truncated);
let reference = output.reference.as_ref().unwrap();
let page = store.read(reference, &scope, 2, 0, 4096)?;
assert!(page.next_offset.is_some());
# Ok::<(), ViewError>(())
```

示例中的来源 ID 在实际接入中应使用已确认 ToolResult 的 event_id。引用包含 store_id、来源事件、Session/Task 作用域、完整字节数和绝对有效期，不包含文件路径。

宿主必须从可信状态提供 scope 和 now_ms；不能把模型提交的 scope 当成读取授权。read 校验引用与保存的元数据完全一致，以及当前作用域、有效期和页面上限。字节游标必须位于字符边界；每页向下取完整字符，装不下一个字符时明确失败。模型需要读取下一页时，宿主应提供单独授权的读取工具，并在工具内注入可信作用域；框架不会自动授予该工具或解析任意文件路径。

MemoryContentStore 由宿主显式创建，限制条目数、总原文字节数、单结果大小、页大小和 TTL。同一有效来源重复写相同原文幂等，内容或作用域冲突拒绝，不覆盖旧数据。put 自动清理已过期条目，也可调用 purge_expired 主动释放；read 不返回过期内容。宿主为每个存储实例选择不混淆的命名空间，时间应来自一致、不可由模型回拨的时钟。

这是进程内存储，进程重启后引用不可用。宿主可实现 ContentStore 接入自己的持久存储，但必须遵循相同作用域、不可变来源、容量和有效期契约。失败结果保留 category、code、retryable，仅对 message 正文进行受控精简。

## 预算之后执行一小步

ContextualActionStep 支持官方 Native 和 Json 协议。它先从授权集合选择工具，再生成完整协议指令和 schema，包括原生 AskUser 控制 schema；这些内容全部进入 ContextBuilder。经过计数的 GenerationRequest 原样发送，不在计数后再次扩展协议。

```rust,no_run
use jingwei::action::*;
use jingwei::agent::AgentContext;
use jingwei::context::*;
use jingwei::llm::ModelMessage;

async fn next_step(
    ctx: &dyn AgentContext,
    history: &ConversationProjection,
    current: &[ModelMessage],
    scope: &ContentScope,
    target: &ContextTarget,
    store: &dyn ContentStore,
    now_ms: u64,
) -> Result<ContextActionReport, Box<dyn std::error::Error>> {
    let task_id = ctx.budget().ok_or("missing task budget")?.report()?.identity.task_id;
    Ok(ContextualActionStep { protocol: ContextActionProtocol::Native }.run(
        ContextActionInput {
            task_id, history, system: &[], current, pinned_turns: &[],
            state_version: Some("host-state-v1"), target,
            budget: ContextBudget {
                window_tokens: 8192, output_reserve: 512,
                output_evidence: TokenBoundEvidence::Estimate,
                safety_margin: 128, mode: TokenBudgetMode::Soft,
            },
            limits: ContextBuildLimits::default(), content_scope: scope, now_ms,
        },
        ContextActionPolicies {
            counter: &ByteHeuristicCounter::default(),
            selector: &GroupedToolSelector::default(),
            result: &BoundedResultPolicy::default(), store,
            tool_limits: ToolViewLimits::default(), max_preview_bytes: 4096,
        },
        ctx.model().ok_or("missing model")?, ctx.tools().ok_or("missing tools")?,
        ctx.cancellation(), ActionStepOptions::default(),
    ).await?)
}
```

history 只包含已终止历史，current 只包含本回合消息。下一步传 report.next_current，配合同一历史和宿主 system；不要将 report.step.next_messages 整体再次作为 current，否则会重复历史或混入 system。生成的协议指令不会积累到 next_current 中。

报告包含实际 ActionStep 证据、上下文计数、工具视图和结果视图。原生路径保持 Tool 消息，JSON 路径沿用带 jingwei_tool_result 标记的用户角色数据信封，两者都明确标记 untrusted_tool_data，工具文本不进入 system。结果只改变后续模型视图，原始 ToolExecution 和 Session 日志仍保留完整正文。历史投影中的既有大结果仍由上下文预算整回合选择；本适配器不改写历史投影。

输出 max_tokens 会被设置或收紧到 output_reserve 内；但该选项本身不证明 provider 实际执行上界。硬模式仍需可信目标计数器和可执行输出上界，模型、模板配置由宿主绑定，参见[上下文预算](context-budget.md)。

构建/选择失败发生在推理前。执行后的精简或内容存储失败返回 ContextActionError::ResultView，携带完整已执行报告；不得因此重放工具。原工具错误仍可能包含未确认副作用，见[单步动作](action-step.md)。该组件只推进一次动作，不自动循环或判断业务已完成；自定义协议可使用底层选择、计数和视图接口自行装配。
