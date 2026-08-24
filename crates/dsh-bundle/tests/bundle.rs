use cordis::Context;
use dsh_bundle::{BaseConfig, install_base, install_profile};
use dsh_session::SessionPersistence;
use dsh_core::AgentOptions;
use serde_json::json;

#[tokio::test]
async fn base_bundle_boots_and_runs_an_agent() {
    let ctx = Context::new();
    let handles = install_base(&ctx, BaseConfig::default()).await.unwrap();
    assert_eq!(handles.len(), 5);

    // All service seams are live.
    assert!(ctx.get::<dsh_api::services::LlmService>("llm").is_some());
    assert!(ctx.get::<dsh_api::services::SessionService>("sessions").is_some());
    assert!(ctx.get::<dsh_api::services::ToolsService>("tools").is_some());
    assert!(ctx.get::<dsh_api::services::SystemPromptService>("systemPrompt").is_some());
    assert!(ctx.get::<dsh_api::services::AgentRegistryService>("agents").is_some());

    // A full turn runs against the mock provider.
    let agents = ctx.require::<dsh_api::services::AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .unwrap();
    agent.followup(dsh_core::agent::user_message_with_text("u-1", "hello"));
    agent.when_idle().await;
    let text = agent.session().derive_messages();
    let last = text.last().unwrap();
    assert_eq!(last.text(), "hello"); // mock echoes the last user message
    let ends = agent
        .session()
        .events()
        .iter()
        .filter(|e| matches!(e.data, dsh_session::SessionEventData::TurnEnd { .. }))
        .count();
    assert_eq!(ends, 1);
}

#[tokio::test]
async fn profile_install_stacks_base_bundle() {
    let ctx = Context::new();
    let profile = json!({
        "bundles": ["base"],
        "config": {}
    });
    let handles = install_profile(&ctx, &profile).await.unwrap();
    assert_eq!(handles.len(), 5);
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
    agent.followup(dsh_core::agent::user_message_with_text("u-1", "hi"));
    agent.when_idle().await;

    let store = ctx.require::<dsh_api::services::SessionService>("sessions").unwrap();
    store.flush(agent.id()).await.unwrap();
    assert!(dir.join("persisted-session.jsonl").exists());

    let backend = dsh_session::JsonlPersistence::new(dir.clone());
    let loaded = backend.load(&"persisted-session".to_string()).unwrap();
    assert!(!loaded.is_empty());
    assert!(loaded.iter().any(|e| matches!(e.data, dsh_session::SessionEventData::TurnEnd { .. })));
    std::fs::remove_dir_all(&dir).ok();
}
