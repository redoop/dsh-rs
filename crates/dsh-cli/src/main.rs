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

use cordis::{Context, Plugin};
use dsh_api::services::{AgentRegistryService, AgentView, LlmService};
use dsh_cli::{last_assistant_text, load_profile, parse_args, render_event, repair_crash_turns};
use dsh_types::AgentOptions;
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
        "plugin" => rt.block_on(cmd_plugin_load(&flags, &positionals)),
        "dump-config" | "manifests" => rt.block_on(cmd_dump_config(&flags)),
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
         \x20 dsh plugin load <path>                 dlopen a compiled plugin (.dylib/.so)\n\
         \x20                                        and register its declared tools\n\
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

    agent.followup(dsh_cli::user_message_with_text("u-1", prompt));
    agent.when_idle().await;
    flush_session(&ctx, &agent).await;

    if flags.contains_key("print-json") {
        println!("{}", serde_json::to_string_pretty(&agent.session().events()).unwrap());
    } else {
        println!("{}", last_assistant_text(agent.session().as_ref()));
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
async fn line_chat(ctx: &Context, agent: &Arc<dyn AgentView>) -> Result<(), String> {
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
        agent.followup(dsh_cli::user_message_with_text(format!("u-{seq}"), line));
        agent.when_idle().await;
        flush_session(ctx, agent).await;
        println!("{}", last_assistant_text(agent.session().as_ref()));
    }
    Ok(())
}

/// Load a stored session transcript. The JSONL backend is reached **through
/// the interface** (`ctx.sessionPersistence` as `Arc<dyn SessionPersistenceApi>`),
/// never through the concrete implementation crate.
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

    // Boot the base bundle with the persistence backend attached.
    let ctx = Context::new();
    let config = dsh_bundle::BaseConfig {
        store_dir: Some(std::path::PathBuf::from(&store_dir)),
        ..Default::default()
    };
    dsh_bundle::install_base(&ctx, config).await?;

    let backend = ctx
        .get::<Arc<dyn dsh_api::services::SessionPersistenceApi>>(
            dsh_api::SESSION_PERSISTENCE_SERVICE,
        )
        .ok_or_else(|| "persistence service missing — was a store dir configured?".to_string())?;

    let mut events = backend
        .load(&session_id)
        .map_err(|e| format!("cannot load {session_id}: {e}"))?;
    repair_crash_turns(&mut events);
    if events.is_empty() {
        println!("(no events for session {session_id})");
        return Ok(());
    }
    for event in &events {
        println!("{}", render_event(event));
    }
    Ok(())
}

async fn cmd_dump_config(
    flags: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let (ctx, _handles, _config) = boot(flags).await?;
    let manifest = ctx
        .get::<dsh_api::services::ManifestService>(dsh_api::MANIFEST_SERVICE)
        .ok_or_else(|| "manifest service missing — is the base bundle installed?".to_string())?;

    let manifests = manifest.list();
    let printable = manifests
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&printable).unwrap());

    let missing = dsh_api::manifest::ManifestRegistry::new().validate_coverage(&manifests);
    if missing.is_empty() {
        println!("dependency coverage: OK");
    } else {
        for m in &missing {
            eprintln!("missing: {m}");
        }
        return Err(format!("{} unsatisfied dependency(ies)", missing.len()));
    }
    Ok(())
}

async fn cmd_plugin_load(
    flags: &std::collections::HashMap<String, String>,
    positionals: &[String],
) -> Result<(), String> {
    if positionals.get(1).map(|s| s.as_str()) != Some("load") {
        return Err("usage: dsh plugin load <path-to-plugin>".to_string());
    }
    let path = positionals
        .get(2)
        .ok_or_else(|| "plugin load requires the path to a compiled .dylib/.so plugin".to_string())?;
    let (ctx, _handles, _config) = boot(flags).await?;

    let plugin = dsh_bundle::load_dynamic_plugin(std::path::Path::new(path))
        .map_err(|e| format!("cannot load plugin {path}: {e}"))?;
    let plugin_name = plugin.name().to_string();
    let declared_tools: Vec<String> = plugin.tools().iter().map(|t| t.name.clone()).collect();

    let config = flags
        .get("config")
        .and_then(|c| serde_json::from_str(c).ok())
        .unwrap_or(serde_json::Value::Null);
    let fiber = ctx.plugin(plugin, Some(config));
    fiber
        .join()
        .await
        .map_err(|e| format!("plugin {plugin_name} failed to activate: {e}"))?;
    println!("loaded plugin: {plugin_name} (state: {:?})", fiber.state());
    if !declared_tools.is_empty() {
        println!("declared tools: {}", declared_tools.join(", "));
    }
    let tools = ctx
        .get::<dsh_api::services::ToolsService>(dsh_api::TOOLS_SERVICE)
        .map(|registry| registry.list())
        .unwrap_or_default();
    println!("tools now registered: {}", tools.join(", "));
    Ok(())
}

async fn cmd_providers(flags: &std::collections::HashMap<String, String>) -> Result<(), String> {
    let (ctx, _handles, _config) = boot(flags).await?;
    let runtime = ctx
        .require::<dsh_api::services::LlmService>(dsh_api::LLM_SERVICE)
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
async fn flush_session(ctx: &Context, agent: &Arc<dyn AgentView>) {
    if let Some(store) = ctx.get::<dsh_api::services::SessionService>("sessions") {
        let _ = store.flush(agent.id()).await;
    }
}

async fn create_agent(
    ctx: &Context,
    flags: &std::collections::HashMap<String, String>,
    config: &dsh_bundle::BaseConfig,
) -> Result<Arc<dyn AgentView>, String> {
    // Provider detection goes through the llm SERVICE wrapper, never the
    // concrete runtime type.
    let runtime = ctx.get::<LlmService>(dsh_api::LLM_SERVICE);
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
        .get::<AgentRegistryService>(dsh_api::AGENTS_SERVICE)
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
