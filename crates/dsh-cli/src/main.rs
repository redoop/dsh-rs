//! dsh — the headless dsh-rs runner.
//!
//! ```text
//! dsh run --prompt "list the files" [--provider mock|openai] [--model m]
//!         [--cwd DIR] [--store DIR] [--openai-base URL] [--openai-key KEY]
//!         [--profile FILE|JSON] [--max-tokens N]
//! dsh chat [same options]              interactive line loop
//! dsh transcript <session-id> [--store DIR]
//! dsh providers                        list registered provider routes
//! ```

use std::sync::Arc;

use cordis::Context;
use dsh_cli::{last_assistant_text, load_profile, parse_args};
use dsh_core::agent::user_message_with_text;
use dsh_core::{AgentOptions, AgentRegistry, AGENTS_SERVICE};
use serde_json::Value;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (flags, positionals) = parse_args(&args);
    let subcommand = if flags.contains_key("help") || flags.contains_key("h") {
        "help"
    } else {
        positionals.first().map(|s| s.as_str()).unwrap_or("run")
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let result = match subcommand {
        "run" => rt.block_on(cmd_run(&flags, &positionals)),
        "chat" => rt.block_on(cmd_chat(&flags, &positionals)),
        "transcript" => rt.block_on(cmd_transcript(&flags, &positionals)),
        "providers" => rt.block_on(cmd_providers(&flags)),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            Err("unknown command".to_string())
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "dsh — headless dsh-rs runner\n\
         \n\
         USAGE:\n\
         \x20 dsh run --prompt \"...\" [options]      run one prompt and print the answer\n\
         \x20 dsh chat [options]                     interactive TUI (line mode when piped)\n\
         \x20 dsh chat --line [options]              force the line-based loop\n\
         \x20 dsh transcript <session-id> [options]  print a stored session transcript\n\
         \x20 dsh providers                           list registered providers\n\
         \n\
         OPTIONS:\n\
         \x20 --provider NAME      provider route (mock | openai)\n\
         \x20 --model NAME         model id for the provider\n\
         \x20 --cwd DIR            working directory for tools\n\
         \x20 --store DIR          session persistence directory\n\
         \x20 --profile FILE       JSON profile ({{bundles, config}})\n\
         \x20 --openai-base URL    OpenAI-compatible base URL\n\
         \x20 --openai-key KEY     API key\n\
         \x20 --max-tokens N       per-request output cap\n\
         \x20 --print-json         print the full session log as JSON\n\
         \n\
         TUI KEYS (dsh chat):\n\
         \x20 enter: send    ↑/↓: history    pgup/pgdn: scroll\n\
         \x20 ctrl-u: clear line    ctrl-c / ctrl-q: quit\n\
         \x20 (IME-safe: a bare 'q' is a normal character and never quits)"
    );
}

fn flag(flags: &std::collections::HashMap<String, String>, key: &str) -> Option<String> {
    flags.get(key).cloned().filter(|v| !v.is_empty())
}

async fn cmd_run(
    flags: &std::collections::HashMap<String, String>,
    positionals: &[String],
) -> Result<(), String> {
    let prompt = flag(flags, "prompt")
        .or_else(|| positionals.get(1).cloned())
        .ok_or_else(|| "run requires --prompt".to_string())?;
    let (ctx, _handles, config) = boot(flags).await?;
    let agent = create_agent(&ctx, flags, &config).await?;

    agent.followup(user_message_with_text("u-1", prompt));
    agent.when_idle().await;
    flush_session(&ctx, &agent).await;

    if flags.contains_key("print-json") {
        println!("{}", serde_json::to_string_pretty(&agent.session.events()).unwrap());
    } else {
        println!("{}", last_assistant_text(&agent.session));
    }
    let _ = ctx;
    Ok(())
}

async fn cmd_chat(
    flags: &std::collections::HashMap<String, String>,
    _positionals: &[String],
) -> Result<(), String> {
    let (ctx, _handles, config) = boot(flags).await?;
    let agent = create_agent(&ctx, flags, &config).await?;

    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if interactive && !flags.contains_key("line") {
        // Full TUI on a real terminal; restore the terminal on exit.
        dsh_cli::tui::run_chat(&ctx, &agent).await?;
        // Flush the session once the TUI closes.
        flush_session(&ctx, &agent).await;
        Ok(())
    } else {
        // Line mode: piped input or explicit --line.
        line_chat(&ctx, &agent).await
    }
}

/// Simple line-based chat loop for piped input / non-TTY use.
async fn line_chat(ctx: &Context, agent: &Arc<dsh_core::Agent>) -> Result<(), String> {
    println!("dsh chat — type a line, Ctrl-D to exit");
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let mut seq = 0u64;
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        seq += 1;
        agent.followup(user_message_with_text(format!("u-{seq}"), line));
        agent.when_idle().await;
        flush_session(ctx, agent).await;
        println!("{}", last_assistant_text(&agent.session));
    }
    Ok(())
}

async fn cmd_transcript(
    flags: &std::collections::HashMap<String, String>,
    positionals: &[String],
) -> Result<(), String> {
    let session_id = positionals
        .get(1)
        .cloned()
        .ok_or_else(|| "transcript requires a session id".to_string())?;
    let store_dir = flag(flags, "store")
        .ok_or_else(|| "transcript requires --store DIR".to_string())?;
    let backend = Arc::new(dsh_session::JsonlPersistence::new(store_dir));
    let events = dsh_session::load_with_repair(backend.as_ref(), &session_id)
        .map_err(|e| format!("cannot load {session_id}: {e}"))?;
    if events.is_empty() {
        println!("(no events for session {session_id})");
        return Ok(());
    }
    for event in &events {
        println!("{}", dsh_session::Session::render_event(event));
    }
    Ok(())
}

async fn cmd_providers(flags: &std::collections::HashMap<String, String>) -> Result<(), String> {
    let (ctx, _handles, _config) = boot(flags).await?;
    let runtime = ctx
        .require::<dsh_llm::LlmRuntime>("llm")
        .map_err(|e| e.to_string())?;
    for provider in runtime.list_providers() {
        println!("{}\t({})", provider.id, provider.name);
    }
    Ok(())
}

/// Boot the base bundle (optionally from a profile) on a fresh context.
///
/// Configuration precedence: `--profile` > discovered default config
/// (`$DSH_CONFIG`, `~/.dsh/config.json`, `./dsh.json`) + CLI flag overrides.
async fn boot(
    flags: &std::collections::HashMap<String, String>,
) -> Result<(Context, Vec<cordis::FiberHandle>, dsh_bundle::BaseConfig), String> {
    let ctx = Context::new();
    let (handles, config) = if let Some(profile) = flag(flags, "profile") {
        let profile = load_profile(&profile)?;
        let handles = dsh_bundle::install_profile(&ctx, &profile).await?;
        let config = profile
            .get("config")
            .map(dsh_bundle::BaseConfig::from_value)
            .unwrap_or_default();
        (handles, config)
    } else {
        let mut config = discover_default_config();
        if let Some(dir) = flag(flags, "store") {
            config.store_dir = Some(std::path::PathBuf::from(dir));
        }
        if flag(flags, "openai-base").is_some() || flag(flags, "openai-key").is_some() {
            let mut section = serde_json::Map::new();
            if let Some(base) = flag(flags, "openai-base") {
                section.insert("base_url".to_string(), Value::String(base));
            }
            if let Some(key) = flag(flags, "openai-key") {
                section.insert("api_key".to_string(), Value::String(key));
            }
            if let Some(model) = flag(flags, "model") {
                section.insert("model".to_string(), Value::String(model));
            }
            config.openai = Some(Value::Object(section));
        }
        let handles = dsh_bundle::install_base(&ctx, config.clone()).await?;
        (handles, config)
    };
    Ok((ctx, handles, config))
}

/// Find and parse the default configuration file, if any.
fn discover_default_config() -> dsh_bundle::BaseConfig {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("DSH_CONFIG") {
        candidates.push(std::path::PathBuf::from(path));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(std::path::Path::new(&home).join(".dsh").join("config.json"));
    }
    candidates.push(std::path::PathBuf::from("dsh.json"));

    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => {
                eprintln!("[config] loaded {}", path.display());
                return dsh_bundle::BaseConfig::from_value(&value);
            }
            Err(err) => {
                eprintln!("[config] ignoring {}: {err}", path.display());
            }
        }
    }
    dsh_bundle::BaseConfig::default()
}

/// Flush the agent's session to any attached persistence backend.
async fn flush_session(ctx: &Context, agent: &Arc<dsh_core::Agent>) {
    if let Some(store) = ctx.get::<dsh_session::SessionStore>("sessions") {
        let _ = store.flush(&agent.session).await;
    }
}

async fn create_agent(
    ctx: &Context,
    flags: &std::collections::HashMap<String, String>,
    config: &dsh_bundle::BaseConfig,
) -> Result<Arc<dsh_core::Agent>, String> {
    let runtime = ctx.get::<dsh_llm::LlmRuntime>("llm");
    let has_deepseek = runtime.is_some_and(|r| r.has_provider("deepseek"));

    let provider = flag(flags, "provider")
        .or_else(|| config.default_provider.clone())
        .unwrap_or_else(|| {
            if has_deepseek {
                "deepseek".to_string()
            } else {
                "mock".to_string()
            }
        });
    let model = flag(flags, "model")
        .or_else(|| config.default_model.clone())
        .unwrap_or_else(|| {
            if provider == "mock" {
                "mock-1".to_string()
            } else {
                "deepseek-chat".to_string()
            }
        });
    let max_tokens = flag(flags, "max-tokens").and_then(|v| v.parse::<u32>().ok());
    let cwd = flag(flags, "cwd");
    let agents = ctx
        .get::<AgentRegistry>(AGENTS_SERVICE)
        .ok_or_else(|| "agents service missing — is the base bundle installed?".to_string())?;
    let agent = agents
        .create(
            None,
            AgentOptions {
                provider,
                model,
                max_tokens,
            },
            cwd,
            None,
        )
        .map_err(|e| e.to_string())?;
    Ok(agent)
}
