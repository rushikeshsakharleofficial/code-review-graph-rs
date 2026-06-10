use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crg_cli::{
    build::{full_build, incremental_update, BuildOptions},
    incremental::{ensure_schema_dir, find_project_root, get_db_path},
};
use crg_core::store::GraphStore;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "code-review-graph",
    version = env!("CARGO_PKG_VERSION"),
    author,
    about = "Persistent, incrementally-updated code knowledge graph for token-efficient code review"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a full knowledge graph from scratch.
    Build(BuildArgs),
    /// Incrementally update the graph for changed files.
    Update(UpdateArgs),
    /// Show graph statistics.
    Status(StatusArgs),
    /// Start the MCP server (JSON-RPC over stdio).
    Serve(ServeArgs),
    /// Alias for `serve` (MCP mode).
    Mcp(ServeArgs),
    /// Generate an interactive D3.js visualization.
    Visualize(VisualizeArgs),
    /// Generate a Markdown wiki from the community structure.
    Wiki(WikiArgs),
    /// Risk-scored analysis of recent changes.
    #[command(name = "detect-changes")]
    DetectChanges(DetectChangesArgs),
    /// Register a repository in the multi-repo registry.
    Register(RegisterArgs),
    /// Unregister a repository or alias.
    Unregister(UnregisterArgs),
    /// List all registered repositories.
    Repos,
    /// Run post-processing steps (FTS rebuild, flows, communities).
    Postprocess(PostprocessArgs),
    /// Generate MCP install configuration for various platforms.
    Install(InstallArgs),
    /// Manage the background file-watching daemon.
    Daemon(DaemonArgs),
    /// Run evaluation benchmarks.
    Eval(EvalArgs),
}

// ---------------------------------------------------------------------------
// Per-subcommand argument structs
// ---------------------------------------------------------------------------

#[derive(Args)]
struct BuildArgs {
    /// Force a full rebuild (re-parses all files; existing stale data is overwritten on conflict).
    #[arg(long)]
    full: bool,
    /// Git base ref for the incremental baseline (informational only for full build).
    #[arg(long, default_value = "HEAD~1")]
    base: String,
    /// Repository root (defaults to nearest project root from CWD).
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Postprocessing level: full, minimal, or none.
    #[arg(long, default_value = "full")]
    postprocess: String,
    /// Maximum Rayon thread count.
    #[arg(long)]
    threads: Option<usize>,
}

#[derive(Args)]
struct UpdateArgs {
    /// Git base ref to diff against.
    #[arg(long, default_value = "HEAD~1")]
    base: String,
    /// Repository root.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Args)]
struct StatusArgs {
    /// Repository root.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Args)]
struct ServeArgs {
    /// Repository root.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Serve over HTTP instead of stdio (not yet implemented).
    #[arg(long)]
    http: bool,
    /// HTTP listen address.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// HTTP listen port.
    #[arg(long, default_value_t = 3000)]
    port: u16,
}

#[derive(Args)]
struct VisualizeArgs {
    /// Output HTML file path.
    #[arg(long, short, default_value = "graph.html")]
    output: PathBuf,
    /// Repository root.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Args)]
struct WikiArgs {
    /// Output directory for generated Markdown files.
    #[arg(long, short, default_value = "wiki")]
    output: PathBuf,
    /// Repository root.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Args)]
struct DetectChangesArgs {
    /// Git base ref to diff against.
    #[arg(long, default_value = "HEAD~1")]
    base: String,
    /// Print a one-line summary instead of full report.
    #[arg(long)]
    brief: bool,
    /// Repository root.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Args)]
struct RegisterArgs {
    /// Path to the repository root to register.
    path: PathBuf,
    /// Optional short alias for the repository.
    #[arg(long)]
    alias: Option<String>,
}

#[derive(Args)]
struct UnregisterArgs {
    /// Path or alias to remove from the registry.
    path_or_alias: String,
}

#[derive(Args)]
struct PostprocessArgs {
    /// Skip flow detection.
    #[arg(long)]
    no_flows: bool,
    /// Skip community detection.
    #[arg(long)]
    no_communities: bool,
    /// Skip FTS index rebuild.
    #[arg(long)]
    no_fts: bool,
    /// Repository root.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Args)]
struct InstallArgs {
    /// Target platform: claude-code, cursor, windsurf, vscode, zed, continue, opencode.
    #[arg(long, default_value = "claude-code")]
    platform: String,
    /// Repository root to embed in the config.
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Args)]
struct DaemonArgs {
    /// Action: start, stop, status, restart.
    #[arg(default_value = "status")]
    action: String,
    /// Run in the foreground instead of daemonising.
    #[arg(long, hide = true)]
    daemon_foreground: bool,
}

#[derive(Args)]
struct EvalArgs {
    /// Repository root.
    #[arg(long)]
    repo: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialise tracing from RUST_LOG (default: info).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Build(args) => cmd_build(args),
        Commands::Update(args) => cmd_update(args),
        Commands::Status(args) => cmd_status(args),
        Commands::Serve(args) | Commands::Mcp(args) => cmd_serve(args),
        Commands::Visualize(args) => cmd_visualize(args),
        Commands::Wiki(args) => cmd_wiki(args),
        Commands::DetectChanges(args) => cmd_detect_changes(args),
        Commands::Register(args) => cmd_register(args),
        Commands::Unregister(args) => cmd_unregister(args),
        Commands::Repos => cmd_repos(),
        Commands::Postprocess(args) => cmd_postprocess(args),
        Commands::Install(args) => cmd_install(args),
        Commands::Daemon(args) => cmd_daemon(args),
        Commands::Eval(args) => cmd_eval(args),
    }
}

// ---------------------------------------------------------------------------
// Helpers shared across subcommands
// ---------------------------------------------------------------------------

/// Resolve the repo root from an explicit `--repo` flag or by walking upward
/// from the current directory.
fn resolve_repo_root(repo: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let root = match repo {
        Some(p) => p,
        None => find_project_root().context(
            "Could not find a project root (no .git or .code-review-graph directory). \
             Use --repo to specify one.",
        )?,
    };
    let root = root.canonicalize().with_context(|| {
        format!("Could not canonicalize repo path: {}", root.display())
    })?;
    crg_core::security::validate_repo_root(&root)?;
    Ok(root)
}

/// Open the graph store at the default db path for `repo_root`.
fn open_store(repo_root: &std::path::Path) -> anyhow::Result<GraphStore> {
    ensure_schema_dir(repo_root)?;
    let db_path = get_db_path(repo_root);
    GraphStore::new(&db_path)
        .with_context(|| format!("Failed to open database: {}", db_path.display()))
}

// ---------------------------------------------------------------------------
// `build`
// ---------------------------------------------------------------------------

fn cmd_build(args: BuildArgs) -> anyhow::Result<()> {
    let repo_root = resolve_repo_root(args.repo)?;
    let store = open_store(&repo_root)?;

    let opts = BuildOptions {
        full_rebuild: args.full,
        base: args.base,
        postprocess: args.postprocess,
        max_threads: args.threads,
        ..Default::default()
    };

    eprintln!("Building graph for {} ...", repo_root.display());
    let result = full_build(&repo_root, &store, &opts)?;

    eprintln!(
        "Done in {:.1}s — {} files parsed, {} skipped",
        result.duration_secs, result.files_parsed, result.files_skipped
    );
    println!(
        "Nodes: {}  Edges: {}",
        result.nodes_total, result.edges_total
    );
    if !result.errors.is_empty() {
        eprintln!("Warnings ({}):", result.errors.len());
        for e in &result.errors {
            eprintln!("  {e}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `update`
// ---------------------------------------------------------------------------

fn cmd_update(args: UpdateArgs) -> anyhow::Result<()> {
    let repo_root = resolve_repo_root(args.repo)?;
    let store = open_store(&repo_root)?;

    let opts = BuildOptions {
        base: args.base,
        ..Default::default()
    };

    eprintln!(
        "Incrementally updating graph for {} ...",
        repo_root.display()
    );
    let result = incremental_update(&repo_root, &store, &opts)?;

    eprintln!(
        "Done in {:.1}s — {} files updated",
        result.duration_secs, result.files_parsed
    );
    println!(
        "Nodes: {}  Edges: {}",
        result.nodes_total, result.edges_total
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `status`
// ---------------------------------------------------------------------------

fn cmd_status(args: StatusArgs) -> anyhow::Result<()> {
    let repo_root = resolve_repo_root(args.repo)?;
    let db_path = get_db_path(&repo_root);

    if !db_path.exists() {
        println!("No graph database found at {}", db_path.display());
        println!("Run `code-review-graph build` to create one.");
        return Ok(());
    }

    let store = open_store(&repo_root)?;
    let stats = store.get_stats()?;

    // Nodes summary
    let mut kinds: Vec<(&String, &i64)> = stats.nodes_by_kind.iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(a.1));
    let kind_summary: Vec<String> = kinds
        .iter()
        .map(|(k, v)| format!("{}: {}", k, fmt_count(**v)))
        .collect();

    println!(
        "Nodes: {} ({})",
        fmt_count(stats.total_nodes),
        kind_summary.join(", ")
    );

    // Edges summary
    let mut ekinds: Vec<(&String, &i64)> = stats.edges_by_kind.iter().collect();
    ekinds.sort_by(|a, b| b.1.cmp(a.1));
    let edge_summary: Vec<String> = ekinds
        .iter()
        .map(|(k, v)| format!("{}: {}", k, fmt_count(**v)))
        .collect();
    println!(
        "Edges: {} ({})",
        fmt_count(stats.total_edges),
        edge_summary.join(", ")
    );

    println!("Files: {}", fmt_count(stats.files_count));

    if !stats.languages.is_empty() {
        let mut langs = stats.languages.clone();
        langs.sort();
        println!("Languages: {}", langs.join(", "));
    }

    if let Some(ts) = &stats.last_updated {
        println!("Last updated: {ts}");
    }

    // DB size
    if let Ok(meta) = std::fs::metadata(&db_path) {
        println!(
            "DB: {} ({:.1} MB)",
            db_path.display(),
            meta.len() as f64 / 1_048_576.0
        );
    }

    Ok(())
}

fn fmt_count(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let chars: Vec<char> = s.chars().rev().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(*c);
    }
    out.chars().rev().collect()
}

// ---------------------------------------------------------------------------
// `serve` / `mcp`
// ---------------------------------------------------------------------------

fn cmd_serve(args: ServeArgs) -> anyhow::Result<()> {
    // Try to locate the crg-mcp-server binary next to the current executable.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let mcp_bin_name = if cfg!(windows) {
        "crg-mcp-server.exe"
    } else {
        "crg-mcp-server"
    };

    let mcp_bin = exe_dir
        .as_ref()
        .map(|d| d.join(mcp_bin_name))
        .filter(|p| p.exists());

    if let Some(bin) = mcp_bin {
        let mut cmd_args: Vec<String> = Vec::new();
        if let Some(repo) = &args.repo {
            // Validate repo before passing to the subprocess.
            let canonical = repo
                .canonicalize()
                .with_context(|| format!("Cannot canonicalize --repo: {}", repo.display()))?;
            crg_core::security::validate_repo_root(&canonical)?;
            cmd_args.push("--repo".to_string());
            cmd_args.push(canonical.to_string_lossy().to_string());
        }

        // SECURITY: never use shell=true.
        let status = std::process::Command::new(&bin)
            .args(&cmd_args)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;

        std::process::exit(status.code().unwrap_or(1));
    }

    eprintln!(
        "The crg-mcp-server binary was not found alongside this executable. \
         Build it with: cargo build -p crg-mcp --release"
    );
    if args.http {
        eprintln!(
            "HTTP mode (--http --host {} --port {}) is not yet implemented.",
            args.host, args.port
        );
    }
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// `visualize`
// ---------------------------------------------------------------------------

fn cmd_visualize(args: VisualizeArgs) -> anyhow::Result<()> {
    eprintln!(
        "Visualize is not yet implemented (crg-visualization crate is pending)."
    );
    eprintln!("Output would be written to: {}", args.output.display());
    if let Some(repo) = args.repo {
        eprintln!("Repository: {}", repo.display());
    }
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// `wiki`
// ---------------------------------------------------------------------------

fn cmd_wiki(args: WikiArgs) -> anyhow::Result<()> {
    eprintln!("Wiki generation is not yet implemented (crg-wiki crate is pending).");
    eprintln!("Output would be written to: {}", args.output.display());
    if let Some(repo) = args.repo {
        eprintln!("Repository: {}", repo.display());
    }
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// `detect-changes`
// ---------------------------------------------------------------------------

fn cmd_detect_changes(args: DetectChangesArgs) -> anyhow::Result<()> {
    eprintln!(
        "detect-changes is not yet implemented (crg-changes crate is pending)."
    );
    eprintln!("Base ref: {}", args.base);
    if args.brief {
        eprintln!("(brief mode)");
    }
    if let Some(repo) = args.repo {
        eprintln!("Repository: {}", repo.display());
    }
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// `register`
// ---------------------------------------------------------------------------

fn cmd_register(args: RegisterArgs) -> anyhow::Result<()> {
    let canonical = args
        .path
        .canonicalize()
        .with_context(|| format!("Cannot canonicalize path: {}", args.path.display()))?;
    crg_core::security::validate_repo_root(&canonical)?;

    let db_path = get_db_path(&canonical);
    let alias = args.alias.clone().unwrap_or_else(|| {
        canonical
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });

    let mut registry = load_registry()?;
    // Remove any existing entry for the same path.
    let canon_str = canonical.to_string_lossy().to_string();
    registry.repos.retain(|r| r.path != canon_str);
    registry.repos.push(RegistryEntry {
        path: canon_str,
        alias: alias.clone(),
        db_path: db_path.to_string_lossy().to_string(),
    });
    save_registry(&registry)?;

    println!("Registered '{}' as '{alias}'", canonical.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// `unregister`
// ---------------------------------------------------------------------------

fn cmd_unregister(args: UnregisterArgs) -> anyhow::Result<()> {
    let key = &args.path_or_alias;
    let mut registry = load_registry()?;
    let before = registry.repos.len();
    registry
        .repos
        .retain(|r| r.path.as_str() != key.as_str() && r.alias.as_str() != key.as_str());
    let removed = before - registry.repos.len();
    if removed == 0 {
        eprintln!("No entry found for '{key}'");
        std::process::exit(1);
    }
    save_registry(&registry)?;
    println!("Unregistered {removed} entry for '{key}'");
    Ok(())
}

// ---------------------------------------------------------------------------
// `repos`
// ---------------------------------------------------------------------------

fn cmd_repos() -> anyhow::Result<()> {
    let registry = load_registry()?;
    if registry.repos.is_empty() {
        println!("No repositories registered.");
        println!("Use `code-review-graph register <path>` to add one.");
        return Ok(());
    }
    println!("{:<20} {}", "ALIAS", "PATH");
    println!("{}", "-".repeat(60));
    for r in &registry.repos {
        println!("{:<20} {}", r.alias, r.path);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `postprocess`
// ---------------------------------------------------------------------------

fn cmd_postprocess(args: PostprocessArgs) -> anyhow::Result<()> {
    let repo_root = resolve_repo_root(args.repo)?;
    let store = open_store(&repo_root)?;

    if !args.no_fts {
        eprintln!("Rebuilding FTS index ...");
        store.rebuild_fts()?;
        eprintln!("FTS index rebuilt.");
    }

    if !args.no_flows {
        eprintln!(
            "Flow detection is not yet implemented (crg-flows crate is pending). Skipping."
        );
    }

    if !args.no_communities {
        eprintln!(
            "Community detection is not yet implemented (crg-communities crate is pending). Skipping."
        );
    }

    println!("Postprocessing complete.");
    Ok(())
}

// ---------------------------------------------------------------------------
// `install`
// ---------------------------------------------------------------------------

fn cmd_install(args: InstallArgs) -> anyhow::Result<()> {
    let exe_path = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("code-review-graph"));

    let repo_str = match args.repo {
        Some(ref p) => p
            .canonicalize()
            .map(|c| c.to_string_lossy().to_string())
            .unwrap_or_else(|_| p.to_string_lossy().to_string()),
        None => "/path/to/your/repo".to_string(),
    };

    match args.platform.as_str() {
        "claude-code" | "claude" => {
            let config = serde_json::json!({
                "mcpServers": {
                    "code-review-graph": {
                        "command": exe_path.to_string_lossy(),
                        "args": ["serve"],
                        "env": {
                            "CRG_REPO": repo_str
                        }
                    }
                }
            });
            println!("Add this to your Claude Code MCP config (~/.claude.json or .claude.json):");
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        "cursor" => {
            let config = serde_json::json!({
                "mcpServers": {
                    "code-review-graph": {
                        "command": exe_path.to_string_lossy(),
                        "args": ["serve", "--repo", repo_str]
                    }
                }
            });
            println!("Add this to your Cursor MCP config (~/.cursor/mcp.json):");
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        "windsurf" => {
            let config = serde_json::json!({
                "mcpServers": {
                    "code-review-graph": {
                        "command": exe_path.to_string_lossy(),
                        "args": ["serve", "--repo", repo_str]
                    }
                }
            });
            println!("Add this to your Windsurf MCP config (~/.codeium/windsurf/mcp_config.json):");
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        "vscode" => {
            let config = serde_json::json!({
                "mcp": {
                    "servers": {
                        "code-review-graph": {
                            "type": "stdio",
                            "command": exe_path.to_string_lossy(),
                            "args": ["serve", "--repo", repo_str]
                        }
                    }
                }
            });
            println!("Add this to your VS Code settings.json:");
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        "zed" => {
            let config = serde_json::json!({
                "context_servers": {
                    "code-review-graph": {
                        "command": {
                            "path": exe_path.to_string_lossy(),
                            "args": ["serve", "--repo", repo_str]
                        }
                    }
                }
            });
            println!("Add this to your Zed settings.json:");
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        "continue" => {
            let config = serde_json::json!({
                "mcpServers": [{
                    "name": "code-review-graph",
                    "command": exe_path.to_string_lossy(),
                    "args": ["serve", "--repo", repo_str]
                }]
            });
            println!("Add this to your Continue config.json:");
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        "opencode" => {
            let config = serde_json::json!({
                "mcp": {
                    "code-review-graph": {
                        "type": "local",
                        "command": exe_path.to_string_lossy(),
                        "args": ["serve", "--repo", repo_str]
                    }
                }
            });
            println!("Add this to your OpenCode config:");
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        other => {
            eprintln!(
                "Unknown platform '{}'. Supported: claude-code, cursor, windsurf, vscode, zed, continue, opencode",
                other
            );
            std::process::exit(1);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// `daemon`
// ---------------------------------------------------------------------------

fn cmd_daemon(args: DaemonArgs) -> anyhow::Result<()> {
    // When the re-exec'd process is spawned by `daemon_start()`, it arrives here
    // with `--daemon-foreground` set.  Dispatch to the event loop immediately;
    // ignore `args.action` (which would default to "status").
    if args.daemon_foreground {
        return daemon_foreground_loop();
    }

    match args.action.as_str() {
        "start" => daemon_start(),
        "stop" => daemon_stop(),
        "restart" => {
            let _ = daemon_stop();
            daemon_start()
        }
        "status" => daemon_show_status(),
        other => {
            eprintln!(
                "Unknown daemon action '{}'. Use: start, stop, status, restart",
                other
            );
            std::process::exit(1);
        }
    }
}

fn daemon_start() -> anyhow::Result<()> {
    let current_exe =
        std::env::current_exe().context("Cannot determine current executable path")?;

    // Re-exec self with the hidden --daemon-foreground flag.
    // This avoids the unsafe double-fork pattern while detaching from the
    // calling terminal.
    let child = std::process::Command::new(&current_exe)
        .arg("daemon")
        .arg("--daemon-foreground")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn daemon process")?;

    let pid_file = daemon_pid_file()?;
    if let Some(parent) = pid_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_file, child.id().to_string())?;

    println!("Daemon started (pid {})", child.id());
    Ok(())
}

fn daemon_stop() -> anyhow::Result<()> {
    let pid_file = daemon_pid_file()?;
    if !pid_file.exists() {
        eprintln!("No daemon PID file found; daemon may not be running.");
        return Ok(());
    }
    let pid_str = std::fs::read_to_string(&pid_file)?;
    let pid: u32 = pid_str.trim().parse().context("Invalid PID in pid file")?;

    #[cfg(unix)]
    {
        // SECURITY: pass pid as a numeric argument, not a shell expansion.
        std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .status()
            .ok();
    }
    #[cfg(windows)]
    {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdin(std::process::Stdio::null())
            .status()
            .ok();
    }

    let _ = std::fs::remove_file(&pid_file);
    println!("Daemon (pid {pid}) terminated.");
    Ok(())
}

fn daemon_show_status() -> anyhow::Result<()> {
    let pid_file = daemon_pid_file()?;
    if !pid_file.exists() {
        println!("Daemon: not running (no PID file)");
        return Ok(());
    }
    let pid_str = std::fs::read_to_string(&pid_file)?;
    let pid: u32 = pid_str.trim().parse().unwrap_or(0);
    println!("Daemon: running (pid {pid})");
    println!("PID file: {}", pid_file.display());
    Ok(())
}

/// Foreground event loop executed by the re-exec'd daemon process.
fn daemon_foreground_loop() -> anyhow::Result<()> {
    use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
    use std::sync::mpsc;
    use std::time::Duration;

    let registry = load_registry().unwrap_or_default();
    if registry.repos.is_empty() {
        tracing::info!("Daemon: no registered repos to watch; exiting.");
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<Result<Event, notify::Error>>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(tx)?;

    for entry in &registry.repos {
        let path = PathBuf::from(&entry.path);
        if path.exists() {
            watcher.watch(&path, RecursiveMode::Recursive)?;
            tracing::info!("Daemon: watching {}", path.display());
        }
    }

    loop {
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(event)) => {
                for entry in &registry.repos {
                    let repo_path = PathBuf::from(&entry.path);
                    let triggered = event.paths.iter().any(|p| p.starts_with(&repo_path));
                    if triggered {
                        tracing::info!(
                            "Daemon: change detected in '{}', triggering incremental update",
                            entry.alias
                        );
                        if let Ok(store) = GraphStore::new(&entry.db_path) {
                            let opts = crg_cli::build::BuildOptions::default();
                            if let Err(e) = incremental_update(&repo_path, &store, &opts) {
                                tracing::warn!("Incremental update failed: {e}");
                            }
                        }
                        break;
                    }
                }
            }
            Ok(Err(e)) => tracing::warn!("Daemon watch error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

fn daemon_pid_file() -> anyhow::Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(".code-review-graph").join("daemon.pid"))
}

/// Return the user home directory without pulling in the `dirs` crate.
fn dirs_home() -> anyhow::Result<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        return Ok(PathBuf::from(h));
    }
    #[cfg(windows)]
    if let Ok(h) = std::env::var("USERPROFILE") {
        return Ok(PathBuf::from(h));
    }
    anyhow::bail!("Cannot determine home directory (set $HOME)")
}

// ---------------------------------------------------------------------------
// `eval`
// ---------------------------------------------------------------------------

fn cmd_eval(args: EvalArgs) -> anyhow::Result<()> {
    eprintln!("Evaluation benchmarks are not yet implemented.");
    if let Some(repo) = args.repo {
        eprintln!("Repository: {}", repo.display());
    }
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// Registry helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Registry {
    #[serde(default)]
    repos: Vec<RegistryEntry>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RegistryEntry {
    path: String,
    alias: String,
    db_path: String,
}

fn registry_path() -> anyhow::Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(".code-review-graph").join("registry.json"))
}

fn load_registry() -> anyhow::Result<Registry> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(Registry::default());
    }
    let text = std::fs::read_to_string(&path)?;
    let reg: Registry = serde_json::from_str(&text).unwrap_or_default();
    Ok(reg)
}

fn save_registry(registry: &Registry) -> anyhow::Result<()> {
    let path = registry_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(registry)?;
    std::fs::write(&path, text)?;
    Ok(())
}
