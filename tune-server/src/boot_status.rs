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

/// Ce que l'étape en cours est en train de traiter — aujourd'hui le greffon
/// en cours de chargement (#5370). Remis à zéro à chaque changement d'étape.
static CURRENT: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

/// Déclare l'étape de démarrage en cours (voir `bootstrap.rs`).
pub fn set_phase(phase: &'static str) {
    *PHASE.lock().unwrap_or_else(|e| e.into_inner()) = phase;
    set_current(None);
}

/// L'étape de démarrage en cours.
pub fn phase() -> &'static str {
    *PHASE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Déclare ce que l'étape en cours traite (le greffon en cours de chargement).
pub fn set_current(current: Option<&str>) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = current.map(str::to_string);
}

/// Ce que l'étape en cours traite, s'il y a lieu.
pub fn current() -> Option<String> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Ce que fait chaque étape de `bootstrap.rs`, en une phrase.
///
/// #5370 — la page disait « Cela peut prendre quelques minutes sur une grande
/// bibliothèque » quelle que soit l'étape, jusque pendant « greffons » : un
/// testeur (603 834 pistes) a demandé, à juste titre, quel rapport il y avait
/// entre la taille de sa bibliothèque et les greffons. La taille de la
/// bibliothèque n'est citée que là où elle compte : la base de données, dont
/// la mise à niveau parcourt les tables.
fn phase_detail(phase: &str) -> &'static str {
    match phase {
        "démarrage" => "Tune prépare son démarrage.",
        "attente du disque de données" => {
            "Tune attend le disque qui contient ses données. Vérifiez qu'il est branché."
        }
        "base de données" => {
            "Tune ouvre sa base de données et la met à niveau si besoin. \
             Cela peut prendre quelques minutes sur une grande bibliothèque."
        }
        "configuration" => "Tune restaure ses réglages et ses zones.",
        "partages réseau" => {
            "Tune remonte les partages réseau de la bibliothèque. \
             Un partage lent ou injoignable peut retarder cette étape."
        }
        "sorties audio" => "Tune recherche les sorties audio de cette machine.",
        "greffons" => "Tune charge ses greffons, un par un.",
        "découverte réseau" => "Tune se prépare à découvrir les appareils du réseau.",
        _ => "Tune termine son démarrage.",
    }
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

    let current = current();
    let body = response(
        path,
        phase(),
        current.as_deref(),
        migration_status::snapshot(),
    );
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
fn response(
    path: &str,
    phase: &str,
    current: Option<&str>,
    progress: Option<MigrationProgress>,
) -> String {
    let detail = progress.as_ref().map(|p| p.describe());
    let (content_type, body) = if wants_json(path) {
        (
            "application/json; charset=utf-8",
            json_body(phase, &progress),
        )
    } else {
        (
            "text/html; charset=utf-8",
            html_body(phase, current, detail.as_deref()),
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
        // 🔴 #3343 — les DEUX champs que le seul client d'API de ce répondeur
        // attend, et qu'il ne trouvait pas.
        //
        // Le chemin est celui-ci : après une mise à jour, le client web sonde
        // `GET /api/v1/system/update/status` toutes les 3 s pendant 180 s et
        // ne recharge la page que si `current_version` a bougé, ou s'il a vu
        // le serveur tomber ET que `update_in_progress` est faux
        // (`tune-web-client/src/components/SettingsView.svelte`, boucle de
        // `installUpdate()` ; `SettingsV2.svelte`, boucle de `installerMaj()`).
        // Or `getUpdateStatus()` appelle `fetch` SANS regarder `res.ok` : notre
        // 503 ne lève donc pas, il est lu comme une réponse normale — et cette
        // réponse ne portait ni l'un ni l'autre. Pendant toute la durée des
        // migrations du nouveau binaire (25,8 s mesurées le 07/09 sur un
        // journal de testeur, davantage sur une grande bibliothèque), le
        // client recevait un JSON parfaitement valide qui ne lui apprenait
        // RIEN, et il ne pouvait même plus conclure « le serveur est tombé ».
        // Passé le plafond, la boucle s'arrête sans recharger : l'onglet reste
        // sur la page d'attente jusqu'à ce qu'on le ferme (fil 1662, JLuc).
        //
        // `current_version` est la version du binaire qui démarre — connue
        // sans base, donc disponible ici — et c'est exactement ce que la route
        // servira une fois debout (`routes/system/update.rs`, `update_status`).
        // Le nouveau serveur l'annonce donc dès sa première connexion acceptée,
        // et le client conclut « la version a bougé » sans attendre la fin des
        // migrations : il recharge, retombe sur la page HTML d'attente qui se
        // rafraîchit toute seule, et sort de l'impasse.
        //
        // `update_in_progress: false` est un CONSTAT, pas une commodité : ce
        // répondeur ne tourne que pendant le démarrage, et un démarrage
        // n'applique aucune mise à jour — celle-ci a eu lieu dans le processus
        // précédent. Sans lui, `!status?.update_in_progress` valait `!undefined`
        // par accident ; il vaut désormais la même chose parce que c'est vrai.
        "current_version": tune_core::version(),
        "update_in_progress": false,
    })
    .to_string()
}

/// Échappe le texte inséré dans la page : un nom de greffon vient d'un
/// manifeste wasm, c'est-à-dire d'un fichier que Tune n'a pas écrit.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn html_body(phase: &str, current: Option<&str>, detail: Option<&str>) -> String {
    // Une migration en cours dit mieux que quiconque ce qui se passe ; sinon,
    // la phrase propre à l'étape.
    let detail = detail.unwrap_or_else(|| phase_detail(phase));
    let current = match current {
        Some(name) if phase == "greffons" => {
            format!(
                "<p>Greffon en cours de chargement : {}.</p>",
                escape_html(name)
            )
        }
        _ => String::new(),
    };
    format!(
        "<!doctype html><html lang=\"fr\"><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"refresh\" content=\"3\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Tune démarre…</title>\
         <style>body{{font-family:system-ui,-apple-system,sans-serif;background:#111;color:#eee;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0;text-align:center}}\
         .c{{max-width:32rem;padding:2rem}}h1{{font-size:1.4rem;font-weight:600}}\
         p{{color:#aaa;line-height:1.6}}</style></head><body><div class=\"c\">\
         <h1>Tune démarre…</h1><p>Étape en cours : {phase}.</p>{current}<p>{detail}</p>\
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
        let raw = response("/api/v1/zones", "base de données", None, Some(progress));
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

    /// #3343 — le client qui ATTEND une mise à jour doit pouvoir conclure.
    ///
    /// Il ne lit que deux champs (`current_version`, `update_in_progress`) et
    /// ne regarde pas le code HTTP : tant que la réponse de démarrage ne les
    /// portait pas, il tournait 180 s dans le vide puis abandonnait sans
    /// recharger, laissant l'onglet sur la page d'attente.
    ///
    /// L'épreuve porte sur les deux formes que sert le répondeur — avec et
    /// sans migration en cours : c'est justement pendant la migration que le
    /// client était aveugle, et la sortie sans progression est celle des
    /// premières secondes du démarrage.
    #[test]
    fn le_client_qui_attend_une_mise_a_jour_lit_la_version_qui_demarre() {
        let sans_migration = response("/api/v1/system/update/status", "démarrage", None, None);
        let avec_migration = response(
            "/api/v1/system/update/status",
            "base de données",
            None,
            Some(MigrationProgress {
                engine: "sqlite",
                done: 0,
                total: 12,
                step: "upgrade_fts5_tables".to_string(),
                elapsed: Duration::from_secs(3),
            }),
        );

        for raw in [sans_migration, avec_migration] {
            let body = raw.split("\r\n\r\n").nth(1).expect("corps absent");
            let v: serde_json::Value = serde_json::from_str(body).expect("JSON invalide");
            assert_eq!(
                v["current_version"],
                serde_json::Value::String(tune_core::version().to_string()),
                "la réponse de démarrage doit annoncer la version du binaire \
                 qui démarre, sinon le client ne voit jamais la version bouger : {body}"
            );
            assert_eq!(
                v["update_in_progress"],
                serde_json::Value::Bool(false),
                "un démarrage n'applique aucune mise à jour ; sans ce champ le \
                 client ne peut pas conclure que le redémarrage est fait : {body}"
            );
        }
    }

    /// Le navigateur, lui, doit voir une page qui explique et se rafraîchit —
    /// c'est tout ce qui séparait « ça travaille » de « c'est planté » (#1701).
    #[test]
    fn browsers_get_a_self_refreshing_page_that_says_what_is_happening() {
        let raw = response("/", "base de données", None, None);
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

    /// Les étapes que `bootstrap.rs` pose réellement, dans l'ordre.
    const ETAPES: [&str; 8] = [
        "démarrage",
        "attente du disque de données",
        "base de données",
        "configuration",
        "partages réseau",
        "sorties audio",
        "greffons",
        "découverte réseau",
    ];

    /// #5370 — chaque étape a sa phrase, et la taille de la bibliothèque
    /// n'est citée que pendant la base de données.
    #[test]
    fn chaque_etape_dit_ce_quelle_fait_et_la_bibliotheque_seulement_pour_la_base() {
        let mut phrases = std::collections::HashSet::new();
        for etape in ETAPES {
            let page = html_body(etape, None, None);
            assert!(
                page.contains(&format!("Étape en cours : {etape}.")),
                "{page}"
            );
            let phrase = phase_detail(etape);
            assert!(page.contains(phrase), "{etape} : phrase absente de {page}");
            assert!(
                phrases.insert(phrase),
                "{etape} partage sa phrase : {phrase}"
            );
            assert_eq!(
                page.contains("grande bibliothèque"),
                etape == "base de données",
                "la taille de la bibliothèque ne compte que pour la base, \
                 pas pour « {etape} » : {page}"
            );
        }
        assert!(
            html_body("greffons", None, None).contains("greffons"),
            "la phrase des greffons doit parler des greffons"
        );
        // Une étape inconnue (ajoutée sans phrase) ne ment pas non plus.
        let inconnue = html_body("étape future", None, None);
        assert!(!inconnue.contains("bibliothèque"), "{inconnue}");
    }

    /// #5370 — pendant « greffons », la page nomme le greffon en cours.
    #[test]
    fn la_page_nomme_le_greffon_en_cours_de_chargement() {
        let page = html_body("greffons", Some("tune-diretta"), None);
        assert!(
            page.contains("Greffon en cours de chargement : tune-diretta."),
            "{page}"
        );
        // Le nom vient parfois d'un manifeste wasm : il est échappé.
        let page = html_body("greffons", Some("<b>x</b>"), None);
        assert!(page.contains("&lt;b&gt;x&lt;/b&gt;"), "{page}");
        assert!(!page.contains("<b>x</b>"), "{page}");
        // Hors de l'étape des greffons, rien n'est nommé.
        let page = html_body("découverte réseau", Some("tune-diretta"), None);
        assert!(!page.contains("tune-diretta"), "{page}");
    }

    /// Une migration en cours garde la main sur l'explication.
    #[test]
    fn une_migration_en_cours_remplace_la_phrase_de_l_etape() {
        let page = html_body("base de données", None, Some("Mise à niveau 5/12"));
        assert!(page.contains("Mise à niveau 5/12"), "{page}");
        assert!(!page.contains(phase_detail("base de données")), "{page}");
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
