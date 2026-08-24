use std::collections::HashSet;

use cordis::Context;
use dsh_api::services::ManifestService;
use dsh_bundle::{BaseConfig, install_base, install_profile};
use dsh_session::SessionPersistence;
use dsh_types::{AgentOptions, SessionEventData};
use serde_json::json;

#[tokio::test]
async fn base_bundle_boots_and_runs_an_agent() {
    let ctx = Context::new();
    let handles = install_base(&ctx, BaseConfig::default()).await.unwrap();
    // manifest, llm, sessions, tools, system-prompt, agent-loop.
    assert_eq!(handles.len(), 6);

    // All service seams are live.
    assert!(ctx.get::<dsh_api::services::LlmService>("llm").is_some());
    assert!(ctx.get::<dsh_api::services::SessionService>("sessions").is_some());
    assert!(ctx.get::<dsh_api::services::ToolsService>("tools").is_some());
    assert!(ctx.get::<dsh_api::services::SystemPromptService>("systemPrompt").is_some());
    assert!(ctx.get::<dsh_api::services::AgentRegistryService>("agents").is_some());
    assert!(ctx.get::<ManifestService>("manifest").is_some());

    // A full turn runs against the mock provider.
    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .unwrap();
    agent.followup(dsh_types::Message::user("u-1", vec![dsh_types::ContentBlock::text("hello")]));
    agent.when_idle().await;
    let text = agent.session().derive_messages();
    let last = text.last().unwrap();
    assert_eq!(last.text(), "hello"); // mock echoes the last user message
    let ends = agent
        .session()
        .events()
        .iter()
        .filter(|e| matches!(e.data, SessionEventData::TurnEnd { .. }))
        .count();
    assert_eq!(ends, 1);
}

#[tokio::test]
async fn manifest_registry_declares_full_coverage() {
    let ctx = Context::new();
    install_base(&ctx, BaseConfig::default()).await.unwrap();

    let manifest = ctx.get::<ManifestService>("manifest").unwrap();
    let manifests = manifest.list();
    // manifest, llm, sessions, session-persistence, tools, system-prompt,
    // agent-loop.
    assert_eq!(manifests.len(), 7);

    // agent-loop declares its five service requirements; coverage holds.
    let agent_loop = manifests.iter().find(|m| m.name == "agent-loop").unwrap();
    assert_eq!(agent_loop.requires.len(), 5);
    let missing =
        dsh_api::manifest::ManifestRegistry::new().validate_coverage(&manifests);
    assert!(missing.is_empty(), "unsatisfied requirements: {missing:?}");

    // The declared provided services cover every seam the harness needs.
    let provided: HashSet<&str> = manifests
        .iter()
        .flat_map(|m| m.provides.iter().map(|s| s.name.as_str()))
        .collect();
    for service in [
        "llm",
        "llmStreams",
        "sessions",
        "sessionPersistence",
        "tools",
        "systemPrompt",
        "agents",
        "manifest",
    ] {
        assert!(provided.contains(service), "service {service} not declared");
    }

    // The tools manifest reflects the actually registered built-ins.
    let tools = manifests.iter().find(|m| m.name == "tools").unwrap();
    let tool_names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains(&"todo_write"), "todo_write must be in the tools manifest");
    assert!(tool_names.contains(&"bash"));
}

#[tokio::test]
async fn profile_install_stacks_base_bundle() {
    let ctx = Context::new();
    let profile = json!({
        "bundles": ["base"],
        "config": {}
    });
    let handles = install_profile(&ctx, &profile).await.unwrap();
    assert_eq!(handles.len(), 6);
    assert!(ctx.get::<dsh_api::services::LlmService>("llm").is_some());
}

#[tokio::test]
async fn unknown_bundle_is_rejected() {
    let ctx = Context::new();
    let profile = json!({ "bundles": ["nope"] });
    let err = match install_profile(&ctx, &profile).await {
        Ok(_) => panic!("expected an error"),
        Err(err) => err,
    };
    assert!(err.contains("unknown bundle"), "got: {err}");
}

#[tokio::test]
async fn bundle_with_persistence_records_sessions() {
    let dir = std::env::temp_dir().join(format!("dsh-bundle-test-{}", std::process::id()));
    let ctx = Context::new();
    install_base(
        &ctx,
        BaseConfig {
            store_dir: Some(dir.clone()),
            openai: None,
            default_provider: None,
            default_model: None,
        },
    )
    .await
    .unwrap();

    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(Some("persisted-session".into()), AgentOptions::mock("mock-1"), None, None)
        .unwrap();
    agent.followup(dsh_types::Message::user("u-1", vec![dsh_types::ContentBlock::text("hi")]));
    agent.when_idle().await;

    let store = ctx.require::<dsh_api::services::SessionService>("sessions").unwrap();
    store.flush(agent.id()).await.unwrap();
    assert!(dir.join("persisted-session.jsonl").exists());

    let backend = dsh_session::JsonlPersistence::new(dir.clone());
    let loaded = backend.load(&"persisted-session".to_string()).unwrap();
    assert!(!loaded.is_empty());
    assert!(loaded.iter().any(|e| matches!(e.data, SessionEventData::TurnEnd { .. })));
    std::fs::remove_dir_all(&dir).ok();
}
