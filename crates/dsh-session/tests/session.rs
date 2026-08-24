use std::sync::Arc;

use dsh_llm::{ContentBlock, MessageSource};
use dsh_session::{
    CreateSessionOptions, JsonlPersistence, Session, SessionEventData, SessionPersistence,
    SessionStore, TurnEndReason, jsonl_persistence_plugin, load_with_repair, repair_crash_turns,
    session_plugin, user_message,
};

fn seed_session(store: &SessionStore, id: &str) -> Arc<Session> {
    store.create(CreateSessionOptions {
        id: Some(id.to_string()),
        cwd: Some("/tmp".to_string()),
        ..Default::default()
    })
}

#[test]
fn append_assigns_contiguous_seq_and_derives_messages() {
    let ctx = cordis::Context::new();
    let store = SessionStore::new(ctx);
    let session = seed_session(&store, "s1");

    session.append(SessionEventData::TurnStart { turn: 1 });
    let user = session.append(SessionEventData::UserMessage {
        message: user_message("u-1", "hello"),
    });
    session.append(SessionEventData::AssistantMessage {
        turn: 1,
        step: 1,
        message: dsh_llm::Message {
            id: "a-1".into(),
            role: dsh_llm::Role::Assistant,
            content: vec![ContentBlock::text("hi")],
            source: MessageSource::Model {
                provider: "mock".into(),
                model: "mock-1".into(),
            },
        },
        usage: None,
        interrupted: None,
    });
    session.append(SessionEventData::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    });

    assert_eq!(user.seq, 1);
    assert_eq!(session.seq(), 4);
    let events = session.events();
    assert_eq!(events.len(), 4);
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64, "seq must equal log position");
    }

    let derived = session.derive_messages();
    assert_eq!(derived.len(), 2);
    assert_eq!(derived[0].text(), "hello");
    assert_eq!(derived[1].text(), "hi");
    assert_eq!(session.open_turn(), None);
}

#[test]
fn empty_assistant_message_is_skipped_in_derivation() {
    let ctx = cordis::Context::new();
    let store = SessionStore::new(ctx);
    let session = seed_session(&store, "s2");
    session.append(SessionEventData::AssistantMessage {
        turn: 1,
        step: 1,
        message: dsh_llm::Message {
            id: "a-1".into(),
            role: dsh_llm::Role::Assistant,
            content: vec![],
            source: MessageSource::Model {
                provider: "mock".into(),
                model: "mock-1".into(),
            },
        },
        usage: None,
        interrupted: None,
    });
    assert!(session.derive_messages().is_empty());
}

#[test]
fn request_header_folds_latest_snapshot() {
    let ctx = cordis::Context::new();
    let store = SessionStore::new(ctx);
    let session = seed_session(&store, "s3");
    let header = dsh_session::EpochHeader {
        config: dsh_llm::LlmCallConfig {
            provider: "mock".into(),
            model: "mock-1".into(),
            ..Default::default()
        },
        system: Some("you are helpful".into()),
        tools: None,
    };
    session.append(SessionEventData::RequestHeader { header: header.clone() });
    assert_eq!(session.request_header(), Some(header));
}

#[test]
fn fork_from_stable_prefix_and_rejects_open_turn() {
    let ctx = cordis::Context::new();
    let store = SessionStore::new(ctx);
    let source = seed_session(&store, "src");
    source.append(SessionEventData::TurnStart { turn: 1 });
    source.append(SessionEventData::UserMessage {
        message: user_message("u-1", "parent prompt"),
    });
    source.append(SessionEventData::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    });

    // Forking an open turn must fail.
    let open = seed_session(&store, "open");
    open.append(SessionEventData::TurnStart { turn: 1 });
    assert!(store.fork(&open, None, None).is_err());

    let child = store.fork(&source, None, Some("child".to_string())).unwrap();
    assert_eq!(child.header.parent_session.as_deref(), Some("src"));
    assert_eq!(child.header.seed_length, 3);
    // The child continues from the parent's last seq and marks the seed end.
    let derived = child.derive_messages();
    assert_eq!(derived.len(), 1);
    assert_eq!(derived[0].text(), "parent prompt");
    let child_events = child.events();
    let last = child_events.last().unwrap();
    assert!(matches!(last.data, SessionEventData::SessionEndSeed));
}

#[tokio::test]
async fn jsonl_persistence_roundtrip_and_crash_repair() {
    let dir = dsh_session::persistence::temp_dir("roundtrip");
    let backend = Arc::new(JsonlPersistence::new(dir.clone()));

    let ctx = cordis::Context::new();
    let store = SessionStore::new(ctx);
    store.attach_persistence(backend.clone());
    let session = seed_session(&store, "p1");
    session.append(SessionEventData::TurnStart { turn: 1 });
    session.append(SessionEventData::UserMessage {
        message: user_message("u-1", "persist me"),
    });
    // No turn/end: simulates a crash mid-turn.
    store.flush(&session).await.unwrap();

    // A fresh backend reads the log back and repairs the orphaned turn.
    let fresh = Arc::new(JsonlPersistence::new(dir.clone()));
    let mut loaded = fresh.load(&"p1".to_string()).unwrap();
    assert_eq!(loaded.len(), 2);
    assert!(repair_crash_turns(&mut loaded));
    assert_eq!(loaded.len(), 3);
    let end = loaded.last().unwrap();
    match &end.data {
        SessionEventData::TurnEnd { turn, reason } => {
            assert_eq!(*turn, 1);
            assert_eq!(*reason, TurnEndReason::Interrupted);
        }
        other => panic!("expected turn/end, got {other:?}"),
    }
    // Reapplying repair is idempotent.
    assert!(!repair_crash_turns(&mut loaded));

    assert_eq!(fresh.list(), vec!["p1".to_string()]);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn load_with_repair_helper() {
    let dir = dsh_session::persistence::temp_dir("repair");
    let backend = Arc::new(JsonlPersistence::new(dir.clone()));
    let ctx = cordis::Context::new();
    let store = SessionStore::new(ctx);
    store.attach_persistence(backend.clone());
    let session = seed_session(&store, "p2");
    session.append(SessionEventData::TurnStart { turn: 5 });
    store.flush(&session).await.unwrap();

    let events = load_with_repair(backend.as_ref(), &"p2".to_string()).unwrap();
    assert_eq!(events.len(), 2);
    match &events[1].data {
        SessionEventData::TurnEnd { turn, reason } => {
            assert_eq!(*turn, 5);
            assert_eq!(*reason, TurnEndReason::Interrupted);
        }
        other => panic!("expected turn/end, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn session_plugin_provides_store() {
    let ctx = cordis::Context::new();
    let handle = ctx.plugin(session_plugin(), None);
    handle.join().await.unwrap();
    let store = ctx.require::<dsh_api::services::SessionService>("sessions").unwrap();
    let session = store.create(CreateSessionOptions {
        id: Some("plugged".into()),
        ..Default::default()
    });
    assert_eq!(store.get("plugged").unwrap().id(), session.id());
    assert_eq!(store.list().len(), 1);
}

#[tokio::test]
async fn jsonl_plugin_attaches_to_store() {
    let dir = dsh_session::persistence::temp_dir("plugin");
    let ctx = cordis::Context::new();
    let sessions = ctx.plugin(session_plugin(), None);
    let pers = ctx.plugin(jsonl_persistence_plugin(dir.clone()), None);
    sessions.join().await.unwrap();
    pers.join().await.unwrap();

    let store = ctx.require::<dsh_api::services::SessionService>("sessions").unwrap();
    let session = store.create(CreateSessionOptions {
        id: Some("plugged-persist".into()),
        ..Default::default()
    });
    session.append(SessionEventData::UserMessage {
        message: user_message("u-1", "through the plugin"),
    });
    store.flush(session.id()).await.unwrap();

    let handle = ctx.require::<dsh_session::persistence::PersistenceService>("sessionPersistence").unwrap();
    let loaded = handle.backend.load(&"plugged-persist".to_string()).unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(dir.join("plugged-persist.jsonl").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn session_renders_transcript() {
    let ctx = cordis::Context::new();
    let store = SessionStore::new(ctx);
    let session = seed_session(&store, "s9");
    session.append(SessionEventData::UserMessage {
        message: user_message("u-1", "list files"),
    });
    let rendered = session.render();
    assert!(rendered.contains("[user] list files"));
}
