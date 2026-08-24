use std::sync::Arc;

use cordis::plugin::BoxFuture;
use cordis::Context;
use dsh_core::{AgentOptions, PromptSection, SystemPromptService, agent_loop_plugin, prompt::system_prompt_plugin};
use dsh_core::agent::user_message_with_text;
use dsh_llm::{ContentBlock, GenerateOptions, LlmAdapter, LlmError, StreamChunk, llm_plugin, runtime::stream_from_chunks};
use dsh_session::{SessionEventData, TurnEndReason, session_plugin};
use dsh_tools::tools_plugin;
use serde_json::{json, Value};

/// Compose the minimal harness plugin stack and wait for convergence.
async fn compose(ctx: &Context) -> Vec<cordis::FiberHandle> {
    let llm = ctx.plugin(llm_plugin(), Some(json!({})));
    let sessions = ctx.plugin(session_plugin(), None);
    let tools = ctx.plugin(tools_plugin(), Some(Value::Null));
    let prompt = ctx.plugin(system_prompt_plugin(), None);
    let agent_loop = ctx.plugin(agent_loop_plugin(), None);
    llm.join().await.unwrap();
    sessions.join().await.unwrap();
    tools.join().await.unwrap();
    prompt.join().await.unwrap();
    agent_loop.join().await.unwrap();
    vec![llm, sessions, tools, prompt, agent_loop]
}

#[tokio::test]
async fn prompt_assemble_orders_and_interpolates() {
    let ctx = Context::new();
    let service = SystemPromptService::new(ctx.clone());
    service.section(PromptSection::new("persona", 0, "You are {{name}}, a helpful agent."));
    service.section(PromptSection::new("harness", -100, "dsh-rs harness identity"));
    service.section(PromptSection::new("tools", 100, "You have tools."));
    service.variable("name", || Some("rover".to_string()));
    let assembly = service.assemble();
    assert!(assembly.system.starts_with("dsh-rs harness identity"));
    assert!(assembly.system.contains("You are rover, a helpful agent."));
    assert!(assembly.system.ends_with("You have tools."));
}

#[tokio::test]
async fn prompt_complete_section_wins() {
    let ctx = Context::new();
    let service = SystemPromptService::new(ctx.clone());
    service.section(PromptSection::new("persona", 0, "normal persona"));
    service.section(PromptSection::new("complete", -50, "ONLY THIS MATTERS").complete());
    let assembly = service.assemble();
    assert_eq!(assembly.system, "ONLY THIS MATTERS");
}

#[tokio::test]
async fn agent_loop_runs_tool_then_finishes() {
    let ctx = Context::new();
    compose(&ctx).await;

    // Script the mock adapter: first a bash tool call, then the final answer.
    let runtime = ctx.require::<dsh_api::services::LlmService>("llm").unwrap();
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(dsh_llm::adapters::mock::MockAdapter::scripted(vec![
                dsh_llm::adapters::mock::MockAdapter::tool_call_response(
                    "call-1",
                    "bash",
                    json!({ "command": "echo hello-dsh" }),
                ),
                dsh_llm::adapters::mock::MockAdapter::text_response("task complete"),
            ])),
        )
        .unwrap();

    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .unwrap();

    agent.followup(user_message_with_text("u-1", "list the files please"));
    agent.when_idle().await;

    let events = agent.session().events();
    let types: Vec<&str> = events.iter().map(|e| e.event_type()).collect();
    assert!(types.contains(&"turn/start"));
    assert!(types.contains(&"user/message"));
    assert!(types.contains(&"assistant/chunk"));
    assert!(types.contains(&"tool/call"));
    assert!(types.contains(&"tool/result"));
    assert!(types.contains(&"assistant/message"));
    assert!(types.contains(&"turn/end"));

    // The turn completed cleanly.
    let ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.data, SessionEventData::TurnEnd { .. }))
        .collect();
    assert_eq!(ends.len(), 1);
    match &ends[0].data {
        SessionEventData::TurnEnd { reason, .. } => {
            assert_eq!(*reason, TurnEndReason::Completed);
        }
        _ => unreachable!(),
    }

    // The final assistant message carries the model's answer.
    let messages = agent.session().derive_messages();
    let last = messages.last().unwrap();
    assert_eq!(last.text(), "task complete");

    // The bash tool result is in the log, with its output nested inside the
    // tool-result content block.
    let has_bash_result = events.iter().any(|e| {
        matches!(
            &e.data,
            SessionEventData::ToolResult { message, .. }
                if message_tool_result_text(message).contains("hello-dsh")
        )
    });
    assert!(has_bash_result);

    // Request headers were logged for the steps.
    let headers = events
        .iter()
        .filter(|e| matches!(e.data, SessionEventData::RequestHeader { .. }))
        .count();
    assert!(headers >= 2, "one header per model request, got {headers}");
}

#[tokio::test]
async fn agent_loop_uses_system_prompt_and_tool_schemas() {
    let ctx = Context::new();
    compose(&ctx).await;

    // Record what the model saw.
    let seen: Arc<std::sync::Mutex<Vec<GenerateOptions>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_for_adapter = seen.clone();
    let runtime = ctx.require::<dsh_api::services::LlmService>("llm").unwrap();
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(RecordingAdapter {
                inner: dsh_llm::adapters::mock::MockAdapter::new(),
                seen: seen_for_adapter,
            }),
        )
        .unwrap();

    // Add a prompt section and a variable.
    let prompt = ctx.require::<dsh_api::services::SystemPromptService>("systemPrompt").unwrap();
    prompt.section("persona", 0, "You are {{who}}, an agent.", false);
    prompt.variable("who", std::sync::Arc::new(|| Some("dsh-rs".to_string())));

    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .unwrap();
    agent.followup(user_message_with_text("u-1", "hello"));
    agent.when_idle().await;

    let requests = seen.lock().unwrap().clone();
    let first = requests.first().expect("at least one request");
    assert!(first.system.as_deref().unwrap().contains("You are dsh-rs, an agent."));
    let tool_names: Vec<&str> = first
        .tools
        .as_ref()
        .unwrap()
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert!(tool_names.contains(&"bash"));
    assert!(tool_names.contains(&"todo_write"));
}

/// An adapter that records every request it serves, then delegates to mock.
struct RecordingAdapter {
    inner: dsh_llm::adapters::mock::MockAdapter,
    seen: Arc<std::sync::Mutex<Vec<GenerateOptions>>>,
}

impl LlmAdapter for RecordingAdapter {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxFuture<Result<dsh_llm::BoxStream<StreamChunk>, LlmError>> {
        self.seen.lock().unwrap().push(options.clone());
        self.inner.stream(options)
    }
}

/// An adapter that never answers until its gate opens; used to test cancel.
#[derive(Clone)]
struct GateAdapter {
    gate: Arc<tokio::sync::Notify>,
}

impl LlmAdapter for GateAdapter {
    fn name(&self) -> &'static str {
        "gate"
    }

    fn stream(
        &self,
        _options: GenerateOptions,
    ) -> BoxFuture<Result<dsh_llm::BoxStream<StreamChunk>, LlmError>> {
        let gate = self.gate.clone();
        Box::pin(async move {
            gate.notified().await;
            Ok(stream_from_chunks(
                dsh_llm::adapters::mock::MockAdapter::text_response("late answer"),
            ))
        })
    }
}

#[tokio::test]
async fn cancel_aborts_the_turn() {
    let ctx = Context::new();
    compose(&ctx).await;

    let runtime = ctx.require::<dsh_api::services::LlmService>("llm").unwrap();
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(GateAdapter {
                gate: Arc::new(tokio::sync::Notify::new()),
            }),
        )
        .unwrap();

    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .unwrap();
    agent.followup(user_message_with_text("u-1", "do something slow"));
    // Wait until the driver is mid-request.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !agent.driver_busy() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(agent.driver_busy(), "driver should be running the request");
    agent.cancel(dsh_core::AgentCancelCause::User, false);
    agent.when_idle().await;

    let events = agent.session().events();
    let ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.data, SessionEventData::TurnEnd { .. }))
        .collect();
    assert_eq!(ends.len(), 1);
    match &ends[0].data {
        SessionEventData::TurnEnd { reason, .. } => {
            assert_eq!(*reason, TurnEndReason::Aborted { cause: "user".to_string() });
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn todo_tool_writes_session_event() {
    let ctx = Context::new();
    compose(&ctx).await;

    let runtime = ctx.require::<dsh_api::services::LlmService>("llm").unwrap();
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(dsh_llm::adapters::mock::MockAdapter::scripted(vec![
                dsh_llm::adapters::mock::MockAdapter::tool_call_response(
                    "call-1",
                    "todo_write",
                    json!({
                        "todos": [
                            { "content": "design", "status": "completed" },
                            { "content": "implement" }
                        ]
                    }),
                ),
                dsh_llm::adapters::mock::MockAdapter::text_response("todos recorded"),
            ])),
        )
        .unwrap();

    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .unwrap();
    agent.followup(user_message_with_text("u-1", "track my todos"));
    agent.when_idle().await;

    let events = agent.session().events();
    let todos: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.data {
            SessionEventData::TodoWrite { todos } => Some(todos.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].len(), 2);
    assert_eq!(todos[0][0].content, "design");
    assert_eq!(todos[0][0].status, dsh_session::TodoStatus::Completed);
}

#[tokio::test]
async fn agents_registry_tracks_live_agents() {
    let ctx = Context::new();
    compose(&ctx).await;
    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let a = agents
        .create(Some("agent-x".into()), AgentOptions::mock("mock-1"), None, None)
        .unwrap();
    assert_eq!(a.id(), "agent-x");
    assert_eq!(agents.get("agent-x").unwrap().id(), "agent-x");
    assert_eq!(agents.list().len(), 1);
    agents.dispose(&a);
    assert!(agents.get("agent-x").is_none());
}

#[tokio::test]
async fn seed_prompt_enters_the_log() {
    let ctx = Context::new();
    compose(&ctx).await;
    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(
            None,
            AgentOptions::mock("mock-1"),
            Some("/tmp".to_string()),
            Some("context seed".to_string()),
        )
        .unwrap();
    agent.followup(user_message_with_text("u-1", "go"));
    agent.when_idle().await;
    let events = agent.session().events();
    let users: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.data {
            SessionEventData::UserMessage { message } => Some(message.text()),
            _ => None,
        })
        .collect();
    assert!(users.iter().any(|t| t == "context seed"));
    assert!(users.iter().any(|t| t == "go"));
}


/// Extract text nested inside a message's tool-result blocks.
fn message_tool_result_text(message: &dsh_llm::Message) -> String {
    let mut out = String::new();
    for block in &message.content {
        if let ContentBlock::ToolResult { content, .. } = block {
            for inner in content {
                if let ContentBlock::Text { text } = inner {
                    out.push_str(text);
                }
            }
        }
    }
    out
}
