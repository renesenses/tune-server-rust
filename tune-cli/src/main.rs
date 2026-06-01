use std::env;
use std::process;

const DEFAULT_SERVER: &str = "http://localhost:8888";

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

fn usage() {
    eprintln!("tune-cli — Tune server command line interface");
    eprintln!();
    eprintln!("Usage: tune [--server URL] [--json] <command> [args]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  status              Show server status");
    eprintln!("  zones               List zones");
    eprintln!("  play <zone> <track> Play a track on a zone");
    eprintln!("  pause <zone>        Pause playback");
    eprintln!("  next <zone>         Skip to next track");
    eprintln!("  volume <zone> <vol> Set volume (0-100)");
    eprintln!("  search <query>      Search the library");
    eprintln!("  scan                Trigger library scan");
    eprintln!("  stats               Library statistics");
    eprintln!("  now-playing <zone>  Current track info");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --server URL  Server address (default: {DEFAULT_SERVER})");
    eprintln!("  --json        Output raw JSON");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let mut server = DEFAULT_SERVER.to_string();
    let mut json_mode = false;
    let mut cmd_args: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" => {
                i += 1;
                if i < args.len() {
                    server = args[i].clone();
                }
            }
            "--json" => json_mode = true,
            "--help" | "-h" => {
                usage();
                return;
            }
            _ => cmd_args.push(args[i].clone()),
        }
        i += 1;
    }

    if cmd_args.is_empty() {
        usage();
        process::exit(1);
    }

    let client = Client::new(&server, json_mode);
    let cmd = cmd_args[0].as_str();

    let result = match cmd {
        "status" => client.get("/system/version").await,
        "stats" => client.get("/system/stats").await,
        "zones" => client.get("/zones").await,
        "scan" => client.post("/system/scan", serde_json::json!({})).await,
        "search" => {
            let q = cmd_args.get(1).map(|s| s.as_str()).unwrap_or("");
            client
                .get(&format!("/search?q={}", urlencoding::encode(q)))
                .await
        }
        "now-playing" => {
            let zone = cmd_args.get(1).map(|s| s.as_str()).unwrap_or("1");
            client.get(&format!("/zones/{zone}/status")).await
        }
        "play" => {
            let zone = cmd_args.get(1).map(|s| s.as_str()).unwrap_or("1");
            let track = cmd_args
                .get(2)
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            client
                .post(
                    &format!("/zones/{zone}/play"),
                    serde_json::json!({"track_id": track}),
                )
                .await
        }
        "pause" => {
            let zone = cmd_args.get(1).map(|s| s.as_str()).unwrap_or("1");
            client
                .post(&format!("/zones/{zone}/pause"), serde_json::json!({}))
                .await
        }
        "next" => {
            let zone = cmd_args.get(1).map(|s| s.as_str()).unwrap_or("1");
            client
                .post(&format!("/zones/{zone}/next"), serde_json::json!({}))
                .await
        }
        "volume" => {
            let zone = cmd_args.get(1).map(|s| s.as_str()).unwrap_or("1");
            let vol = cmd_args
                .get(2)
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(50);
            client
                .post(
                    &format!("/zones/{zone}/volume"),
                    serde_json::json!({"volume": vol}),
                )
                .await
        }
        _ => {
            eprintln!("Unknown command: {cmd}");
            usage();
            process::exit(1);
        }
    };

    match result {
        Ok(v) => client.print(&v),
        Err(e) => {
            eprintln!("Error: {e}");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_url_construction() {
        let client = Client::new("http://192.168.1.18:8888", false);
        assert_eq!(client.base, "http://192.168.1.18:8888/api/v1");
    }

    #[test]
    fn print_human_object() {
        let val = serde_json::json!({"version": "0.8.16", "engine": "rust"});
        print_human(&val); // should not panic
    }

    #[test]
    fn default_server() {
        assert_eq!(DEFAULT_SERVER, "http://localhost:8888");
    }
}
