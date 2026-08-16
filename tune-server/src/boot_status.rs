//! Ce que Tune répond pendant qu'il démarre.
//!
//! La socket d'écoute est ouverte **avant** la base (voir `bootstrap.rs` : c'est
//! délibéré, ça protège la base d'une seconde instance). Mais personne
//! n'acceptait les connexions avant la toute fin du démarrage : elles
//! restaient en file dans le backlog du noyau. Pour l'utilisateur, le
//! navigateur tourne dans le vide et l'application « plante » — c'est ce qu'a
//! vécu le testeur « eric » sur une migration longue (#1701, fil forum 1386),
//! et probablement une partie des « Tune ne démarre pas » sous Windows.
//!
//! Ce répondeur accepte ces connexions pendant le démarrage et répond
//! `503 Service Unavailable` en disant **où on en est** : une page d'attente
//! qui se rafraîchit pour un navigateur, du JSON pour un client d'API. Il
//! s'arrête juste avant qu'axum ne prenne la main, sur le même descripteur
//! dupliqué, donc il n'y a jamais deux accepteurs en même temps.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;
use std::time::Duration;

use tune_core::db::migration_status::{self, MigrationProgress};

/// Étape de démarrage en cours, affichée tant que le serveur ne sert pas.
static PHASE: LazyLock<Mutex<&'static str>> = LazyLock::new(|| Mutex::new("démarrage"));

/// Déclare l'étape de démarrage en cours (voir `bootstrap.rs`).
pub fn set_phase(phase: &'static str) {
    *PHASE.lock().unwrap_or_else(|e| e.into_inner()) = phase;
}

/// L'étape de démarrage en cours.
pub fn phase() -> &'static str {
    *PHASE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Le répondeur de démarrage ; [`stop`](BootResponder::stop) le termine.
pub struct BootResponder {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl BootResponder {
    /// Arrête le répondeur et **attend** que son fil soit sorti : au retour,
    /// plus personne n'accepte sur la socket, et axum peut la reprendre sans
    /// qu'une connexion parte chez le mauvais accepteur.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Démarre le répondeur sur `listener` (un descripteur dupliqué de la socket
/// d'écoute du serveur).
pub fn spawn(listener: TcpListener) -> BootResponder {
    // Non bloquant : c'est ainsi que la boucle peut regarder le drapeau d'arrêt
    // au lieu de rester coincée dans `accept()` jusqu'à la prochaine connexion.
    // En production le descripteur est déjà non bloquant (tokio l'a réglé sur
    // l'original, et `dup` partage les drapeaux), donc c'est un no-op.
    let _ = listener.set_nonblocking(true);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::Builder::new()
        .name("tune-boot-responder".into())
        .spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => answer(stream),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(40));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(200)),
                }
            }
        })
        .ok();

    BootResponder { stop, handle }
}

/// Lit la requête, répond, raccroche. Toute erreur est ignorée : un client qui
/// part en cours de route ne doit pas peser sur un démarrage.
fn answer(mut stream: TcpStream) {
    // La socket acceptée hérite du mode non bloquant sur macOS/BSD : on le
    // retire pour cette connexion-ci, et on borne l'attente pour qu'un client
    // muet ne retarde pas le prochain.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let mut buf = [0u8; 2048];
    let read = stream.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..read]);
    let path = request_path(&head).unwrap_or("/");

    let body = response(path, phase(), migration_status::snapshot());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// Le chemin de la requête HTTP, depuis sa première ligne.
fn request_path(head: &str) -> Option<&str> {
    head.lines().next()?.split_whitespace().nth(1)
}

/// Un client d'API veut du JSON ; un navigateur veut une page.
fn wants_json(path: &str) -> bool {
    path.starts_with("/api/") || path == "/api"
}

/// La réponse HTTP complète servie pendant le démarrage.
fn response(path: &str, phase: &str, progress: Option<MigrationProgress>) -> String {
    let detail = progress.as_ref().map(|p| p.describe());
    let (content_type, body) = if wants_json(path) {
        (
            "application/json; charset=utf-8",
            json_body(phase, &progress),
        )
    } else {
        (
            "text/html; charset=utf-8",
            html_body(phase, detail.as_deref()),
        )
    };

    format!(
        "HTTP/1.1 503 Service Unavailable\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Retry-After: 2\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len()
    )
}

fn json_body(phase: &str, progress: &Option<MigrationProgress>) -> String {
    let migration = match progress {
        Some(p) => serde_json::json!({
            "engine": p.engine,
            "step": (p.done + 1).min(p.total.max(1)),
            "total": p.total.max(1),
            "name": p.step,
            "elapsed_s": p.elapsed.as_secs(),
            "message": p.describe(),
        }),
        None => serde_json::Value::Null,
    };
    serde_json::json!({
        "status": "starting",
        "phase": phase,
        "message": match progress {
            Some(p) => p.describe(),
            None => format!("Tune démarre : {phase}"),
        },
        "migration": migration,
    })
    .to_string()
}

fn html_body(phase: &str, detail: Option<&str>) -> String {
    let detail =
        detail.unwrap_or("Cela peut prendre quelques minutes sur une grande bibliothèque.");
    format!(
        "<!doctype html><html lang=\"fr\"><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"refresh\" content=\"3\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Tune démarre…</title>\
         <style>body{{font-family:system-ui,-apple-system,sans-serif;background:#111;color:#eee;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0;text-align:center}}\
         .c{{max-width:32rem;padding:2rem}}h1{{font-size:1.4rem;font-weight:600}}\
         p{{color:#aaa;line-height:1.6}}</style></head><body><div class=\"c\">\
         <h1>Tune démarre…</h1><p>Étape en cours : {phase}.</p><p>{detail}</p>\
         <p>Ne fermez pas l'application : cette page se rafraîchit toute seule.</p>\
         </div></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_path_is_read_from_the_request_line() {
        assert_eq!(
            request_path("GET /api/v1/zones HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some("/api/v1/zones")
        );
        assert_eq!(request_path(""), None);
    }

    /// Un client d'API doit recevoir du JSON exploitable, pas du HTML.
    #[test]
    fn api_callers_get_json_with_the_migration_step() {
        let progress = MigrationProgress {
            engine: "sqlite",
            done: 4,
            total: 12,
            step: "upgrade_fts5_tables".to_string(),
            elapsed: Duration::from_secs(61),
        };
        let raw = response("/api/v1/zones", "base de données", Some(progress));
        assert!(
            raw.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{raw}"
        );
        assert!(raw.contains("Retry-After: 2"), "{raw}");
        assert!(raw.contains("application/json"), "{raw}");

        let body = raw.split("\r\n\r\n").nth(1).expect("corps absent");
        let v: serde_json::Value = serde_json::from_str(body).expect("JSON invalide");
        assert_eq!(v["status"], "starting");
        assert_eq!(v["phase"], "base de données");
        assert_eq!(v["migration"]["step"], 5);
        assert_eq!(v["migration"]["total"], 12);
        assert_eq!(v["migration"]["name"], "upgrade_fts5_tables");
        assert_eq!(v["migration"]["elapsed_s"], 61);
    }

    /// Le navigateur, lui, doit voir une page qui explique et se rafraîchit —
    /// c'est tout ce qui séparait « ça travaille » de « c'est planté » (#1701).
    #[test]
    fn browsers_get_a_self_refreshing_page_that_says_what_is_happening() {
        let raw = response("/", "base de données", None);
        assert!(raw.contains("text/html"), "{raw}");
        let body = raw.split("\r\n\r\n").nth(1).expect("corps absent");
        assert!(body.contains("Tune démarre"), "{body}");
        assert!(body.contains("http-equiv=\"refresh\""), "{body}");
        assert!(body.contains("base de données"), "{body}");
        // L'annonce d'octets doit être exacte, sinon le client attend la suite.
        let declared: usize = raw
            .split("Content-Length: ")
            .nth(1)
            .and_then(|s| s.split("\r\n").next())
            .and_then(|s| s.parse().ok())
            .expect("Content-Length absent");
        assert_eq!(declared, body.len());
    }

    /// Le vrai test du bug : une connexion qui arrive pendant le démarrage
    /// obtient une réponse au lieu de rester pendue dans le backlog.
    #[test]
    fn a_connection_during_startup_is_answered_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        set_phase("base de données");
        let responder = spawn(listener);

        let mut client = TcpStream::connect(addr).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("write");
        let mut answer = String::new();
        client.read_to_string(&mut answer).expect("read");

        assert!(answer.starts_with("HTTP/1.1 503"), "{answer}");
        assert!(answer.contains("Tune démarre"), "{answer}");

        // Et il rend la socket quand on le lui demande.
        responder.stop();
    }
}
