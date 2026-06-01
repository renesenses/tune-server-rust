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
    };

    match result {
        Ok(data) => client.print(&data),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
