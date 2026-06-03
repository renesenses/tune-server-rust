use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use serde_json::json;

const DEFAULT_SERVER: &str = "http://localhost:8888";

#[derive(Parser)]
#[command(name = "tune", about = "Tune server command line interface", version)]
struct Cli {
    /// Server URL
    #[arg(long, default_value = DEFAULT_SERVER, env = "TUNE_SERVER")]
    server: String,

    /// Output as JSON
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show server status
    Status,
    /// List zones
    Zones,
    /// Play a track on a zone
    Play {
        /// Zone ID
        zone: i64,
        /// Track ID
        track: i64,
    },
    /// Pause playback on a zone
    Pause {
        /// Zone ID
        zone: i64,
    },
    /// Skip to next track
    Next {
        /// Zone ID
        zone: i64,
    },
    /// Set volume (0-100)
    Volume {
        /// Zone ID
        zone: i64,
        /// Volume level (0-100)
        level: u32,
    },
    /// Search the library
    Search {
        /// Search query
        query: Vec<String>,
    },
    /// Trigger library scan
    Scan,
    /// Library statistics
    Stats,
    /// Current track info
    NowPlaying {
        /// Zone ID
        zone: i64,
    },
    /// Show listening history
    History {
        /// Number of items
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },
    /// Show OAAT endpoint diagnostics
    Oaat,
    /// Generate shell completions
    Completions {
        /// Shell type
        shell: Shell,
    },
    /// Release tooling (offline — operates on the local repo)
    Release {
        #[command(subcommand)]
        action: ReleaseAction,
    },
}

#[derive(Subcommand)]
enum ReleaseAction {
    /// Bump the workspace version (semver). Prints the planned bump
    /// in dry-run mode; pass --apply to actually edit Cargo.toml and
    /// regenerate Cargo.lock.
    Bump {
        /// Bump level: patch, minor, or major
        #[arg(value_parser = ["patch", "minor", "major"])]
        level: String,
        /// Actually apply the bump (default is dry-run)
        #[arg(long)]
        apply: bool,
    },
}

struct Client {
    base: String,
    http: reqwest::Client,
    json_mode: bool,
}

impl Client {
    fn new(server: &str, json_mode: bool) -> Self {
        Self {
            base: format!("{server}/api/v1"),
            http: reqwest::Client::new(),
            json_mode,
        }
    }

    async fn get(&self, path: &str) -> Result<serde_json::Value, String> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("{e}"))?;
        resp.json().await.map_err(|e| format!("{e}"))
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("{e}"))?;
        resp.json().await.map_err(|e| format!("{e}"))
    }

    fn print(&self, value: &serde_json::Value) {
        if self.json_mode {
            println!(
                "{}",
                serde_json::to_string_pretty(value).unwrap_or_default()
            );
        } else {
            print_human(value);
        }
    }
}

// ─── Release tooling (offline) ────────────────────────────────────────

/// Locate the workspace root by walking up from the binary's working
/// directory until we find a Cargo.toml that declares `[workspace]`.
fn find_workspace_root() -> Result<std::path::PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let mut dir = cwd.as_path();
    loop {
        let cargo = dir.join("Cargo.toml");
        if cargo.exists() {
            let s = std::fs::read_to_string(&cargo).map_err(|e| format!("read {cargo:?}: {e}"))?;
            if s.contains("[workspace]") {
                return Ok(dir.to_path_buf());
            }
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => {
                return Err(
                    "could not find workspace root (no Cargo.toml with [workspace])".into(),
                );
            }
        }
    }
}

fn read_workspace_version(root: &std::path::Path) -> Result<(u64, u64, u64), String> {
    let cargo = root.join("Cargo.toml");
    let s = std::fs::read_to_string(&cargo).map_err(|e| format!("read {cargo:?}: {e}"))?;
    for line in s.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("version") {
            let after_eq = rest.split('=').nth(1).unwrap_or("").trim();
            let lit = after_eq.trim_matches('"');
            if lit.is_empty() {
                continue;
            }
            let parts: Vec<&str> = lit.split('.').collect();
            if parts.len() != 3 {
                continue;
            }
            let parse = |p: &str| p.parse::<u64>().map_err(|e| format!("parse {p}: {e}"));
            return Ok((parse(parts[0])?, parse(parts[1])?, parse(parts[2])?));
        }
    }
    Err("no `version = \"X.Y.Z\"` found in workspace Cargo.toml".into())
}

fn bump_version(v: (u64, u64, u64), level: &str) -> Result<(u64, u64, u64), String> {
    match level {
        "patch" => Ok((v.0, v.1, v.2 + 1)),
        "minor" => Ok((v.0, v.1 + 1, 0)),
        "major" => Ok((v.0 + 1, 0, 0)),
        other => Err(format!("unknown bump level: {other}")),
    }
}

fn write_workspace_version(root: &std::path::Path, new: (u64, u64, u64)) -> Result<(), String> {
    let cargo = root.join("Cargo.toml");
    let s = std::fs::read_to_string(&cargo).map_err(|e| format!("read {cargo:?}: {e}"))?;
    let mut out = String::with_capacity(s.len());
    let mut found = false;
    for line in s.lines() {
        if !found && line.trim_start().starts_with("version") && line.contains('=') {
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            out.push_str(&format!(
                "{indent}version = \"{}.{}.{}\"\n",
                new.0, new.1, new.2
            ));
            found = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !found {
        return Err("could not locate version line to rewrite".into());
    }
    std::fs::write(&cargo, out).map_err(|e| format!("write {cargo:?}: {e}"))?;
    Ok(())
}

fn release_run(action: &ReleaseAction) -> Result<(), String> {
    match action {
        ReleaseAction::Bump { level, apply } => {
            let root = find_workspace_root()?;
            let current = read_workspace_version(&root)?;
            let new = bump_version(current, level)?;
            println!("  Current : {}.{}.{}", current.0, current.1, current.2);
            println!("  Bump    : {level}");
            println!("  Next    : {}.{}.{}", new.0, new.1, new.2);
            if !*apply {
                println!("\nDry run. Pass --apply to actually rewrite Cargo.toml + Cargo.lock.");
                return Ok(());
            }
            write_workspace_version(&root, new)?;
            println!("\n  [ok] Cargo.toml rewritten");
            // Regenerate Cargo.lock via cargo update -w
            let status = std::process::Command::new("cargo")
                .arg("update")
                .arg("-w")
                .current_dir(&root)
                .status()
                .map_err(|e| format!("spawn cargo update: {e}"))?;
            if !status.success() {
                return Err(format!(
                    "cargo update -w failed (exit {})",
                    status.code().unwrap_or(-1)
                ));
            }
            println!("  [ok] Cargo.lock regenerated");
            println!(
                "\nNext steps:\n  git add Cargo.toml Cargo.lock\n  git commit -m 'bump v{}.{}.{}'\n  git tag v{}.{}.{}\n  git push origin main --tags",
                new.0, new.1, new.2, new.0, new.1, new.2
            );
            Ok(())
        }
    }
}

fn print_human(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                match v {
                    serde_json::Value::String(s) => println!("  {k}: {s}"),
                    serde_json::Value::Number(n) => println!("  {k}: {n}"),
                    serde_json::Value::Bool(b) => println!("  {k}: {b}"),
                    serde_json::Value::Array(arr) => println!("  {k}: [{} items]", arr.len()),
                    _ => println!("  {k}: {v}"),
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                print_human(item);
                println!("  ---");
            }
        }
        _ => println!("{value}"),
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Commands::Completions { shell } = &cli.command {
        clap_complete::generate(*shell, &mut Cli::command(), "tune", &mut std::io::stdout());
        return;
    }

    if let Commands::Release { action } = &cli.command {
        match release_run(action) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }

    let client = Client::new(&cli.server, cli.json);

    let result = match &cli.command {
        Commands::Status => client.get("/system/version").await,
        Commands::Zones => client.get("/zones").await,
        Commands::Play { zone, track } => {
            client
                .post(
                    &format!("/zones/{zone}/play"),
                    json!({"track_id": track, "source": "local"}),
                )
                .await
        }
        Commands::Pause { zone } => {
            client
                .post(&format!("/zones/{zone}/pause"), json!({}))
                .await
        }
        Commands::Next { zone } => client.post(&format!("/zones/{zone}/next"), json!({})).await,
        Commands::Volume { zone, level } => {
            client
                .post(
                    &format!("/zones/{zone}/volume"),
                    json!({"volume": *level as f64 / 100.0}),
                )
                .await
        }
        Commands::Search { query } => {
            let q = query.join(" ");
            client
                .get(&format!(
                    "/library/search?q={}&limit=20",
                    urlencoding::encode(&q)
                ))
                .await
        }
        Commands::Scan => client.post("/system/scan", json!({})).await,
        Commands::Stats => client.get("/system/stats").await,
        Commands::NowPlaying { zone } => client.get(&format!("/zones/{zone}")).await,
        Commands::History { limit } => client.get(&format!("/history?limit={limit}")).await,
        Commands::Oaat => client.get("/system/diagnostics/oaat").await,
        Commands::Completions { .. } => unreachable!(),
        Commands::Release { .. } => unreachable!(),
    };

    match result {
        Ok(data) => client.print(&data),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
