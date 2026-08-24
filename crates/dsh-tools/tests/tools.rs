use std::sync::Arc;

use dsh_llm::CancelToken;
use dsh_tools::{
    ToolExecutionResult, ToolRegistry, ToolRunContext, tools_plugin,
};
use serde_json::{json, Value};

fn run_ctx(cwd: &str) -> ToolRunContext {
    ToolRunContext {
        ctx: cordis::Context::new(),
        signal: CancelToken::new(),
        agent_id: Some("agent-1".into()),
        cwd: Some(cwd.to_string()),
    }
}

fn make_registry() -> (cordis::Context, ToolRegistry) {
    let ctx = cordis::Context::new();
    let registry = ToolRegistry::new(ctx.clone());
    dsh_tools::builtin::register_builtin_tools(&registry).unwrap();
    (ctx, registry)
}

#[tokio::test]
async fn lists_builtin_tools() {
    let (_, registry) = make_registry();
    let names = registry.list();
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"read_file".to_string()));
    assert!(names.contains(&"write_file".to_string()));
    assert!(names.contains(&"edit_file".to_string()));
    assert!(names.contains(&"glob".to_string()));
    assert!(names.contains(&"grep".to_string()));
    assert_eq!(registry.schemas().len(), 6);
}

#[tokio::test]
async fn unknown_tool_is_an_error() {
    let (_, registry) = make_registry();
    let result = registry
        .execute(
            "call-1".into(),
            "nope".into(),
            json!({}),
            run_ctx("/tmp"),
        )
        .await;
    assert!(result.is_error());
    match result {
        ToolExecutionResult::Error { code, .. } => assert_eq!(code, "UNKNOWN_TOOL"),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn bash_echoes() {
    let (_, registry) = make_registry();
    let result = registry
        .execute(
            "call-1".into(),
            "bash".into(),
            json!({ "command": "echo hello-dsh" }),
            run_ctx("/tmp"),
        )
        .await;
    match result {
        ToolExecutionResult::Success { value, .. } => {
            assert_eq!(value["stdout"], "hello-dsh\n");
            assert_eq!(value["exit_code"], 0);
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_failure_reports_exit_code() {
    let (_, registry) = make_registry();
    let result = registry
        .execute(
            "call-1".into(),
            "bash".into(),
            json!({ "command": "exit 3" }),
            run_ctx("/tmp"),
        )
        .await;
    assert!(result.is_error());
}

#[tokio::test]
async fn read_write_edit_roundtrip() {
    let dir = std::env::temp_dir().join(format!("dsh-tools-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (_, registry) = make_registry();

    let write = registry
        .execute(
            "call-1".into(),
            "write_file".into(),
            json!({ "path": "hello.txt", "content": "one\ntwo\nthree" }),
            run_ctx(dir.to_str().unwrap()),
        )
        .await;
    assert!(!write.is_error(), "write failed: {write:?}");

    let read = registry
        .execute(
            "call-2".into(),
            "read_file".into(),
            json!({ "path": "hello.txt" }),
            run_ctx(dir.to_str().unwrap()),
        )
        .await;
    match read {
        ToolExecutionResult::Success { value, .. } => {
            assert_eq!(value["content"], "one\ntwo\nthree");
        }
        other => panic!("expected success, got {other:?}"),
    }

    let edit = registry
        .execute(
            "call-3".into(),
            "edit_file".into(),
            json!({ "path": "hello.txt", "old_string": "two", "new_string": "2" }),
            run_ctx(dir.to_str().unwrap()),
        )
        .await;
    assert!(!edit.is_error(), "edit failed: {edit:?}");

    let read2 = registry
        .execute(
            "call-4".into(),
            "read_file".into(),
            json!({ "path": "hello.txt" }),
            run_ctx(dir.to_str().unwrap()),
        )
        .await;
    match read2 {
        ToolExecutionResult::Success { value, .. } => {
            assert_eq!(value["content"], "one\n2\nthree");
        }
        other => panic!("expected success, got {other:?}"),
    }

    // Missing old_string fails cleanly.
    let bad = registry
        .execute(
            "call-5".into(),
            "edit_file".into(),
            json!({ "path": "hello.txt", "old_string": "absent", "new_string": "x" }),
            run_ctx(dir.to_str().unwrap()),
        )
        .await;
    match bad {
        ToolExecutionResult::Error { code, .. } => assert_eq!(code, "NOT_FOUND"),
        other => panic!("expected error, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn glob_finds_files() {
    let dir = std::env::temp_dir().join(format!("dsh-glob-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src/sub")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.join("src/sub/deep.rs"), "fn deep() {}").unwrap();
    std::fs::write(dir.join("README.md"), "# readme").unwrap();

    let (_, registry) = make_registry();
    let shallow = registry
        .execute(
            "call-1".into(),
            "glob".into(),
            json!({ "pattern": "src/*.rs", "path": dir.to_str().unwrap() }),
            run_ctx("/tmp"),
        )
        .await;
    match &shallow {
        ToolExecutionResult::Success { value, .. } => {
            let matches = value["matches"].as_array().unwrap();
            assert_eq!(matches.len(), 1);
            assert_eq!(matches[0], "src/lib.rs");
        }
        other => panic!("expected success, got {other:?}"),
    }

    let deep = registry
        .execute(
            "call-2".into(),
            "glob".into(),
            json!({ "pattern": "**/*.rs", "path": dir.to_str().unwrap() }),
            run_ctx("/tmp"),
        )
        .await;
    match deep {
        ToolExecutionResult::Success { value, .. } => {
            assert_eq!(value["count"], 2);
        }
        other => panic!("expected success, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn grep_finds_matches_with_line_numbers() {
    let dir = std::env::temp_dir().join(format!("dsh-grep-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.txt"),
        "alpha\nbeta\ngamma\n",
    )
    .unwrap();
    std::fs::write(dir.join("b.txt"), "delta\n").unwrap();

    let (_, registry) = make_registry();
    let result = registry
        .execute(
            "call-1".into(),
            "grep".into(),
            json!({ "pattern": "a$", "path": dir.to_str().unwrap() }),
            run_ctx("/tmp"),
        )
        .await;
    match result {
        ToolExecutionResult::Success { value, .. } => {
            let matches = value.as_array().unwrap();
            // "alpha", "beta", "gamma" (a.txt) and "delta" (b.txt).
            assert_eq!(matches.len(), 4);
            let first = &matches[0];
            assert_eq!(first["line"], 1);
            assert_eq!(first["text"], "alpha");
            assert_eq!(matches[1]["text"], "beta");
            assert_eq!(matches[2]["text"], "gamma");
            assert_eq!(matches[3]["text"], "delta");
        }
        other => panic!("expected success, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn pre_execute_waterfall_can_deny() {
    let ctx = cordis::Context::new();
    let registry = ToolRegistry::new(ctx.clone());
    dsh_tools::builtin::register_builtin_tools(&registry).unwrap();
    ctx.on("tools/pre-execute", |_ctx, payload, _next| {
        Box::pin(async move {
            let name = payload.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name == "bash" {
                Ok(json!({ "kind": "deny", "reason": "bash is disabled in this scope" }))
            } else {
                Ok(json!({ "kind": "allow" }))
            }
        })
    })
    .await
    .unwrap();

    let denied = registry
        .execute("call-1".into(), "bash".into(), json!({ "command": "ls" }), run_ctx("/tmp"))
        .await;
    match denied {
        ToolExecutionResult::Error { code, message, .. } => {
            assert_eq!(code, "DENIED");
            assert!(message.contains("bash is disabled"));
        }
        other => panic!("expected denial, got {other:?}"),
    }

    let allowed = registry
        .execute("call-2".into(), "glob".into(), json!({ "pattern": "*.rs" }), run_ctx("/tmp"))
        .await;
    assert!(!allowed.is_error());
}

#[tokio::test]
async fn guard_denies_after_waterfall() {
    let ctx = cordis::Context::new();
    let registry = ToolRegistry::new(ctx.clone());
    dsh_tools::builtin::register_builtin_tools(&registry).unwrap();
    registry.add_guard(Arc::new(|args: &dsh_tools::ToolCallArgs| {
        if args.name == "write_file" {
            Some("writes are read-only in this scope".to_string())
        } else {
            None
        }
    }));

    let result = registry
        .execute(
            "call-1".into(),
            "write_file".into(),
            json!({ "path": "/tmp/x", "content": "nope" }),
            run_ctx("/tmp"),
        )
        .await;
    match result {
        ToolExecutionResult::Error { code, .. } => assert_eq!(code, "DENIED"),
        other => panic!("expected denial, got {other:?}"),
    }
}

#[tokio::test]
async fn tools_plugin_provides_registry() {
    let ctx = cordis::Context::new();
    let handle = ctx.plugin(tools_plugin(), Some(Value::Null));
    handle.join().await.unwrap();
    let registry = ctx.require::<ToolRegistry>("tools").unwrap();
    assert_eq!(registry.list().len(), 6);
}

#[tokio::test]
async fn bash_cancellation_returns_cancelled() {
    let (_, registry) = make_registry();
    let token = CancelToken::new();
    let token2 = token.clone();
    let handle = tokio::spawn(async move {
        let run_ctx = ToolRunContext {
            ctx: cordis::Context::new(),
            signal: token2,
            agent_id: None,
            cwd: Some("/tmp".to_string()),
        };
        registry
            .execute("call-1".into(), "bash".into(), json!({ "command": "sleep 30" }), run_ctx)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    token.cancel();
    let result = handle.await.unwrap();
    match result {
        ToolExecutionResult::Error { code, .. } => assert_eq!(code, "CANCELLED"),
        other => panic!("expected cancellation, got {other:?}"),
    }
}
