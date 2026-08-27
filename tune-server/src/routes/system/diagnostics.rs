use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::migrations;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

/// Number of recent log lines embedded in a bug report (kept modest so the
/// forum thread stays readable; the "Export logs" button has the full tail).
const BUG_REPORT_LOG_LINES: usize = 200;

/// Fenêtre LUE avant filtrage, pour que les 200 lignes retenues soient 200
/// lignes utiles.
///
/// Mesuré sur un rapport réel (#1884, Bertrand, analyse acoustique figée) :
/// **160 des 200 lignes étaient la même sonde `ssdp_unicast_probe_ok` en
/// DEBUG**, et le rapport ne contenait pas une seule ligne acoustique — la
/// fenêtre couvrait moins de trois minutes. Un rapport arrivé vide de ce qui
/// concerne le défaut oblige à redemander un journal complet, et un
/// signalement sur deux s'éteint en route.
const BUG_REPORT_LOG_SCAN_LINES: usize = 3000;

/// Ne garder d'un journal que ce qui documente un défaut.
///
/// Le DEBUG des modules de découverte est une sonde de bon fonctionnement :
/// sa place est dans le fichier et dans l'export complet, pas dans un rapport
/// de bogue où il chasse tout le reste. On ne garde donc que INFO et au-dessus.
///
/// Une ligne de continuation — celle d'une trace d'erreur, qui ne porte ni
/// horodatage ni niveau — hérite de la décision prise pour la ligne qui la
/// précède : découper une trace en deux vaudrait moins que de la jeter
/// entière.
fn lignes_utiles_pour_un_rapport(journal: &str, garder: usize) -> String {
    let mut retenu: Vec<&str> = Vec::new();
    // Une ligne sans niveau reconnu ouvre le journal : on la garde, faute de
    // quoi un format inattendu viderait le rapport au lieu de l'alléger.
    let mut on_garde = true;
    for ligne in journal.lines() {
        match niveau_de_ligne(ligne) {
            Some(niveau) => {
                on_garde = !matches!(niveau, "DEBUG" | "TRACE");
                if on_garde {
                    retenu.push(ligne);
                }
            }
            None => {
                if on_garde {
                    retenu.push(ligne);
                }
            }
        }
    }
    // Le rapport passe désormais par la MÊME sélection que l'export (#1974) :
    // un module ne peut occuper plus d'un quart de la fenêtre. Il ne l'avait
    // pas, et il tronquait bêtement.
    //
    // Trois journaux de testeurs la même semaine l'exigeaient, et jamais avec
    // le même coupable : chez Bilou, `tune_server::scan_import` et
    // `tune_core::metadata` prenaient les deux tiers de 1 003 lignes — zéro
    // ligne d'enrichissement ne survivait, alors que c'était le sujet de son
    // signalement ; chez Jean Valjean, la boucle de sondage UPnP en prenait
    // 807 sur 1 003. Plafonner le module tient quel que soit le bavard du
    // jour, là où nommer les coupables un à un ne tient jamais longtemps.
    //
    // Écrire ici un SECOND mécanisme aurait été le vrai piège : deux réponses
    // à la même question dérivent, et c'est exactement ce que la doctrine du
    // dépôt interdit.
    let candidates: Vec<String> = retenu.into_iter().map(str::to_owned).collect();
    let (gardees, ecartees) = selectionner_lignes(candidates, garder);
    let mut sortie = gardees.join("\n");
    // Un rapport qui tait ce qu'il a laissé tomber se lit comme s'il avait
    // tout montré — même règle que pour l'export.
    for (module, combien) in ecartees {
        sortie.push_str(&format!(
            "\n… {combien} lignes de « {module} » écartées du rapport (elles sont dans l'export complet)"
        ));
    }
    sortie
}

/// Le niveau d'une ligne de journal, quand elle en porte un.
///
/// Format écrit par `tracing` : `2026-08-17T15:22:15.003+02:00  DEBUG
/// tune_core::discovery::ssdp: …`. On ne cherche le niveau que dans les
/// premiers champs — un `DEBUG` au milieu d'un message ne doit pas faire
/// passer la ligne pour du DEBUG.
fn niveau_de_ligne(ligne: &str) -> Option<&'static str> {
    for mot in ligne.split_whitespace().take(3) {
        match mot {
            "TRACE" => return Some("TRACE"),
            "DEBUG" => return Some("DEBUG"),
            "INFO" => return Some("INFO"),
            "WARN" => return Some("WARN"),
            "ERROR" => return Some("ERROR"),
            _ => {}
        }
    }
    None
}

/// L'horodatage en tête d'une ligne de journal, sous la forme brute écrite par
/// `tracing` (`2026-08-20T09:03:15.059+02:00`), tronqué à la minute.
fn horodatage_de_ligne(ligne: &str) -> Option<&str> {
    let premier = ligne.split_whitespace().next()?;
    // `2026-08-20T09:03` — dix caractères de date, un `T`, cinq d'heure.
    if premier.len() >= 16 && premier.as_bytes()[10] == b'T' && premier.starts_with("20") {
        Some(&premier[..16])
    } else {
        None
    }
}

/// La période réellement couverte par un extrait de journal, `du … au …`.
///
/// Trois mille lignes couvrent des heures sur un serveur au repos et **dix
/// minutes** sur un serveur qui scanne (#2028). L'utilisateur qui décrit un
/// blocage vieux de plusieurs heures nous envoie alors un journal qui ne peut
/// rien en contenir — et rien, ni pour lui ni pour nous, ne distingue ce
/// rapport-là d'un rapport qui couvre la journée. On l'annonce donc.
fn periode_couverte(extrait: &str) -> Option<String> {
    let mut lignes = extrait.lines().filter_map(horodatage_de_ligne);
    let debut = lignes.next()?;
    match lignes.last() {
        Some(fin) if fin != debut => Some(format!("du {debut} au {fin}")),
        _ => Some(format!("à {debut}")),
    }
}

/// Public bug-intake endpoint on the community site. It creates a *moderated*
/// (pending) forum thread server-side with the site's own credentials — the
/// distributed Tune server never holds a forum admin token. Same
/// `/api/v1/community/*` family as the DAC-profile / covers endpoints.
const BUG_REPORT_SUBMIT_URL: &str = "https://mozaiklabs.fr/api/v1/community/bug-report";

/// The community endpoint caps the thread body at 50k chars; keep headroom.
const BUG_REPORT_MAX_BODY_CHARS: usize = 49_000;

pub(super) async fn diagnostics(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let db_version = if state.backend.engine() == tune_core::db::engine::Engine::Sqlite {
        state
            .db
            .as_ref()
            .and_then(|db| migrations::current_version(db).ok())
            .unwrap_or(0)
    } else {
        0
    };
    let music_dirs = super::get_music_dirs_list(&state.backend);
    let uptime_secs = state.started_at.elapsed().as_secs();

    // Zone count
    let zone_count = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);

    // Discovered devices grouped by type
    let scanner = &state.scanner;
    let devices = scanner.devices().await;
    let mut devices_by_type: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for d in &devices {
        devices_by_type
            .entry(d.device_type.to_string())
            .or_default()
            .push(d.name.clone());
    }

    // Connectors (streaming services)
    let registry = state.services.lock().await;
    let connectors: Vec<String> = registry.list();
    drop(registry);

    // Audio outputs
    let audio_backend_pref = &state.display_audio_backend();
    let (audio_outputs, audio_backend_name, asio_avail) = {
        #[cfg(feature = "local-audio")]
        {
            let devs: Vec<String> =
                tune_core::outputs::local::list_audio_devices_with_backend(audio_backend_pref)
                    .iter()
                    .map(|d| d.name.clone())
                    .collect();
            let name = tune_core::outputs::local::active_backend_name(audio_backend_pref);
            let asio = tune_core::outputs::local::asio_available();
            (devs, name, asio)
        }
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = audio_backend_pref;
            (Vec::<String>::new(), "none", false)
        }
    };

    // Scan status
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let scan_result: Option<serde_json::Value> = settings
        .get("scan_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());

    // Memory RSS
    let rss_mb = get_rss_mb();

    // DB backend
    let db_backend = settings
        .get("db_engine")
        .ok()
        .flatten()
        .unwrap_or_else(|| "sqlite".into());

    Json(json!({
        "server_version": tune_core::version(),
        "rust_version": tune_core::rustc_version(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "uptime_seconds": uptime_secs,
        "memory_rss_mb": rss_mb,
        "db_backend": db_backend,
        "active_zones": zone_count,
        // #2154 — une base incomplète ne doit plus pouvoir ignorer des
        // réglages pendant des mois sans laisser de trace dans le rapport.
        "zone_settings_ignored": tune_core::db::zone_repo::zone_settings_ignored(),
        "discovered_devices": devices_by_type,
        "connectors": connectors,
        "audio_outputs_available": audio_outputs,
        "audio_backend": audio_backend_name,
        "asio_available": asio_avail,
        // #2392 : pourquoi un fournisseur de sortie hors-arbre est inerte.
        // Absent de la liste = non compilé ; présent avec un `refusal` = droit
        // manquant, et le refus dit lequel et quoi faire ; présent sans refus
        // et `devices: 0` = il cherche et ne trouve rien. Ces trois cas
        // donnaient jusqu'ici le même écran vide.
        "output_providers": crate::discovery_setup::provider_status_snapshot(),
        "scan_status": {
            "status": scan_status,
            "tracks": tracks,
            "albums": albums,
            "last_result": scan_result,
        },
        "features": tune_core::enabled_features(),
        // Legacy fields kept for backward compatibility
        "engine": "rust",
        "platform": std::env::consts::OS,
        "pid": std::process::id(),
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "db": {
            "engine": db_backend,
            "migration_version": db_version,
        },
        "music_dirs": music_dirs,
        "tracks_count": tracks,
        "albums_count": albums,
        "artists_count": artists,
        "rust_engines": {
            "available": true,
            "version": tune_core::version(),
            "metadata_engine": "lofty",
            "discovery_engine": "mdns-sd + socket2",
            "scanner_engine": "walkdir + rayon",
            "db_engine": "rusqlite",
        },
    }))
}

/// Read process RSS in megabytes. Returns None on unsupported platforms.
fn get_rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * 4096 / 1024 / 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id();
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8(o.stdout)
                    .ok()?
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .map(|kb| kb / 1024)
            })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None::<u64>
    }
}

/// La section « fournisseurs de sortie » d'un rapport de bogue.
///
/// Vide quand le binaire n'embarque aucun fournisseur hors-arbre : il n'y a
/// alors rien à dire, et une section vide dans chaque rapport serait du bruit.
fn section_fournisseurs_de_sortie(instantane: &Value) -> String {
    let Some(fournisseurs) = instantane["providers"].as_array().filter(|l| !l.is_empty()) else {
        return String::new();
    };

    let mut md = String::from("## Output Providers\n");
    if !instantane["account_linked"].as_bool().unwrap_or(true) {
        md.push_str(
            "- ⚠ **No linked Mozaiklabs account** — paid module entitlements travel with the \
             account, never with the license key, so no paid output module can be active.\n",
        );
    }
    let modules = instantane["licensed_modules"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    md.push_str(&format!(
        "- Licensed modules: {}\n",
        if modules.is_empty() { "none" } else { &modules }
    ));

    for f in fournisseurs {
        let nom = f["provider"].as_str().unwrap_or("?");
        let appareils = f["devices"].as_u64().unwrap_or(0);
        match f["refusal"]["code"].as_str() {
            Some(code) => md.push_str(&format!(
                "- {nom}: **idle — {code}** ({})\n",
                f["refusal"]["message"].as_str().unwrap_or("")
            )),
            None => md.push_str(&format!("- {nom}: active, {appareils} device(s)\n")),
        }
    }
    md.push('\n');
    md
}

pub(super) async fn diagnostics_bundle(State(state): State<AppState>) -> Json<Value> {
    diagnostics(State(state)).await
}

pub(super) async fn diagnostics_network(State(state): State<AppState>) -> Json<Value> {
    let scanner = &state.scanner;
    let devices = scanner.devices().await;
    let outputs = state.outputs.lock().await;
    let output_count = outputs.list().len();
    Json(json!({
        "discovered_devices": devices.len(),
        "registered_outputs": output_count,
        "devices": devices.iter().map(|d| json!({
            "id": d.id,
            "name": d.name,
            "host": d.host,
            "type": format!("{:?}", d.device_type),
        })).collect::<Vec<_>>(),
    }))
}

pub(super) async fn diagnostics_oaat(State(state): State<AppState>) -> Json<Value> {
    let outputs = state.outputs.lock().await;
    let mut endpoints = Vec::new();
    for id in outputs.list() {
        if let Some(output) = outputs.get(&id) {
            let output = output.lock().await;
            if let Some(diag) = output.diagnostics_json() {
                endpoints.push(diag);
            }
        }
    }
    Json(json!({
        "oaat_endpoints": endpoints,
        "count": endpoints.len(),
    }))
}

pub(super) async fn health_monitor(State(state): State<AppState>) -> Json<Value> {
    let report = state.health_monitor.run_checks().await;
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    Json(json!({
        "status": report.status,
        "uptime_seconds": report.uptime_seconds,
        "tracks": tracks,
        "scan_status": scan_status,
        "engine": "rust",
        "checks": report.checks,
        "alerts": report.alerts,
    }))
}

pub(super) async fn health_alerts(State(state): State<AppState>) -> Json<Value> {
    let alerts = state.health_monitor.alerts().await;
    Json(json!({ "alerts": alerts }))
}

#[derive(Deserialize)]
pub(super) struct LogsQuery {
    lines: Option<usize>,
}

/// Bounded tail window for `/system/logs`, kept bounded regardless of how
/// large the append-only log has grown (rotation only runs at startup, so a
/// long-running server's file can reach hundreds of MB).
///
/// 8 MiB et non 2 : depuis #1974 on lit `CANDIDATE_FACTOR` fois plus de lignes
/// que demandé pour pouvoir SÉLECTIONNER au lieu de tronquer. 8 000 lignes de
/// journal pèsent environ 1,6 Mo — 2 Mo passait tout juste, et « tout juste »
/// se transforme en fenêtre amputée le jour où les messages s'allongent, sans
/// que rien ne le dise.
const LOG_TAIL_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug)]
enum LogTailError {
    /// No file at the path — fall through to journalctl/syslog fallbacks.
    Missing,
    /// The file exists but reading it failed — surfaced as such instead of
    /// the misleading "No log file found".
    Unreadable(String),
}

fn read_log_tail(
    log_path: &str,
    max_lines: usize,
    tail_bytes: u64,
) -> Result<Vec<String>, LogTailError> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LogTailError::Missing),
        Err(e) => return Err(LogTailError::Unreadable(e.to_string())),
    };
    let unreadable = |e: std::io::Error| LogTailError::Unreadable(e.to_string());
    let len = f.metadata().map_err(unreadable)?.len();
    let start = len.saturating_sub(tail_bytes);
    f.seek(SeekFrom::Start(start)).map_err(unreadable)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(unreadable)?;

    let text = String::from_utf8_lossy(&buf);
    // If we started mid-file the first line is likely truncated — drop it.
    let body = if start > 0 {
        text.find('\n').map(|nl| &text[nl + 1..]).unwrap_or("")
    } else {
        &text
    };

    let lines: Vec<&str> = body.lines().rev().take(max_lines).collect();
    Ok(lines.into_iter().rev().map(str::to_string).collect())
}

/// Combien de lignes brutes lire avant d'en selectionner `max_lines`.
///
/// Rebalancer suppose d'avoir le choix : prendre exactement les 1000
/// dernieres lignes, c'est deja avoir subi la troncature qu'on veut corriger.
/// On lit donc large, puis on selectionne.
const CANDIDATE_FACTOR: usize = 8;

/// Part maximale d'une fenetre d'export qu'un seul sous-systeme peut occuper.
///
/// Les sous-systemes n'ecrivent pas au meme rythme : `discovery::ssdp` ecrit
/// toutes les quelques secondes (annonces reseau, re-enregistrements),
/// `audio::embedding` une ligne par lot — une toutes les quinze minutes. Sur
/// une fenetre a plafond simple, le premier chasse mecaniquement le second.
///
/// Autrement dit : **plus un traitement est lent, donc plus il est suspect,
/// moins il a de chances d'apparaitre dans l'export.** L'outil de diagnostic
/// est aveugle exactement la ou on en a besoin.
///
/// Mesure sur deux exports de Bilou (#1974) : 529 et 562 lignes de SSDP sur
/// 1003. Le second ne contenait AUCUNE ligne d'embedding, alors que l'analyse
/// acoustique etait l'objet du signalement. Il avait fourni le bon fichier, au
/// bon moment, et il etait inexploitable.
const QUOTA_PAR_MODULE: f64 = 0.25;

/// Le module (`target` de tracing) d'une ligne de log, si elle en porte un.
///
/// Format du writer (`fmt::layer()` par defaut, `bootstrap.rs`) :
/// `<horodatage>  INFO tune_core::discovery::ssdp: message`.
///
/// Rend `None` pour tout ce qui ne suit pas cette forme — continuation d'un
/// message multiligne, trace de panique, sortie d'un tiers. Ces lignes-la ne
/// sont JAMAIS ecartees : une ligne qu'on ne sait pas classer est une ligne
/// dont on ne sait pas si elle compte.
fn module_de_la_ligne(ligne: &str) -> Option<&str> {
    const NIVEAUX: [&str; 5] = [" ERROR ", " WARN ", " INFO ", " DEBUG ", " TRACE "];
    let (_, apres) = NIVEAUX
        .iter()
        .find_map(|n| ligne.split_once(n).map(|p| (n, p)))?;
    let cible = apres.1.split_whitespace().next()?;
    let cible = cible.strip_suffix(':')?;
    // `tune_core::discovery::ssdp` — un module, pas un mot isole comme le
    // debut d'une phrase. Sans cette exigence, un message qui commence par
    // « erreur: » se ferait compter comme un module a lui tout seul.
    if cible.is_empty() || !cible.contains("::") {
        return None;
    }
    Some(cible)
}

/// Choisit `max_lines` lignes parmi `candidates`, en empechant un seul module
/// d'occuper plus de [`QUOTA_PAR_MODULE`] de la fenetre.
///
/// Deux passes, et la seconde est ce qui rend la premiere sans risque :
///
/// 1. du plus recent au plus ancien, on garde chaque ligne dont le module n'a
///    pas epuise son quota ;
/// 2. si la fenetre n'est pas pleine — parce que les quotas ont beaucoup
///    ecarte — on la complete avec les lignes mises de cote, toujours du plus
///    recent au plus ancien.
///
/// La seconde passe garantit qu'on ne rend JAMAIS moins de lignes que la
/// troncature simple : a taille egale, l'export dit strictement plus. Sur une
/// machine ou seul SSDP parle, il reste donc integralement.
///
/// Rend les lignes dans l'ordre chronologique, et le decompte par module de ce
/// qui a ete ecarte — un export qui tait ce qu'il a laisse tomber se lit comme
/// s'il avait tout montre.
fn selectionner_lignes(
    candidates: Vec<String>,
    max_lines: usize,
) -> (Vec<String>, std::collections::BTreeMap<String, usize>) {
    use std::collections::BTreeMap;

    if max_lines == 0 {
        return (Vec::new(), BTreeMap::new());
    }
    if candidates.len() <= max_lines {
        return (candidates, BTreeMap::new());
    }

    let quota = ((max_lines as f64 * QUOTA_PAR_MODULE).floor() as usize).max(1);
    let mut comptes: BTreeMap<String, usize> = BTreeMap::new();
    // `Option<String>` et non l'indice : on garde la ligne retenue et, pour
    // celles mises de cote, de quoi les reprendre en seconde passe.
    let mut retenues: Vec<usize> = Vec::with_capacity(max_lines);
    let mut ecartees: Vec<usize> = Vec::new();

    for (i, ligne) in candidates.iter().enumerate().rev() {
        if retenues.len() >= max_lines {
            break;
        }
        match module_de_la_ligne(ligne) {
            Some(m) => {
                let n = comptes.entry(m.to_string()).or_insert(0);
                if *n < quota {
                    *n += 1;
                    retenues.push(i);
                } else {
                    ecartees.push(i);
                }
            }
            // Non classable : jamais ecartee.
            None => retenues.push(i),
        }
    }

    // Seconde passe : completer avec ce qu'on avait mis de cote.
    for i in ecartees.iter().copied() {
        if retenues.len() >= max_lines {
            break;
        }
        retenues.push(i);
    }

    retenues.sort_unstable();

    // Ce qu'on RAPPORTE comme mis de cote, et le calcul n'est pas celui qu'on
    // ecrit d'abord.
    //
    // Compter toutes les lignes non retenues serait faux, et faussement
    // alarmant : sur 3000 lignes lues pour une fenetre de 1000, la troncature
    // simple en jetait deja 2000 sans jamais le dire. Les annoncer ici ferait
    // passer pour une perte ce qui est le fonctionnement normal d'une fenetre.
    //
    // Le seul chiffre honnete est le DEPLACEMENT : les lignes qui auraient
    // figure dans la fenetre d'avant — les `max_lines` dernieres — et qu'on a
    // ecartees au profit d'autres. C'est exactement ce que le quota a coute, ni
    // plus ni moins. Un module bavard seul en scene n'y apparait donc pas : il
    // n'a rien cede a personne.
    //
    // Ce calcul est le second : le premier comptait tout, et le test de
    // non-regression `un_seul_module_bavard_reste_entier` l'a refuse.
    let seuil_ancienne_fenetre = candidates.len().saturating_sub(max_lines);
    let retenu: std::collections::BTreeSet<usize> = retenues.iter().copied().collect();
    let mut vraiment_ecartees: BTreeMap<String, usize> = BTreeMap::new();
    for i in seuil_ancienne_fenetre..candidates.len() {
        if retenu.contains(&i) {
            continue;
        }
        if let Some(m) = module_de_la_ligne(&candidates[i]) {
            *vraiment_ecartees.entry(m.to_string()).or_insert(0) += 1;
        }
    }

    let lignes = retenues
        .into_iter()
        .map(|i| candidates[i].clone())
        .collect::<Vec<_>>();
    (lignes, vraiment_ecartees)
}

pub(super) async fn logs(Query(q): Query<LogsQuery>) -> Json<Value> {
    collect_recent_logs(q.lines.unwrap_or(1000)).await
}

/// Collect the most recent server logs (tail): log file first, then
/// journalctl/syslog (Linux) or stderr files / unified log (macOS). Returns a
/// `Json<Value>` with `logs`/`lines`/`source`. Shared by the `/logs` endpoint
/// and the bug report so both surface identical output. Async because the tail
/// read runs on a blocking pool (spawn_blocking) to keep off the Tokio runtime.
pub(super) async fn collect_recent_logs(max_lines: usize) -> Json<Value> {
    // Try the server's own log file first — same path the writer uses (main),
    // resolved via the shared helper so reader and writer always agree. This is
    // what makes "Export logs" work on Linux under Docker / a bare terminal,
    // where journalctl doesn't apply and no file existed before.
    let log_path = crate::config::default_log_file_path()
        .to_string_lossy()
        .into_owned();

    // Read only a bounded tail, off the async runtime. Reading the whole file
    // with read_to_string both blocked a Tokio worker (same trap as
    // admin_errors, #1096) and could fail outright on a low-RAM box once the
    // file had grown large — and that failure fell through to the misleading
    // "No log file found" fallback, exporting an empty log (Yacine, DS418j
    // 1 GB RAM).
    {
        let path = log_path.clone();
        // On lit CANDIDATE_FACTOR fois plus de lignes que demandé, puis on
        // sélectionne : rebalancer suppose d'avoir le choix, et prendre
        // exactement les N dernières lignes c'est déjà avoir subi la troncature
        // qu'on veut corriger (#1974).
        let a_lire = max_lines.saturating_mul(CANDIDATE_FACTOR).max(max_lines);
        let tail =
            tokio::task::spawn_blocking(move || read_log_tail(&path, a_lire, LOG_TAIL_BYTES)).await;
        match tail {
            Ok(Ok(brutes)) => {
                let lues = brutes.len();
                let (lines, ecartees) = selectionner_lignes(brutes, max_lines);
                if !ecartees.is_empty() {
                    tracing::info!(
                        lues,
                        rendues = lines.len(),
                        ecartees = ?ecartees,
                        "log_export_rebalanced"
                    );
                }
                return Json(json!({
                    "logs": lines.join("\n"),
                    "lines": lines.len(),
                    "source": "file",
                    "path": log_path,
                    // Ce qui a été mis de côté, par module. Un export qui tait
                    // ce qu'il a laissé tomber se lit comme s'il avait tout
                    // montré — et c'est exactement ce qui a coûté deux
                    // allers-retours à Bilou.
                    "scanned_lines": lues,
                    "set_aside": ecartees,
                }));
            }
            Ok(Err(LogTailError::Unreadable(e))) => {
                return Json(json!({
                    "logs": format!("Log file exists but could not be read: {e}\nPath: {log_path}"),
                    "lines": 0,
                    "source": "file_unreadable",
                    "path": log_path,
                }));
            }
            // Missing file or a cancelled blocking task: try the fallbacks.
            Ok(Err(LogTailError::Missing)) | Err(_) => {}
        }
    }

    // Try journalctl on Linux (multiple service names)
    #[cfg(target_os = "linux")]
    {
        // `tune` D'ABORD : c'est le nom de l'unité sur Tune OS
        // (`/etc/systemd/system/tune.service`, posé par l'image), et il
        // manquait à cette liste. Conséquence : sur l'appliance que nous
        // distribuons, l'export de journaux ne trouvait JAMAIS rien — ni
        // fichier (le serveur y écrit sur la sortie standard, captée par
        // systemd), ni journalctl (mauvais nom d'unité), ni syslog. Le
        // testeur recevait « No log file found. Launch Tune from a terminal »,
        // conseil absurde sur un boîtier sans écran, et nous joignait un
        // fichier de quatre lignes (Stéphane Villerio, 19/08).
        for service in &["tune", "tune-server", "tune-rust"] {
            if let Ok(output) = std::process::Command::new("journalctl")
                .args([
                    "-u",
                    service,
                    "-n",
                    &max_lines.to_string(),
                    "--no-pager",
                    "-o",
                    "short-iso",
                ])
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    let count = text.lines().count();
                    if count > 1 {
                        return Json(json!({
                            "logs": text,
                            "lines": count,
                            "source": "journalctl",
                            "service": service,
                        }));
                    }
                }
            }
        }
        // Fallback: read from /var/log/syslog
        if let Ok(content) = std::fs::read_to_string("/var/log/syslog") {
            let lines: Vec<&str> = content
                .lines()
                .filter(|l| l.contains("tune-server") || l.contains("tune_"))
                .rev()
                .take(max_lines)
                .collect();
            if !lines.is_empty() {
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                return Json(json!({
                    "logs": lines.join("\n"),
                    "lines": lines.len(),
                    "source": "syslog",
                }));
            }
        }
    }

    // macOS: try stderr log files FIRST (Homebrew launchd captures tracing
    // output here), then fall back to `log show`.  The tracing logs contain
    // the actual application events (auto_next, track_ended, etc.) while
    // `log show` only captures CoreAudio/system noise.
    #[cfg(target_os = "macos")]
    {
        let stderr_paths = [
            format!(
                "{}/Library/Logs/tune-server.log",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
            ),
            "/usr/local/var/log/tune-server.log".into(),
            "/opt/homebrew/var/log/tune-server.log".into(),
        ];
        for p in &stderr_paths {
            if let Ok(content) = std::fs::read_to_string(p) {
                let lines: Vec<&str> = content.lines().rev().take(max_lines).collect();
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                if !lines.is_empty() {
                    return Json(json!({
                        "logs": lines.join("\n"),
                        "lines": lines.len(),
                        "source": "file",
                        "path": p,
                    }));
                }
            }
        }

        // Fallback: macOS unified log — filter to Tune tracing lines only
        if let Ok(output) = std::process::Command::new("log")
            .args([
                "show",
                "--predicate",
                "process == \"tune-server\"",
                "--last",
                "5m",
                "--style",
                "compact",
            ])
            .output()
        {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let lines: Vec<&str> = text
                    .lines()
                    .filter(|l| {
                        l.contains("tune_")
                            || l.contains("INFO")
                            || l.contains("WARN")
                            || l.contains("ERROR")
                    })
                    .collect();
                let lines: Vec<&str> = lines.into_iter().rev().take(max_lines).collect();
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                if !lines.is_empty() {
                    return Json(json!({
                        "logs": lines.join("\n"),
                        "lines": lines.len(),
                        "source": "macos_log",
                    }));
                }
            }
        }
    }

    // Fallback: check stderr capture file (Linux / non-macOS)
    #[cfg(not(target_os = "macos"))]
    {
        let stderr_paths: [String; 3] = [
            format!(
                "{}/Library/Logs/tune-server.log",
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
            ),
            "/usr/local/var/log/tune-server.log".into(),
            "/opt/homebrew/var/log/tune-server.log".into(),
        ];
        for p in &stderr_paths {
            if let Ok(content) = std::fs::read_to_string(p) {
                let lines: Vec<&str> = content.lines().rev().take(max_lines).collect();
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                if !lines.is_empty() {
                    return Json(json!({
                        "logs": lines.join("\n"),
                        "lines": lines.len(),
                        "source": "file",
                        "path": p,
                    }));
                }
            }
        }
    }

    // Dire ce qui a été tenté, pas seulement ce qui a échoué.
    //
    // « No log file found » avec un seul chemin laissait croire à un problème
    // de fichier, alors que trois mécanismes distincts ont été essayés. Sans
    // cette liste, ni le testeur ni nous ne pouvons dire lequel a manqué — et
    // c'est nous qui redemandons un journal que sa machine ne sait pas
    // produire.
    #[cfg(target_os = "linux")]
    let tentatives = format!(
        "Chemins et sources essayés :\n  - fichier : {log_path}\n           - journalctl -u tune / tune-server / tune-rust\n  - /var/log/syslog"
    );
    #[cfg(not(target_os = "linux"))]
    let tentatives = format!("Chemins et sources essayés :\n  - fichier : {log_path}");

    Json(json!({
        "logs": format!(
            "Aucun journal accessible. Si Tune tourne en service, la commande \
             ci-dessous le donne en direct :\n  journalctl -u tune -n 2000 --no-pager\n\n{tentatives}"
        ),
        "lines": 0,
        "source": "none",
    }))
}

// --- Log level management ---

#[derive(Deserialize)]
pub(super) struct LogLevelBody {
    level: String,
}

pub(super) async fn get_log_level(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let level = settings
        .get("log_level")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_LOG").ok())
        .unwrap_or_else(|| "info".into());
    Json(json!({
        "level": level,
        "available": ["error", "warn", "info", "debug", "trace"],
    }))
}

pub(super) async fn set_log_level(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<LogLevelBody>,
) -> Json<Value> {
    let valid = ["error", "warn", "info", "debug", "trace"];
    let level = body.level.to_lowercase();
    if !valid.contains(&level.as_str()) {
        return Json(json!({ "error": format!("Invalid level: {}. Use: {:?}", level, valid) }));
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let _ = settings.set("log_level", &level);

    // Also update the TUNE_LOG env var for the current process
    // SAFETY: single-threaded env access at this point
    unsafe {
        std::env::set_var("TUNE_LOG", &level);
    }

    Json(json!({
        "status": "ok",
        "level": level,
        "note": "Log level saved. Full effect after server restart.",
    }))
}

/// Generate a bug report with comprehensive diagnostic data.
/// Returns JSON that can also be rendered as markdown by the client.
pub(super) async fn generate_bug_report(State(state): State<AppState>) -> Json<Value> {
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let uptime_secs = state.started_at.elapsed().as_secs();
    let db_version = if state.backend.engine() == tune_core::db::engine::Engine::Sqlite {
        state
            .db
            .as_ref()
            .and_then(|db| migrations::current_version(db).ok())
            .unwrap_or(0)
    } else {
        0
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let music_dirs = super::get_music_dirs_list(&state.backend);
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());

    // Zones
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone_count = zone_repo.count().unwrap_or(0);
    let zone_settings_ignored = tune_core::db::zone_repo::zone_settings_ignored();
    let zones: Vec<Value> = zone_repo
        .list()
        .unwrap_or_default()
        .iter()
        .map(|z| json!({ "id": z.id, "name": z.name, "output_type": z.output_type }))
        .collect();

    // Streaming services status
    let registry = state.services.lock().await;
    let service_status = registry.status_all().await;
    drop(registry);

    // Discovered devices
    let scanner = &state.scanner;
    let devices = scanner.devices().await;
    let outputs = state.outputs.lock().await;
    let output_count = outputs.list().len();
    drop(outputs);

    let uptime_str = format!(
        "{}d {}h {}m {}s",
        uptime_secs / 86400,
        (uptime_secs % 86400) / 3600,
        (uptime_secs % 3600) / 60,
        uptime_secs % 60,
    );

    // Memory RSS
    let rss_mb = {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map(|pages| pages * 4096 / 1024 / 1024)
        }
        #[cfg(target_os = "macos")]
        {
            let pid = std::process::id();
            std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()?
                        .trim()
                        .parse::<u64>()
                        .ok()
                        .map(|kb| kb / 1024)
                })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None::<u64>
        }
    };

    // OAAT diagnostics
    let oaat_endpoints: Vec<Value> = {
        let outputs = state.outputs.lock().await;
        outputs
            .list()
            .iter()
            .filter_map(|id| {
                let output = outputs.get(id)?;
                let output = output.try_lock().ok()?;
                output.diagnostics_json()
            })
            .collect()
    };

    // Build markdown text
    let mut md = String::new();
    md.push_str("# Tune Bug Report\n\n");
    md.push_str(&format!(
        "**Version**: {} (engine: rust)\n",
        tune_core::version()
    ));
    md.push_str(&format!(
        "**Platform**: {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    md.push_str(&format!("**Uptime**: {uptime_str}\n"));
    md.push_str(&format!("**PID**: {}\n", std::process::id()));
    if let Some(rss) = rss_mb {
        md.push_str(&format!("**Memory**: {rss} MB RSS\n"));
    }
    md.push('\n');

    md.push_str("## Library\n");
    md.push_str(&format!("- Tracks: {tracks}\n"));
    md.push_str(&format!("- Albums: {albums}\n"));
    md.push_str(&format!("- Artists: {artists}\n"));
    md.push_str(&format!("- Music dirs: {}\n", music_dirs.join(", ")));
    md.push_str(&format!("- Scan status: {scan_status}\n\n"));

    md.push_str(&format!("## Zones ({zone_count})\n"));
    for z in &zones {
        md.push_str(&format!(
            "- {} ({})\n",
            z["name"].as_str().unwrap_or("?"),
            z["output_type"].as_str().unwrap_or("?")
        ));
    }
    md.push_str(&format!(
        "- Zone settings not persisted: {zone_settings_ignored}\n"
    ));
    md.push('\n');

    md.push_str("## Streaming Services\n");
    for s in &service_status {
        let auth = if s["authenticated"].as_bool().unwrap_or(false) {
            "authenticated"
        } else {
            "not authenticated"
        };
        let enabled = if s["enabled"].as_bool().unwrap_or(false) {
            "enabled"
        } else {
            "disabled"
        };
        md.push_str(&format!(
            "- {}: {}, {}\n",
            s["name"].as_str().unwrap_or("?"),
            enabled,
            auth
        ));
    }
    md.push('\n');

    md.push_str("## Network\n");
    md.push_str(&format!("- Discovered devices: {}\n", devices.len()));
    md.push_str(&format!("- Registered outputs: {output_count}\n"));
    md.push('\n');

    // #2392 : c'est CE bloc qui aurait épargné au bêta-testeur du module
    // Diretta une réinstallation complète de Fedora. Un rapport de bogue qui
    // dit « fournisseur diretta, 0 appareil, aucun compte lié » se lit en dix
    // secondes ; un rapport muet oblige à tout redemander.
    md.push_str(&section_fournisseurs_de_sortie(
        &crate::discovery_setup::provider_status_snapshot(),
    ));

    if !oaat_endpoints.is_empty() {
        md.push_str(&format!("## OAAT Endpoints ({})\n", oaat_endpoints.len()));
        for ep in &oaat_endpoints {
            md.push_str(&format!(
                "- {} ({}): connected={}, packets={}, format={}\n",
                ep["name"].as_str().unwrap_or("?"),
                ep["host"].as_str().unwrap_or("?"),
                ep["connected"].as_bool().unwrap_or(false),
                ep["packets_sent"].as_u64().unwrap_or(0),
                ep["format"].as_str().unwrap_or("?"),
            ));
            if ep["stall_detected"].as_bool().unwrap_or(false) {
                md.push_str("  **⚠ STALL DETECTED**\n");
            }
        }
        md.push('\n');
    }

    md.push_str("## Database\n");
    md.push_str(&format!("- Engine: sqlite\n"));
    md.push_str(&format!("- Migration version: {db_version}\n"));

    // Recent logs (tail) — the single most useful part of a bug report. Reuses
    // the same collector as the /logs endpoint so the report matches what the
    // "Export logs" button shows.
    // On lit large et on filtre, plutôt que de lire 200 lignes et d'espérer
    // qu'elles parlent du défaut (#1884). L'export complet, lui, reste verbatim.
    let Json(logs_json) = collect_recent_logs(BUG_REPORT_LOG_SCAN_LINES).await;
    let brut = logs_json["logs"].as_str().unwrap_or("");
    let filtre = lignes_utiles_pour_un_rapport(brut, BUG_REPORT_LOG_LINES);
    let log_text = filtre.trim();
    let log_source = logs_json["source"].as_str().unwrap_or("none");
    let periode = periode_couverte(log_text)
        .map(|p| format!(", {p}"))
        .unwrap_or_default();
    md.push_str(&format!(
        "\n## Recent Logs ({BUG_REPORT_LOG_LINES} dernières lignes INFO et au-dessus{periode}, source: {log_source} — le DEBUG est dans l'export complet)\n"
    ));
    if log_text.is_empty() {
        md.push_str("_No logs available._\n");
    } else {
        md.push_str("```\n");
        md.push_str(log_text);
        md.push_str("\n```\n");
    }

    Json(json!({
        "version": tune_core::version(),
        "engine": "rust",
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "uptime_seconds": uptime_secs,
        "uptime": uptime_str,
        "pid": std::process::id(),
        "rss_mb": rss_mb,
        "library": {
            "tracks": tracks,
            "albums": albums,
            "artists": artists,
            "music_dirs": music_dirs,
            "scan_status": scan_status,
        },
        "zones": {
            "count": zone_count,
            "items": zones,
        },
        "zone_settings_ignored": zone_settings_ignored,
        "streaming_services": service_status,
        "network": {
            "discovered_devices": devices.len(),
            "registered_outputs": output_count,
        },
        "oaat_endpoints": oaat_endpoints,
        "database": {
            "engine": "sqlite",
            "migration_version": db_version,
        },
        "markdown": md,
    }))
}

/// Returns the bug report as raw markdown (text/markdown) for direct forum paste.
pub(super) async fn bug_report_markdown(
    State(state): State<AppState>,
) -> (
    axum::http::StatusCode,
    [(axum::http::header::HeaderName, &'static str); 1],
    String,
) {
    let Json(report) = generate_bug_report(State(state)).await;
    let md = report["markdown"].as_str().unwrap_or("").to_string();
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        md,
    )
}

#[derive(Deserialize)]
pub(super) struct BugReportSubmitBody {
    #[serde(default)]
    description: String,
}

/// POST /system/bug-report/submit — build the local bug report (diagnostics +
/// recent logs), prepend the user's free-text description, and forward it to the
/// mozaiklabs.fr community bug endpoint, which creates a *moderated* (pending)
/// `bug` forum thread with its own credentials and returns the public URL. Done
/// server-to-server (this Rust process, not the browser) so it dodges the cloud's
/// CORS origin allow-list and can attach the instance id / version / OS the
/// browser doesn't have. The distributed server never holds a forum admin token.
pub(super) async fn submit_bug_report(
    State(state): State<AppState>,
    Json(body): Json<BugReportSubmitBody>,
) -> (axum::http::StatusCode, Json<Value>) {
    use axum::http::StatusCode;

    let description = body.description.trim().to_string();

    // Build the diagnostics + logs report (same content as the preview/markdown).
    let backend = state.backend.clone();
    let Json(report) = generate_bug_report(State(state)).await;
    let report_md = report["markdown"].as_str().unwrap_or("").to_string();
    if report_md.trim().is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "empty bug report" })),
        );
    }

    // Compose the thread body: the user's own words first, then diagnostics.
    let full_markdown = if description.is_empty() {
        report_md
    } else {
        format!("{description}\n\n---\n\n{report_md}")
    };

    let version = tune_core::version();
    let platform = std::env::consts::OS;

    // Title: first non-empty line of the description, else a generic one.
    let title = description
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| format!("Bug: {}", l.chars().take(80).collect::<String>()))
        .unwrap_or_else(|| format!("Bug report — Tune {version} ({platform})"));

    // The site caps the body at 50k chars — truncate the tail (oldest logs) if
    // the report runs long rather than getting rejected wholesale.
    let body_md = if full_markdown.chars().count() > BUG_REPORT_MAX_BODY_CHARS {
        let kept: String = full_markdown
            .chars()
            .take(BUG_REPORT_MAX_BODY_CHARS)
            .collect();
        format!("{kept}\n\n_…report truncated…_")
    } else {
        full_markdown
    };

    let instance_id = tune_core::db::settings_repo::SettingsRepo::with_backend(backend)
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    // Contract of the community bug-report endpoint: { title?, body, os?, version?, instance_id? }.
    let payload = json!({
        "title": title,
        "body": body_md,
        "os": platform,
        "version": version,
        "instance_id": instance_id,
    });

    let client = match tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("http client: {e}") })),
            );
        }
    };

    match client
        .post(BUG_REPORT_SUBMIT_URL)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            // Site responds { status, thread: { id, slug, url } }.
            let data: Value = resp.json().await.unwrap_or_else(|_| json!({}));
            let thread = &data["thread"];
            (
                StatusCode::OK,
                Json(json!({
                    "status": "ok",
                    "url": thread.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                    "slug": thread.get("slug").and_then(|v| v.as_str()).unwrap_or(""),
                })),
            )
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            tracing::warn!(status, "bug_report_submit_rejected");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "cloud rejected the report", "status": status })),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "bug_report_submit_failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("could not reach the bug service: {e}") })),
            )
        }
    }
}

pub(super) async fn audio_check() -> Json<Value> {
    let formats = vec![
        "flac", "wav", "aiff", "mp3", "aac", "ogg", "opus", "alac", "dsd", "wavpack", "ape",
    ];

    Json(json!({
        "native_engine": true,
        "supported_formats": formats,
        "lofty_available": true,
        "engine": "rust",
    }))
}

/// Anonymous telemetry snapshot — returns what would be sent if telemetry
/// is enabled. No data leaves the server unless the user explicitly opts in.
pub(super) async fn telemetry_snapshot(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings.get("telemetry_enabled").ok().flatten().as_deref() == Some("true");
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let zone_count = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let uptime = state.started_at.elapsed().as_secs();

    Json(json!({
        "enabled": enabled,
        "payload": {
            "version": tune_core::version(),
            "engine": "rust",
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "uptime_seconds": uptime,
            "tracks": tracks,
            "albums": albums,
            "artists": artists,
            "zones": zone_count,
        }
    }))
}

pub(super) async fn telemetry_toggle(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let _ = settings.set("telemetry_enabled", if enabled { "true" } else { "false" });
    Json(json!({ "enabled": enabled }))
}

pub(super) async fn api_stats(State(state): State<AppState>) -> Json<Value> {
    let stats = state.api_analytics.stats();
    Json(serde_json::to_value(stats).unwrap_or_default())
}

pub(super) async fn api_insights(State(state): State<AppState>) -> Json<Value> {
    let stats = state.api_analytics.stats();
    let mut issues: Vec<Value> = Vec::new();

    // High error rate
    if stats.error_rate_pct > 5.0 {
        issues.push(json!({
            "severity": "warning",
            "type": "high_error_rate",
            "message": format!("API error rate is {:.1}% (threshold: 5%)", stats.error_rate_pct),
        }));
    }

    // Slow endpoints (P95 > 500ms)
    for ep in &stats.slowest_endpoints {
        if ep.p95_latency_ms > 500 {
            issues.push(json!({
                "severity": "warning",
                "type": "slow_endpoint",
                "endpoint": ep.endpoint,
                "p95_ms": ep.p95_latency_ms,
                "message": format!("{} P95 latency {}ms (threshold: 500ms)", ep.endpoint, ep.p95_latency_ms),
            }));
        }
    }

    // Zone poller issues
    let metrics = state.poller_metrics.lock().await;
    for (zone_id, m) in metrics.iter() {
        if m.total_polls > 10 && m.total_errors > 0 {
            let err_pct = m.total_errors as f64 / m.total_polls as f64 * 100.0;
            if err_pct > 10.0 {
                issues.push(json!({
                    "severity": "error",
                    "type": "zone_poll_failures",
                    "zone_id": zone_id,
                    "error_rate_pct": (err_pct * 10.0).round() / 10.0,
                    "message": format!("Zone {} has {:.0}% poll error rate", zone_id, err_pct),
                }));
            }
        }
        if m.max_latency_ms > 2000 {
            issues.push(json!({
                "severity": "warning",
                "type": "zone_high_latency",
                "zone_id": zone_id,
                "max_latency_ms": m.max_latency_ms,
                "message": format!("Zone {} max latency {}ms", zone_id, m.max_latency_ms),
            }));
        }
    }
    drop(metrics);

    let status = if issues.iter().any(|i| i["severity"] == "error") {
        "degraded"
    } else if issues.is_empty() {
        "healthy"
    } else {
        "warning"
    };

    Json(json!({
        "status": status,
        "issues": issues,
        "total_issues": issues.len(),
        "api_requests_analyzed": stats.total_requests,
    }))
}

pub(super) async fn api_docs() -> Json<Value> {
    let routes = vec![
        // System
        ("GET", "/system/version", "Server version and engine"),
        ("GET", "/system/health", "Health check"),
        (
            "GET",
            "/system/stats",
            "Library statistics (tracks, albums, artists, zones)",
        ),
        ("GET", "/system/diagnostics", "Full diagnostic report"),
        ("GET", "/system/changelog", "Version changelog"),
        (
            "GET",
            "/system/api-stats",
            "Per-endpoint latency and error analytics",
        ),
        (
            "GET",
            "/system/api-docs",
            "This endpoint — API documentation",
        ),
        ("GET", "/system/telemetry", "Telemetry snapshot (opt-in)"),
        ("POST", "/system/scan", "Trigger library scan"),
        ("GET", "/system/scan/status", "Scan progress"),
        ("GET", "/system/logs", "Server logs"),
        ("GET", "/system/backups", "List backups"),
        ("POST", "/system/backups", "Create backup"),
        ("POST", "/system/backups/encrypt", "Create encrypted backup"),
        ("POST", "/system/import/roon", "Import from Roon"),
        ("POST", "/system/import/jriver", "Import from JRiver XML"),
        ("POST", "/system/import/plex", "Import from Plex"),
        // Library
        (
            "GET",
            "/library/albums",
            "List albums (paginated, filterable)",
        ),
        (
            "GET",
            "/library/albums/grouped",
            "Albums grouped by release (deluxe/remastered)",
        ),
        ("GET", "/library/albums/{id}", "Album details"),
        ("GET", "/library/albums/{id}/tracks", "Album tracks"),
        (
            "GET",
            "/library/albums/{id}/completeness",
            "Album track completeness check",
        ),
        ("GET", "/library/artists", "List artists"),
        (
            "GET",
            "/library/artists/{id}/timeline",
            "Artist discography with gaps",
        ),
        ("GET", "/library/tracks", "List tracks (paginated)"),
        (
            "GET",
            "/library/tracks/{id}/waveform",
            "Track waveform (200-point amplitude)",
        ),
        (
            "GET",
            "/library/tracks/{id}/synced-lyrics",
            "Synchronized lyrics (.lrc)",
        ),
        (
            "GET",
            "/library/tracks/{id}/source-links",
            "Cross-service matches",
        ),
        (
            "POST",
            "/library/identify",
            "Identify track via AcoustID fingerprint",
        ),
        (
            "GET",
            "/library/duplicates",
            "Duplicate tracks (hash + fingerprint + metadata)",
        ),
        (
            "GET",
            "/library/stats/completeness",
            "Library health score (A-F grade)",
        ),
        ("GET", "/library/genre-tree", "Hierarchical genre tree"),
        ("GET", "/search", "Federated search (local + streaming)"),
        // Zones & Playback
        ("GET", "/zones", "List zones"),
        ("POST", "/zones", "Create zone"),
        (
            "GET",
            "/zones/{id}/status",
            "Zone playback status + credits",
        ),
        (
            "GET",
            "/zones/{id}/network-health",
            "Zone network quality metrics",
        ),
        ("GET", "/zones/sync-status", "All zones with poller metrics"),
        ("POST", "/zones/{id}/play", "Play track/album/playlist"),
        ("POST", "/zones/{id}/pause", "Pause"),
        ("POST", "/zones/{id}/next", "Next track"),
        ("POST", "/zones/{id}/sleep", "Sleep timer with fade"),
        ("GET", "/zones/{id}/dsp", "Zone DSP/EQ config"),
        // Streaming
        (
            "GET",
            "/streaming/services",
            "List streaming services status",
        ),
        (
            "GET",
            "/streaming/compare",
            "Compare search across services",
        ),
        (
            "GET",
            "/streaming/{service}/search",
            "Search a streaming service",
        ),
        // Playlists
        ("GET", "/playlists", "List playlists"),
        ("POST", "/playlists", "Create playlist"),
        (
            "GET",
            "/playlists/{id}/export",
            "Export (format=m3u|json|csv|xspf)",
        ),
        // Radio & DJ
        ("GET", "/radio/auto", "Auto-DJ playlist from seed track"),
        ("GET", "/radios", "List radio stations"),
        // Dashboard
        ("GET", "/dashboard/stats", "Listening dashboard"),
        ("GET", "/dashboard/wrapped", "Year-in-review Wrapped stats"),
        ("GET", "/dashboard/top-artists", "Top artists"),
        ("GET", "/dashboard/genre-breakdown", "Genre distribution"),
        // Party
        ("POST", "/party/rooms", "Create collaborative room"),
        ("GET", "/party/rooms", "List rooms"),
        // Other
        (
            "POST",
            "/voice-search",
            "Voice search via Whisper transcription",
        ),
        (
            "GET",
            "/demo/library",
            "Read-only library browse (demo mode)",
        ),
    ];

    let endpoints: Vec<Value> = routes.iter().map(|(method, path, desc)| {
        json!({"method": method, "path": format!("/api/v1{path}"), "description": desc})
    }).collect();

    Json(json!({
        "version": tune_core::version(),
        "total_endpoints": endpoints.len(),
        "endpoints": endpoints,
    }))
}

/// List ASIO audio devices (Windows-only, requires `asio` feature).
pub(super) async fn asio_devices(State(_state): State<AppState>) -> Json<Value> {
    #[cfg(feature = "local-audio")]
    {
        let devices = tokio::task::spawn_blocking(tune_core::outputs::local::list_asio_devices)
            .await
            .unwrap_or_default();
        let count = devices.len();
        Json(json!({
            "devices": devices,
            "asio_available": tune_core::outputs::local::asio_available(),
            "count": count,
        }))
    }
    #[cfg(not(feature = "local-audio"))]
    {
        Json(json!({
            "devices": [],
            "asio_available": false,
            "count": 0,
        }))
    }
}

#[cfg(test)]
mod log_tail_tests {
    use super::*;

    #[test]
    fn missing_file_is_missing_not_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.log");
        match read_log_tail(path.to_str().unwrap(), 10, 1024) {
            Err(LogTailError::Missing) => {}
            _ => panic!("expected Missing"),
        }
    }

    #[test]
    fn tail_window_drops_truncated_first_line_and_caps_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let content: String = (0..100).map(|i| format!("line-{i:03}\n")).collect();
        std::fs::write(&path, &content).unwrap();

        // Window smaller than the file: starts mid-file, first partial line dropped.
        let lines = read_log_tail(path.to_str().unwrap(), 1000, 95).unwrap();
        assert!(lines.len() < 100);
        assert_eq!(lines.last().unwrap(), "line-099");
        // Every returned line is complete.
        assert!(lines.iter().all(|l| l.starts_with("line-")));

        // max_lines caps the result at the newest lines.
        let lines = read_log_tail(path.to_str().unwrap(), 3, u64::MAX).unwrap();
        assert_eq!(lines, ["line-097", "line-098", "line-099"]);
    }

    #[test]
    fn whole_file_when_window_is_larger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.log");
        std::fs::write(&path, "a\nb\n").unwrap();
        let lines = read_log_tail(path.to_str().unwrap(), 1000, 1024).unwrap();
        assert_eq!(lines, ["a", "b"]);
    }
}

#[cfg(test)]
mod tests_journal_rapport {
    use super::{
        horodatage_de_ligne, lignes_utiles_pour_un_rapport, niveau_de_ligne, periode_couverte,
    };

    /// Le cas mesuré : 160 sondes SSDP en DEBUG chassaient tout le reste.
    #[test]
    fn le_debug_bavard_ne_chasse_plus_ce_qui_compte() {
        let mut journal = String::new();
        for i in 0..160 {
            journal.push_str(&format!(
                "2026-08-17T15:22:15.003+02:00 DEBUG tune_core::discovery::ssdp: ssdp_unicast_probe_ok id=uuid:{i}\n"
            ));
        }
        journal.push_str(
            "2026-08-17T15:25:00.000+02:00  INFO tune_core::audio::embedding: audio_embedding_batch embedded=10\n",
        );
        journal.push_str(
            "2026-08-17T15:25:01.000+02:00  WARN tune_core::audio::embedding: audio_embed_decode_failed track_id=42\n",
        );

        let garde = lignes_utiles_pour_un_rapport(&journal, 200);

        assert!(
            !garde.contains("ssdp_unicast_probe_ok"),
            "le DEBUG bavard sort"
        );
        assert!(garde.contains("audio_embedding_batch"), "l'INFO reste");
        assert!(garde.contains("audio_embed_decode_failed"), "le WARN reste");
        assert_eq!(garde.lines().count(), 2);
    }

    /// La coupe se fait APRÈS le filtrage : on garde N lignes utiles, pas les
    /// N dernières lignes du fichier.
    #[test]
    fn on_garde_les_dernieres_lignes_utiles() {
        let mut journal = String::new();
        for i in 0..10 {
            journal.push_str(&format!("2026-08-17T10:00:0{i}Z  INFO m: utile-{i}\n"));
            journal.push_str(&format!("2026-08-17T10:00:0{i}Z DEBUG m: bruit-{i}\n"));
        }
        let garde = lignes_utiles_pour_un_rapport(&journal, 3);
        assert_eq!(garde.lines().count(), 3);
        assert!(garde.contains("utile-9") && garde.contains("utile-7"));
        assert!(!garde.contains("utile-6"), "seules les trois dernières");
        assert!(!garde.contains("bruit"));
    }

    /// Une trace d'erreur suit sa ligne d'en-tête : la découper en deux
    /// vaudrait moins que de la jeter entière.
    #[test]
    fn une_trace_suit_la_ligne_qui_la_porte() {
        let journal = "2026-08-17T10:00:00Z ERROR m: panic\n    at src/lib.rs:12\n    at src/main.rs:3\n\
                       2026-08-17T10:00:01Z DEBUG m: sonde\n    detail de la sonde\n";
        let garde = lignes_utiles_pour_un_rapport(journal, 200);
        assert!(
            garde.contains("at src/lib.rs:12"),
            "la trace de l'ERROR reste"
        );
        assert!(garde.contains("at src/main.rs:3"));
        assert!(!garde.contains("detail de la sonde"), "celle du DEBUG part");
    }

    /// Un format inattendu ne doit pas vider le rapport : sans niveau
    /// reconnu, on garde.
    #[test]
    fn un_journal_sans_niveau_reconnu_est_conserve() {
        let journal = "ligne sans niveau\nune autre\n";
        let garde = lignes_utiles_pour_un_rapport(journal, 200);
        assert_eq!(garde.lines().count(), 2);
    }

    /// Le niveau se lit dans les premiers champs — pas au milieu du message,
    /// sans quoi une ligne parlant de « DEBUG » serait jetée.
    #[test]
    fn le_mot_debug_dans_un_message_ne_compte_pas() {
        assert_eq!(
            niveau_de_ligne("2026-08-17T10:00:00Z  INFO m: log_level=DEBUG applique"),
            Some("INFO")
        );
        assert_eq!(
            niveau_de_ligne("2026-08-17T10:00:00Z DEBUG m: coucou"),
            Some("DEBUG")
        );
        assert_eq!(niveau_de_ligne("    at src/lib.rs:12"), None);

        let journal = "2026-08-17T10:00:00Z  INFO m: log_level=DEBUG applique\n";
        assert!(lignes_utiles_pour_un_rapport(journal, 10).contains("log_level=DEBUG"));
    }

    #[test]
    fn la_periode_couverte_est_annoncee() {
        let j = "2026-08-20T09:03:15.059+02:00  INFO a: debut\n\
                 2026-08-20T09:08:00.000+02:00  WARN a: milieu\n\
                 2026-08-20T09:13:13.491+02:00  INFO a: fin\n";
        assert_eq!(
            periode_couverte(j).unwrap(),
            "du 2026-08-20T09:03 au 2026-08-20T09:13"
        );
    }

    #[test]
    fn une_seule_minute_ne_sannonce_pas_comme_un_intervalle() {
        let j = "2026-08-20T09:03:15.059+02:00  INFO a: seule\n";
        assert_eq!(periode_couverte(j).unwrap(), "à 2026-08-20T09:03");
        let deux = "2026-08-20T09:03:15.059+02:00  INFO a: une\n\
                    2026-08-20T09:03:59.000+02:00  INFO a: deux\n";
        assert_eq!(periode_couverte(deux).unwrap(), "à 2026-08-20T09:03");
    }

    #[test]
    fn un_journal_sans_horodatage_ne_promet_aucune_periode() {
        // Un format inattendu ne doit pas produire une période inventée : mieux
        // vaut ne rien annoncer que d'annoncer faux.
        assert!(periode_couverte("Tune Server 0.9.90 | windows\n=====\n").is_none());
        assert!(periode_couverte("").is_none());
        assert!(horodatage_de_ligne("    at src/lib.rs:12").is_none());
        assert!(horodatage_de_ligne("2026-08-20 09:03:15 INFO a: espace au lieu de T").is_none());
    }

    /// Le cas qui a motivé #2028 : trois mille lignes qui ne couvrent que dix
    /// minutes. Rien ne le disait, et l'en-tête laissait croire à un journal
    /// représentatif.
    #[test]
    fn dix_minutes_de_scan_sannoncent_comme_dix_minutes() {
        let mut j = String::new();
        for i in 0..600 {
            j.push_str(&format!(
                "2026-08-20T09:{:02}:{:02}.000+02:00  INFO tune_server::scan_import: DIAG\n",
                3 + i / 60,
                i % 60
            ));
        }
        assert_eq!(
            periode_couverte(&j).unwrap(),
            "du 2026-08-20T09:03 au 2026-08-20T09:12"
        );
    }
}

/// #1974 — l'export de journaux était noyé par la découverte SSDP.
///
/// Deux exports successifs de Bilou (fil forum 1479, 0.9.88 · Windows) :
/// 529 puis **562 lignes de SSDP sur 1003**. Le second ne contenait AUCUNE
/// ligne d'embedding, alors que l'analyse acoustique était l'objet même du
/// signalement. Il avait fourni le bon fichier, au bon moment, et il était
/// inexploitable.
#[cfg(test)]
mod selection_de_lignes {
    use super::*;

    fn ligne(module: &str, n: usize) -> String {
        format!("2026-08-20T10:00:00.000+02:00  INFO {module}: message {n}")
    }

    /// Reproduit la proportion mesurée : 562 lignes de SSDP, une poignée du
    /// sujet, et le reste réparti. Avant, la fenêtre de 1000 les avalait.
    fn journal_de_bilou() -> Vec<String> {
        let mut v = Vec::new();
        // L'embedding écrit une ligne par lot — une toutes les quinze minutes.
        // Elles sont donc ANCIENNES, et c'est précisément ce qui les
        // condamnait : la troncature garde la fin.
        for i in 0..4 {
            v.push(ligne("tune_core::audio::embedding", i));
        }
        for i in 0..151 {
            v.push(ligne("tune_core::metadata::matcher", i));
        }
        for i in 0..1400 {
            v.push(ligne("tune_core::discovery::ssdp", i));
        }
        v
    }

    #[test]
    fn le_module_est_lu_dans_la_ligne() {
        assert_eq!(
            module_de_la_ligne(&ligne("tune_core::discovery::ssdp", 1)),
            Some("tune_core::discovery::ssdp")
        );
        // Une continuation de message multiligne, une trace de panique : pas de
        // module, donc jamais écartée.
        assert_eq!(module_de_la_ligne("    at src/main.rs:42"), None);
        assert_eq!(module_de_la_ligne(""), None);
        // Un mot isolé suivi de « : » n'est pas un module — sans l'exigence du
        // `::`, un message commençant par « erreur: » compterait pour un module
        // à lui tout seul et se ferait rationner.
        assert_eq!(
            module_de_la_ligne("2026-08-20T10:00:00.000+02:00  WARN erreur: ceci"),
            None
        );
    }

    #[test]
    fn le_signalement_de_bilou_ne_disparait_plus() {
        let (retenues, ecartees) = selectionner_lignes(journal_de_bilou(), 1000);

        assert_eq!(retenues.len(), 1000, "la fenêtre doit rester pleine");

        let compte = |m: &str| retenues.iter().filter(|l| l.contains(m)).count();
        // LE point du ticket : les quatre lignes d'embedding survivent.
        assert_eq!(
            compte("audio::embedding"),
            4,
            "les lignes du sujet signalé ont de nouveau disparu"
        );
        // Et SSDP ne peut plus prendre plus du quart... en première passe.
        // Il en reprend ensuite, faute d'autre chose à montrer — c'est voulu.
        assert!(
            compte("discovery::ssdp") < 1400,
            "SSDP occupe encore toute la fenêtre"
        );
        assert!(!ecartees.is_empty(), "rien n'a été mis de côté ?");
    }

    /// La troncature simple est le point de comparaison : avec la même fenêtre,
    /// elle perdait tout du sujet. Ce test échouerait sur l'ancien code.
    #[test]
    fn la_troncature_simple_perdait_tout() {
        let journal = journal_de_bilou();
        let ancienne: Vec<&String> = journal.iter().rev().take(1000).collect();
        assert_eq!(
            ancienne
                .iter()
                .filter(|l| l.contains("audio::embedding"))
                .count(),
            0,
            "le journal d'essai ne reproduit pas le défaut : revoir les proportions"
        );
    }

    /// Garde-fou de non-régression, et le plus important des trois : on ne rend
    /// JAMAIS moins de lignes qu'avant. Sur une machine où seul SSDP parle, le
    /// quota ne doit rien retirer — il n'y a rien d'autre à montrer.
    #[test]
    fn un_seul_module_bavard_reste_entier() {
        let journal: Vec<String> = (0..3000)
            .map(|i| ligne("tune_core::discovery::ssdp", i))
            .collect();
        let (retenues, ecartees) = selectionner_lignes(journal, 1000);
        assert_eq!(retenues.len(), 1000);
        assert!(
            ecartees.is_empty(),
            "des lignes ont été perdues alors qu'il n'y avait rien à leur préférer"
        );
    }

    #[test]
    fn l_ordre_chronologique_est_conserve() {
        let mut journal = Vec::new();
        for i in 0..50 {
            journal.push(ligne("a::b", i));
            journal.push(ligne("c::d", i));
        }
        let (retenues, _) = selectionner_lignes(journal.clone(), 40);
        // Les retenues doivent apparaître dans le même ordre relatif que dans
        // le journal : un export dont les lignes sont mélangées ne se lit pas.
        let positions: Vec<usize> = retenues
            .iter()
            .map(|l| journal.iter().position(|j| j == l).unwrap())
            .collect();
        let mut triees = positions.clone();
        triees.sort_unstable();
        assert_eq!(positions, triees);
    }

    #[test]
    fn moins_de_candidats_que_demande_rend_tout() {
        let journal: Vec<String> = (0..10).map(|i| ligne("a::b", i)).collect();
        let (retenues, ecartees) = selectionner_lignes(journal.clone(), 1000);
        assert_eq!(retenues, journal);
        assert!(ecartees.is_empty());
    }

    #[test]
    fn une_fenetre_nulle_ne_panique_pas() {
        let (retenues, _) = selectionner_lignes(journal_de_bilou(), 0);
        assert!(retenues.is_empty());
    }

    // --- #2028, dernier volet : le rapport hérite du quota par module ---

    fn ligne_de(module: &str, n: usize) -> String {
        format!(
            "2026-08-20T09:03:{:02}.000+02:00  INFO {module}: evenement n={n}",
            n % 60
        )
    }

    /// Le cœur du défaut : chez Bilou, 311 lignes de `scan_import` et 322 de
    /// `metadata` ne laissaient AUCUNE ligne d'enrichissement dans le rapport
    /// — alors que l'enrichissement était l'objet de son signalement.
    #[test]
    fn le_bavard_ne_chasse_plus_la_ligne_qui_compte_du_rapport() {
        let mut journal = String::new();
        for i in 0..311 {
            journal.push_str(&ligne_de("tune_server::scan_import", i));
            journal.push('\n');
        }
        for i in 0..322 {
            journal.push_str(&ligne_de("tune_core::metadata", i));
            journal.push('\n');
        }
        journal.push_str(
            "2026-08-20T09:13:00.000+02:00  INFO tune_core::enrichment: batch_artist_mbid_match_started count=7837\n",
        );

        let rapport = lignes_utiles_pour_un_rapport(&journal, 200);
        assert!(
            rapport.contains("batch_artist_mbid_match_started"),
            "la ligne rare doit survivre au vacarme"
        );
    }

    /// Le décompte ne rapporte QUE le déplacement — ce que le quota a coûté à
    /// d'autres — et pas le débordement de fenêtre, qui est son fonctionnement
    /// normal. Il faut donc un vrai cas de sauvetage : un module ancien que le
    /// bavard aurait entièrement chassé, et que le quota ramène.
    #[test]
    fn le_rapport_dit_ce_que_le_quota_a_deplace() {
        let mut journal = String::new();
        // Anciennes, et hors de la fenêtre simple : elles n'y seraient jamais.
        for i in 0..50 {
            journal.push_str(&ligne_de("tune_core::orchestrator", i));
            journal.push('\n');
        }
        // Récentes, assez nombreuses pour remplir la fenêtre à elles seules.
        for i in 0..300 {
            journal.push_str(&ligne_de("tune_server::scan_import", i));
            journal.push('\n');
        }

        let rapport = lignes_utiles_pour_un_rapport(&journal, 200);
        assert!(
            rapport.contains("tune_core::orchestrator"),
            "le module ancien doit être sauvé par son quota"
        );
        assert!(rapport.contains("écartées du rapport"), "{rapport}");
        assert!(
            rapport.contains("tune_server::scan_import"),
            "et c'est le bavard qui a cédé la place : {rapport}"
        );
        assert!(rapport.contains("export complet"), "{rapport}");
    }

    #[test]
    fn un_journal_calme_traverse_le_rapport_sans_rien_perdre() {
        // Le quota ne doit pas s'inviter là où personne ne monopolise rien :
        // à taille égale, le rapport ne dit jamais moins qu'avant.
        let mut journal = String::new();
        for i in 0..20 {
            journal.push_str(&ligne_de("tune_core::orchestrator", i));
            journal.push('\n');
        }
        let rapport = lignes_utiles_pour_un_rapport(&journal, 200);
        assert_eq!(rapport.lines().count(), 20);
        assert!(!rapport.contains("écartées"), "rien à annoncer : {rapport}");
    }

    #[test]
    fn le_niveau_filtre_toujours_avant_le_quota() {
        // L'ordre compte : plafonner d'abord laisserait du DEBUG occuper un
        // quota au détriment d'un WARN.
        let journal = "2026-08-20T09:03:00.000+02:00 DEBUG tune_core::discovery::ssdp: sonde a=1\n\
                       2026-08-20T09:03:01.000+02:00  WARN tune_core::outputs::bluos: add_rejected b=2\n";
        let rapport = lignes_utiles_pour_un_rapport(journal, 200);
        assert!(rapport.contains("add_rejected"));
        assert!(!rapport.contains("sonde a=1"));
    }
}

/// #2392 — la section « fournisseurs de sortie » du rapport de bogue.
#[cfg(test)]
mod fournisseurs_de_sortie {
    use super::*;

    /// #2392 : le rapport de bogue doit dire pourquoi un fournisseur payant est
    /// inerte. C'est le canal qui aurait épargné au bêta-testeur du module
    /// Diretta une réinstallation complète de son système d'exploitation.
    #[test]
    fn le_rapport_dit_quand_un_module_paye_est_inerte_faute_de_compte_lie() {
        let instantane = serde_json::json!({
            "account_linked": false,
            "licensed_modules": [],
            "providers": [{
                "provider": "diretta",
                "required_module": "diretta",
                "devices": 0,
                "refusal": {
                    "code": "module_account_not_linked",
                    "message": "link your Mozaiklabs account",
                },
            }],
        });
        let md = section_fournisseurs_de_sortie(&instantane);
        assert!(md.contains("No linked Mozaiklabs account"), "{md}");
        assert!(md.contains("Licensed modules: none"), "{md}");
        assert!(
            md.contains("diretta: **idle — module_account_not_linked**"),
            "{md}"
        );
    }

    /// Droit présent mais rien sur le réseau : l'autre cas, et il doit se lire
    /// différemment — sinon on n'a fait que déplacer l'ambiguïté.
    #[test]
    fn le_rapport_distingue_un_module_actif_qui_ne_trouve_rien() {
        let instantane = serde_json::json!({
            "account_linked": true,
            "licensed_modules": ["diretta"],
            "providers": [{
                "provider": "diretta",
                "required_module": "diretta",
                "devices": 0,
                "refusal": null,
            }],
        });
        let md = section_fournisseurs_de_sortie(&instantane);
        assert!(!md.contains("No linked Mozaiklabs account"), "{md}");
        assert!(md.contains("Licensed modules: diretta"), "{md}");
        assert!(md.contains("diretta: active, 0 device(s)"), "{md}");
    }

    /// Aucun fournisseur hors-arbre (le cas du binaire public, et l'état avant
    /// la première passe) : pas de section du tout, pas de bruit.
    #[test]
    fn aucun_fournisseur_hors_arbre_najoute_aucune_section() {
        assert_eq!(section_fournisseurs_de_sortie(&Value::Null), "");
        assert_eq!(
            section_fournisseurs_de_sortie(&serde_json::json!({ "providers": [] })),
            ""
        );
    }
}
