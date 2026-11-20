#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod decouverte;
mod etat_serveur;
mod ouverture_session;
mod raccourcis;
#[cfg(windows)]
mod raccourcis_globaux;
mod server;
mod serveur_local;
mod volume;
mod vue_mini;
mod zone_cible;

use serde::Serialize;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, State,
};
use tokio::sync::RwLock;

struct AppState {
    server_url: RwLock<String>,
    /// La liste des serveurs a été demandée (lancement sans serveur joignable,
    /// menu « Choisir un serveur… ») : le mini-lecteur embarqué l'ouvre dès
    /// qu'il est prêt, même s'il a manqué l'évènement (#9255).
    choix_demande: std::sync::atomic::AtomicBool,
    /// La zone signalée par la page (`select_zone` ou `/__widget/zone/<id>`).
    /// `None` tant que rien n'a été signalé : voir [`zone_visee`] (#8531).
    active_zone_id: RwLock<Option<i64>>,
    http: reqwest::Client,
}

/// La zone que visent les commandes, les raccourcis et les touches média.
///
/// Celle que la page a signalée ; à défaut, la zone qui joue, sinon en pause,
/// sinon la première (`zone_cible::zone_par_defaut`) ; serveur muet : 1, la
/// valeur d'avant #8531.
async fn zone_visee(state: &AppState) -> i64 {
    if let Some(id) = *state.active_zone_id.read().await {
        return id;
    }
    let url = state.server_url.read().await.clone();
    fetch_widget_data(&state.http, &url, None)
        .await
        .and_then(|d| zone_cible::zone_par_defaut(&d))
        .unwrap_or(1)
}

/// Shape of the `/api/v1/widget/data` payload. Kept as the written record of
/// what the endpoint returns, even though the code currently forwards the raw
/// JSON: deleting it would lose the only place that documents the contract.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
struct WidgetData {
    zones: Vec<serde_json::Value>,
    now_playing: Option<serde_json::Value>,
    state: String,
    zone_id: i64,
    position_ms: i64,
    volume: f64,
    queue_length: i64,
    queue_position: i64,
}

#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let zone_id = zone_visee(&state).await;
    let url = state.server_url.read().await;
    let resp = state
        .http
        .get(format!("{url}/api/v1/widget/data?zone_id={zone_id}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_zones(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let url = state.server_url.read().await;
    let resp = state
        .http
        .get(format!("{url}/api/v1/zones"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn play_pause(state: State<'_, AppState>) -> Result<(), String> {
    let zone_id = zone_visee(&state).await;
    let url = state.server_url.read().await;
    let status: serde_json::Value = state
        .http
        .get(format!("{url}/api/v1/widget/data?zone_id={zone_id}"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let endpoint = if status["state"].as_str() == Some("playing") {
        "pause"
    } else {
        "resume"
    };
    state
        .http
        .post(format!("{url}/api/v1/zones/{zone_id}/{endpoint}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn next_track(state: State<'_, AppState>) -> Result<(), String> {
    let zone_id = zone_visee(&state).await;
    let url = state.server_url.read().await;
    state
        .http
        .post(format!("{url}/api/v1/zones/{zone_id}/next"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn prev_track(state: State<'_, AppState>) -> Result<(), String> {
    let zone_id = zone_visee(&state).await;
    let url = state.server_url.read().await;
    state
        .http
        .post(format!("{url}/api/v1/zones/{zone_id}/previous"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn set_volume(state: State<'_, AppState>, volume: i32) -> Result<(), String> {
    let zone_id = zone_visee(&state).await;
    let url = state.server_url.read().await;
    state
        .http
        .put(format!("{url}/api/v1/zones/{zone_id}/volume"))
        // Une fraction, jamais un pourcentage : voir `volume.rs`.
        .json(&volume::corps_lineaire(volume))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn select_zone(state: State<'_, AppState>, zone_id: i64) -> Result<(), String> {
    *state.active_zone_id.write().await = Some(zone_id);
    Ok(())
}

#[tauri::command]
async fn search(state: State<'_, AppState>, query: String) -> Result<serde_json::Value, String> {
    let url = state.server_url.read().await;
    let resp = state
        .http
        .get(format!(
            "{url}/api/v1/search?q={}&limit=8&sources=local",
            urlencoding::encode(&query)
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}

/// `http://` devant une adresse tapée sans schéma.
fn adresse_complete_de(saisie: &str) -> String {
    let saisie = saisie.trim().trim_end_matches('/');
    if saisie.starts_with("http") {
        saisie.to_string()
    } else {
        format!("http://{saisie}")
    }
}

/// Range le serveur retenu dans `config.json`, en FUSIONNANT : `full_mode` et
/// les réglages de la vue mini vivent dans le même fichier.
///
/// `cle` : l'identité mDNS du serveur quand il a été choisi dans la liste
/// (`decouverte::ServeurTrouve::cle`), pour le retrouver s'il change d'adresse ;
/// `None` pour une adresse tapée, qui efface l'ancienne identité.
fn retenir_serveur(url: &str, cle: Option<&str>) {
    let Some(cfg_file) = config_file() else {
        return;
    };
    let mut cfg: serde_json::Value = std::fs::read_to_string(&cfg_file)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    cfg["server_url"] = serde_json::json!(url);
    match cle {
        Some(c) => cfg["server_cle"] = serde_json::json!(c),
        None => {
            if let Some(o) = cfg.as_object_mut() {
                o.remove("server_cle");
            }
        }
    }
    if let Some(dir) = cfg_file.parent() {
        if std::fs::create_dir_all(dir).is_ok() {
            let _ = std::fs::write(&cfg_file, cfg.to_string());
        }
    }
}

/// Une chaîne de `config.json` (`server_url`, `server_cle`).
fn lire_cfg_texte(cle: &str) -> Option<String> {
    config_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        .and_then(|c| c[cle].as_str().map(str::to_string))
}

#[tauri::command]
async fn set_server_url(state: State<'_, AppState>, url: String) -> Result<String, String> {
    let clean = adresse_complete_de(&url);
    *state.server_url.write().await = clean.clone();
    retenir_serveur(&clean, None);
    Ok(clean)
}

/// Les Tune Server du réseau local (mDNS `_tune-server._tcp`, #9255).
#[tauri::command]
async fn decouvrir_serveurs() -> Result<Vec<decouverte::ServeurTrouve>, String> {
    tokio::task::spawn_blocking(|| {
        decouverte::decouvrir(decouverte::DUREE_PARCOURS, decouverte::Interfaces::Toutes)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Se connecter au serveur choisi dans la liste (ou tapé) : l'adresse est
/// retenue, puis la fenêtre bascule sur ce serveur dans le mode retenu.
#[tauri::command]
async fn choisir_serveur(
    app: tauri::AppHandle,
    url: String,
    cle: Option<String>,
) -> Result<String, String> {
    let clean = adresse_complete_de(&url);
    if url::Url::parse(&clean).is_err() {
        return Err(format!("adresse illisible : {url}"));
    }
    *app.state::<AppState>().server_url.write().await = clean.clone();
    retenir_serveur(&clean, cle.as_deref());
    tracing::info!(server_url = %clean, cle = ?cle, "serveur_choisi");
    set_full_mode(&app, saved_full_mode()).await;
    Ok(clean)
}

/// Le mini-lecteur embarqué demande, une fois prêt, s'il doit ouvrir la liste
/// des serveurs (demande faite avant que sa page ne soit chargée).
#[tauri::command]
async fn choix_serveur_demande(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state
        .choix_demande
        .swap(false, std::sync::atomic::Ordering::SeqCst))
}

#[tauri::command]
async fn get_server_url(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state.server_url.read().await.clone())
}

#[tauri::command]
async fn http_get(state: State<'_, AppState>, url: String) -> Result<serde_json::Value, String> {
    state
        .http
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn http_post(state: State<'_, AppState>, url: String) -> Result<(), String> {
    state
        .http
        .post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn http_put(state: State<'_, AppState>, url: String, body: String) -> Result<(), String> {
    state
        .http
        .put(&url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn play_item(
    state: State<'_, AppState>,
    item_type: String,
    item_id: i64,
) -> Result<(), String> {
    let zone_id = zone_visee(&state).await;
    let url = state.server_url.read().await;
    let body = match item_type.as_str() {
        "album" => serde_json::json!({"album_id": item_id}),
        _ => serde_json::json!({"track_id": item_id}),
    };
    state
        .http
        .post(format!("{url}/api/v1/zones/{zone_id}/play"))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn fetch_widget_data(
    http: &reqwest::Client,
    url: &str,
    zone_id: Option<i64>,
) -> Option<serde_json::Value> {
    let requete = match zone_id {
        Some(id) => format!("{url}/api/v1/widget/data?zone_id={id}"),
        None => format!("{url}/api/v1/widget/data"),
    };
    http.get(requete)
        .send()
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()
}

/// Exécute une action de lecture du menu (ou de son raccourci) sur la zone
/// visée, côté natif : elle marche dans le mini-lecteur embarqué comme dans la
/// page du serveur, sans rien demander à la page.
async fn executer_lecture(app: tauri::AppHandle, action: raccourcis::Action) {
    use raccourcis::Action;
    let state = app.state::<AppState>();
    let url = state.server_url.read().await.clone();
    let zone_id = zone_visee(&state).await;
    let http = state.http.clone();
    match action {
        Action::PisteSuivante => {
            let _ = http
                .post(format!("{url}/api/v1/zones/{zone_id}/next"))
                .send()
                .await;
        }
        Action::PistePrecedente => {
            let _ = http
                .post(format!("{url}/api/v1/zones/{zone_id}/previous"))
                .send()
                .await;
        }
        Action::LecturePause => {
            let playing = fetch_widget_data(&http, &url, Some(zone_id))
                .await
                .map(|d| d["state"].as_str() == Some("playing"))
                .unwrap_or(false);
            let endpoint = if playing { "pause" } else { "resume" };
            let _ = http
                .post(format!("{url}/api/v1/zones/{zone_id}/{endpoint}"))
                .send()
                .await;
        }
        Action::VolumePlus | Action::VolumeMoins => {
            // Le gain et la courbe de la zone sont dans `GET /zones`, pas à la
            // racine de `/widget/data` (voir `volume::zone_dans`). Sans eux,
            // on ne touche à rien plutôt que de repartir d'une valeur inventée.
            let zones = match http.get(format!("{url}/api/v1/zones")).send().await {
                Ok(r) => r.json::<serde_json::Value>().await.ok(),
                Err(_) => None,
            };
            let Some((gain, courbe)) = zones.as_ref().and_then(|z| volume::zone_dans(z, zone_id))
            else {
                tracing::info!(zone_id, "raccourci_volume_zone_inconnue");
                return;
            };
            let delta = if action == Action::VolumePlus { 5 } else { -5 };
            let next = volume::apres_un_pas(gain, courbe, delta);
            let _ = http
                .put(format!("{url}/api/v1/zones/{zone_id}/volume"))
                .json(&serde_json::json!({ "volume": next }))
                .send()
                .await;
        }
        // Pas des actions de lecture : voir `executer_action_menu`.
        _ => {}
    }
}

/// La fenêtre au premier plan (la page du serveur ou le mini-lecteur
/// embarqué), à défaut celle qui est visible.
fn fenetre_active(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    let fenetres: Vec<_> = ["full", "main"]
        .iter()
        .filter_map(|l| app.get_webview_window(l))
        .collect();
    fenetres
        .iter()
        .find(|w| w.is_focused().unwrap_or(false))
        .or_else(|| fenetres.iter().find(|w| w.is_visible().unwrap_or(false)))
        .cloned()
}

/// Exécute une entrée de la barre des menus (#9255).
async fn executer_action_menu(app: tauri::AppHandle, action: raccourcis::Action) {
    use raccourcis::Action;
    match action {
        Action::BasculerMini => {
            let full = !full_mode_active(&app);
            set_full_mode(&app, full).await;
        }
        Action::ChoisirServeur => ouvrir_choix_serveur(&app),
        Action::Reculer | Action::Avancer | Action::Rechercher => {
            let (Some(script), Some(win)) = (raccourcis::script_de(action), fenetre_active(&app))
            else {
                return;
            };
            if let Err(e) = win.eval(script) {
                tracing::warn!(?action, error = %e, "action_menu_echouee");
            }
        }
        _ => executer_lecture(app, action).await,
    }
}

/// Montre la liste des serveurs du réseau dans le mini-lecteur embarqué.
fn ouvrir_choix_serveur(app: &tauri::AppHandle) {
    use tauri::Emitter;
    app.state::<AppState>()
        .choix_demande
        .store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(full) = app.get_webview_window("full") {
        let _ = full.hide();
    }
    if let Some(mini) = app.get_webview_window("main") {
        let _ = mini.show();
        let _ = mini.set_focus();
        let _ = mini.emit("choisir-serveur", ());
    }
}

/// Au lancement : s'assurer d'un serveur (#9255, fil 2968).
///
/// Si l'adresse retenue ne répond pas, le réseau est parcouru
/// (`_tune-server._tcp`) et `decouverte::choisir_au_demarrage` décide : se
/// connecter au seul serveur trouvé, suivre le serveur choisi qui a changé
/// d'adresse, ou montrer la liste. Rend `false` quand la liste est montrée :
/// la fenêtre du mode retenu ne doit alors pas la recouvrir.
async fn assurer_un_serveur(app: &tauri::AppHandle) -> bool {
    let (url, http) = {
        let state = app.state::<AppState>();
        let url = state.server_url.read().await.clone();
        (url, state.http.clone())
    };
    let joignable = server_reachable(&http, &url).await;
    if joignable {
        return true;
    }
    // Le parcours dure ~3 s : un lancement doit montrer quelque chose tout de
    // suite (#8934). Le mini-lecteur embarqué attend, il cède la place à la
    // fenêtre du serveur adopté ou s'ouvre sur la liste.
    if let Some(mini) = app.get_webview_window("main") {
        let _ = mini.show();
    }
    let trouves = tokio::task::spawn_blocking(|| {
        decouverte::decouvrir(decouverte::DUREE_PARCOURS, decouverte::Interfaces::Toutes)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()))
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "decouverte_impossible");
        Vec::new()
    });
    let cle = lire_cfg_texte("server_cle");
    let choisi = lire_cfg_texte("server_url").is_some();
    match decouverte::choisir_au_demarrage(cle.as_deref(), choisi, joignable, &trouves) {
        decouverte::ChoixAuDemarrage::Garder => true,
        decouverte::ChoixAuDemarrage::Adopter(s) => {
            tracing::info!(nom = %s.nom, url = %s.url, "serveur_adopte_au_lancement");
            *app.state::<AppState>().server_url.write().await = s.url.clone();
            retenir_serveur(&s.url, Some(&s.cle));
            true
        }
        decouverte::ChoixAuDemarrage::Proposer => {
            tracing::info!(trouves = trouves.len(), ancienne = %url, "liste_des_serveurs_proposee");
            ouvrir_choix_serveur(app);
            false
        }
    }
}

/// True if an HTTP server answers at `url` (any response, even an error status,
/// means the port is up).
async fn server_reachable(http: &reqwest::Client, url: &str) -> bool {
    http.get(url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

/// Option « Démarrer le serveur Tune s'il ne tourne pas » (fil 2868).
///
/// Désactivée par défaut. Ne fait rien si l'adresse configurée n'est pas une
/// adresse de bouclage (`serveur_local::adresse_locale`) ou si le serveur
/// répond déjà. Sinon lance le serveur installé sur cette machine (chemins
/// fixes ou lus dans la clé de l'installeur, arguments constants : voir
/// `serveur_local`) et attend jusqu'à ~30 s qu'il réponde.
///
/// Remplace le lancement d'un « sidecar » `tune-server` à côté du widget, que
/// rien n'installait (`externalBin` absent de tauri.conf.json) : il échouait à
/// chaque démarrage.
///
/// `montrer_mini` : pendant l'attente, montrer le mini-lecteur embarqué (au
/// lancement, pour qu'un démarrage lent ne ressemble pas à un lancement raté).
///
/// Rend `true` quand un serveur a été lancé.
async fn demarrer_serveur_si_absent(app: &tauri::AppHandle, montrer_mini: bool) -> bool {
    if !serveur_local::PRIS_EN_CHARGE || !lire_cfg_bool(serveur_local::CLE_CONFIG, false) {
        return false;
    }
    let http = app.state::<AppState>().http.clone();
    let url = app.state::<AppState>().server_url.read().await.clone();
    if !serveur_local::adresse_locale(&url) {
        tracing::info!(server_url = %url, "serveur_distant_rien_a_lancer");
        return false;
    }
    if server_reachable(&http, &url).await {
        return false;
    }
    lancer_le_serveur_local(app, &http, &url, montrer_mini).await
}

/// Lance le serveur installé sur cette machine et attend jusqu'à ~30 s qu'il
/// réponde à `url`. Commun à l'option de lancement et à l'entrée « Démarrer le
/// serveur » du menu (#9244). L'appelant a vérifié que `url` est locale.
async fn lancer_le_serveur_local(
    app: &tauri::AppHandle,
    http: &reqwest::Client,
    url: &str,
    montrer_mini: bool,
) -> bool {
    // `lancer` attend la fin de `open` sous macOS : hors du fil asynchrone.
    let lancement = tokio::task::spawn_blocking(serveur_local::lancer)
        .await
        .unwrap_or_else(|e| Err(format!("tâche de lancement interrompue : {e}")));
    match lancement {
        Ok(programme) => {
            tracing::info!(programme = %programme, "serveur_local_lance");
            if montrer_mini {
                if let Some(mini) = app.get_webview_window("main") {
                    let _ = mini.show();
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "serveur_local_non_lance");
            return false;
        }
    }
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if server_reachable(http, url).await {
            tracing::info!("serveur_local_pret");
            return true;
        }
    }
    tracing::warn!("serveur_local_delai_depasse");
    true
}

/// L'option est-elle active ? (réglage du mini-lecteur embarqué)
#[tauri::command]
///
/// `None` hors Windows et macOS : l'interface masque alors la case.
async fn get_demarrer_serveur_local() -> Result<Option<bool>, String> {
    Ok(serveur_local::PRIS_EN_CHARGE.then(|| lire_cfg_bool(serveur_local::CLE_CONFIG, false)))
}

/// Active ou désactive l'option, et tient la case du menu à jour.
#[tauri::command]
async fn set_demarrer_serveur_local(app: tauri::AppHandle, actif: bool) -> Result<(), String> {
    if !serveur_local::PRIS_EN_CHARGE {
        return Err("non pris en charge sur ce système".to_string());
    }
    ecrire_cfg_bool(serveur_local::CLE_CONFIG, actif);
    if let Some(item) = app.try_state::<CaseDemarrageServeur>() {
        let _ = item.0.set_checked(actif);
    }
    tracing::info!(actif, "option_demarrer_serveur_local");
    Ok(())
}

/// La case « Démarrer le serveur Tune s'il ne tourne pas » du menu de la zone
/// de notification, gardée pour la synchroniser avec le réglage du mini-lecteur.
struct CaseDemarrageServeur(CheckMenuItem<tauri::Wry>);

/// Les entrées « état du serveur » du menu de l'icône (#9244) : la ligne
/// d'état (inactive) et l'entrée Démarrer / Arrêter.
struct MenuServeur {
    etat: MenuItem<tauri::Wry>,
    action: MenuItem<tauri::Wry>,
    /// Une action est en cours : la surveillance ne réécrit pas le menu.
    occupe: std::sync::atomic::AtomicBool,
    /// Ce que le menu affiche, pour ne le réécrire qu'au changement.
    affiche: std::sync::Mutex<Option<(String, String, bool)>>,
}

/// Période de la surveillance de l'état du serveur.
const PERIODE_ETAT_SERVEUR: std::time::Duration = std::time::Duration::from_secs(5);

/// Photographie l'état du serveur configuré (`GET /system/version`, publique).
async fn instantane_serveur(app: &tauri::AppHandle) -> etat_serveur::Instantane {
    use etat_serveur::Etat;
    let (serveur, http) = {
        let state = app.state::<AppState>();
        let url = state.server_url.read().await.clone();
        (url, state.http.clone())
    };
    let local = serveur_local::adresse_locale(&serveur);
    let route = format!(
        "{}{}",
        serveur.trim_end_matches('/'),
        etat_serveur::ROUTE_VERSION
    );
    let etat = match http
        .get(&route)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => Etat::EnMarche {
            version: r
                .json::<serde_json::Value>()
                .await
                .ok()
                .as_ref()
                .and_then(etat_serveur::version_de),
        },
        // Le port répond (même par une erreur) : il tourne.
        Ok(_) => Etat::EnMarche { version: None },
        Err(_) => Etat::Arrete,
    };
    let processus_local = local
        && serveur_local::PRIS_EN_CHARGE
        && matches!(etat, Etat::EnMarche { .. })
        && tokio::task::spawn_blocking(etat_serveur::processus_serveur_present)
            .await
            .unwrap_or(false);
    etat_serveur::Instantane {
        serveur,
        local,
        etat,
        processus_local,
    }
}

/// Réécrit les deux entrées si ce qu'elles disent a changé.
fn afficher_menu_serveur(menu: &MenuServeur, ligne: String, action: &str, actif: bool) {
    let nouveau = (ligne, action.to_string(), actif);
    let mut affiche = menu.affiche.lock().unwrap_or_else(|e| e.into_inner());
    if affiche.as_ref() == Some(&nouveau) {
        return;
    }
    let _ = menu.etat.set_text(&nouveau.0);
    let _ = menu.action.set_text(&nouveau.1);
    let _ = menu.action.set_enabled(nouveau.2);
    *affiche = Some(nouveau);
}

/// Met le menu à jour d'après l'état réel du serveur.
async fn rafraichir_menu_serveur(app: &tauri::AppHandle) {
    let Some(menu) = app.try_state::<MenuServeur>() else {
        return;
    };
    if menu.occupe.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let i = instantane_serveur(app).await;
    let (texte, actif) = etat_serveur::libelle_action(etat_serveur::action_proposee(
        &i,
        serveur_local::PRIS_EN_CHARGE,
    ));
    afficher_menu_serveur(&menu, etat_serveur::libelle_etat(&i), texte, actif);
}

/// Surveille le serveur tant que Tune Client tourne.
async fn surveiller_serveur(app: tauri::AppHandle) {
    loop {
        rafraichir_menu_serveur(&app).await;
        tokio::time::sleep(PERIODE_ETAT_SERVEUR).await;
    }
}

/// Arrête le serveur LOCAL (#9244) : la route du serveur d'abord, le processus
/// si elle refuse. Rend ce qui a été fait, ou pourquoi rien ne l'a été.
async fn arreter_serveur_local(app: &tauri::AppHandle) -> Result<String, String> {
    // Revérifié ici, quel que soit l'état affiché : jamais un serveur distant.
    let i = instantane_serveur(app).await;
    if etat_serveur::action_proposee(&i, serveur_local::PRIS_EN_CHARGE)
        != etat_serveur::Action::Arreter
    {
        return Err(format!(
            "rien à arrêter ici ({})",
            etat_serveur::libelle_etat(&i)
        ));
    }
    let http = app.state::<AppState>().http.clone();
    let route = format!(
        "{}{}",
        i.serveur.trim_end_matches('/'),
        etat_serveur::ROUTE_ARRET
    );
    let statut = http
        .post(&route)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()
        .map(|r| r.status().as_u16());
    let fait = match etat_serveur::apres_route(statut) {
        etat_serveur::ApresRoute::Accepte => "arrêté par POST /system/stop".to_string(),
        etat_serveur::ApresRoute::ParLeProcessus => {
            tracing::info!(?statut, "arret_serveur_route_refusee_repli_processus");
            tokio::task::spawn_blocking(etat_serveur::arreter_par_le_processus)
                .await
                .unwrap_or_else(|e| Err(format!("tâche interrompue : {e}")))?
        }
    };
    // Attendre que le port se libère et, sous macOS, que le lanceur
    // `Tune Server.app` ait quitté (son `on idle` le fait dans les 5 s) : un
    // « Démarrer » pendant ce délai le trouverait encore là et n'ouvrirait rien.
    for _ in 0..30 {
        let port_libre = !server_reachable(&http, &i.serveur).await;
        let lanceur_parti = !tokio::task::spawn_blocking(etat_serveur::applet_presente)
            .await
            .unwrap_or(false);
        if port_libre && lanceur_parti {
            return Ok(fait);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(format!("{fait}, mais le serveur répond encore après 15 s"))
}

/// L'entrée Démarrer / Arrêter du menu de l'icône.
async fn executer_action_serveur(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    let Some(menu) = app.try_state::<MenuServeur>() else {
        return;
    };
    if menu.occupe.swap(true, Ordering::SeqCst) {
        return;
    }
    let i = instantane_serveur(&app).await;
    let ligne = etat_serveur::libelle_etat(&i);
    match etat_serveur::action_proposee(&i, serveur_local::PRIS_EN_CHARGE) {
        etat_serveur::Action::Demarrer => {
            afficher_menu_serveur(&menu, ligne, "Démarrage du serveur…", false);
            let http = app.state::<AppState>().http.clone();
            let pret = lancer_le_serveur_local(&app, &http, &i.serveur, false).await;
            tracing::info!(pret, "serveur_demarre_depuis_le_menu");
        }
        etat_serveur::Action::Arreter => {
            afficher_menu_serveur(&menu, ligne, "Arrêt du serveur…", false);
            match arreter_serveur_local(&app).await {
                Ok(fait) => tracing::info!(%fait, "serveur_arrete_depuis_le_menu"),
                Err(e) => tracing::warn!(error = %e, "serveur_non_arrete"),
            }
        }
        etat_serveur::Action::Aucune => {}
    }
    menu.occupe.store(false, Ordering::SeqCst);
    rafraichir_menu_serveur(&app).await;
}

/// La case « Ouvrir Tune Client à l'ouverture de session » : l'état réel du
/// système est relu après le réglage (#9244).
struct CaseOuvertureSession(CheckMenuItem<tauri::Wry>);

/// Montre la fenêtre du mode retenu (mini ou complète).
///
/// Appelée au lancement, par une seconde instance, et sur macOS par un
/// « reopen » : un lancement par l'utilisateur doit toujours montrer quelque
/// chose (#8934).
async fn montrer_le_widget(app: tauri::AppHandle) {
    // Une fenêtre déjà visible est seulement ramenée au premier plan : passer
    // par `set_full_mode` la rechargerait.
    for label in ["full", "main"] {
        if let Some(win) = app.get_webview_window(label) {
            if win.is_visible().unwrap_or(false) {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
                return;
            }
        }
    }
    set_full_mode(&app, saved_full_mode()).await;
}

/// Ouvre un journal sur disque, à côté de la configuration.
///
/// Sans lui, `tracing` était déclaré mais aucun abonné n'était installé : tous
/// les messages du programme partaient à la poubelle. Un échec au démarrage —
/// typiquement le WebView2 absent sous Windows, qui laisse le processus vivant
/// sans jamais ouvrir de fenêtre — ne laissait donc aucune trace, et le testeur
/// n'avait rien à envoyer (retour Sandro).
///
/// Renvoie le chemin du journal et la garde du writer, qui doit vivre aussi
/// longtemps que le programme sous peine de perdre les derniers messages.
fn init_logging() -> Option<(
    std::path::PathBuf,
    tracing_appender::non_blocking::WorkerGuard,
)> {
    let dir = dirs::config_dir()?.join("tune-widget");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("tune-widget.log");

    // Repart de zéro au-delà de 1 Mo : un widget qui tourne des semaines ne doit
    // pas remplir le disque, et seul le dernier démarrage sert au diagnostic.
    if std::fs::metadata(&path)
        .map(|m| m.len() > 1_000_000)
        .unwrap_or(false)
    {
        std::fs::remove_file(&path).ok();
    }

    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(&dir, "tune-widget.log"));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer)
        .with_max_level(tracing::Level::INFO)
        .init();

    Some((path, guard))
}

/// Dossiers de cache web à jeter quand la version du widget change.
///
/// L'interface du mini-lecteur est une page servie au moteur de rendu du
/// système ; celui-ci en garde une copie sur disque, que ni la désinstallation
/// ni la réinstallation n'emportaient. Une correction d'interface pouvait donc
/// rester invisible pour toujours — c'est ce qu'a vécu Sandro,
/// qui voyait le bouton ⤢ dans un bac à sable Windows vierge et pas sur sa
/// machine, même après désinstallation puis réinstallation (#1704).
///
/// On ne rend QUE le cache HTTP. Le reste du profil (`Local Storage` en
/// particulier) contient l'adresse du serveur choisie par l'utilisateur, que
/// `app.js` y range sous la clé `tune-server` : la purger le renverrait sur
/// l'adresse en dur du code, c'est-à-dire le réseau de quelqu'un d'autre.
fn webview_http_cache_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs_to_clear = Vec::new();

    // Windows. Aucun `dataDirectory` n'est déclaré dans tauri.conf.json et wry
    // ne pose rien à la place : WebView2 retombe donc sur son emplacement par
    // défaut, documenté par Microsoft comme « le chemin de l'exécutable suivi
    // de .WebView2 » — soit, ici, à l'intérieur même du dossier d'installation.
    // C'est aussi pourquoi le désinstalleur le laisse : il termine par un
    // `RMDir "$INSTDIR"` sans /r, qui échoue en silence sur un dossier non vide.
    // On le déduit de l'exécutable en cours plutôt que de recomposer un chemin
    // à partir du nom du produit : c'est la seule façon de rester juste si
    // l'installation a été déplacée ou renommée.
    #[cfg(target_os = "windows")]
    if let Ok(exe) = std::env::current_exe() {
        let mut udf = exe.into_os_string();
        udf.push(".WebView2");
        let ebwebview = std::path::PathBuf::from(udf).join("EBWebView");
        // Un profil par sous-dossier (« Default » sauf configuration
        // contraire) : on les balaie tous plutôt que de parier sur le nom.
        if let Ok(entries) = std::fs::read_dir(&ebwebview) {
            for entry in entries.flatten() {
                let profile = entry.path();
                if profile.is_dir() {
                    dirs_to_clear.push(profile.join("Cache"));
                    dirs_to_clear.push(profile.join("Code Cache"));
                }
            }
        }
    }

    // macOS. WKWebView range son cache disque sous ~/Library/Caches/<identifiant
    // du paquet>, à côté — et non à l'intérieur — des données de site, qui
    // vivent dans ~/Library/WebKit. Le piège y est donc le même que sous
    // Windows (rien n'est effacé en déplaçant l'application à la corbeille), et
    // le dossier ne contient que du cache : le vider ne coûte rien.
    #[cfg(target_os = "macos")]
    if let Some(caches) = dirs::cache_dir() {
        dirs_to_clear.push(caches.join("fr.mozaiklabs.tune-widget"));
    }

    dirs_to_clear
}

/// Le cache doit-il être jeté, au vu du témoin laissé par le dernier démarrage ?
///
/// Sans témoin, oui : c'est le cas de l'utilisateur qui installe cette version
/// par-dessus une ancienne, donc exactement celui qu'on cherche à rattraper.
fn cache_is_stale(seen: Option<&str>, current: &str) -> bool {
    seen.map(str::trim) != Some(current)
}

/// Jette le cache web dès que la version diffère de celle du dernier démarrage.
///
/// À appeler AVANT que Tauri ne construise la fenêtre : une fois le moteur de
/// rendu démarré, les fichiers du cache sont ouverts et ne peuvent plus être
/// supprimés.
///
/// Le témoin de version est rangé à côté de la configuration et du journal,
/// dans un dossier que ni la mise à jour ni la purge ne touchent — sinon la
/// purge se redéclencherait à chaque démarrage.
fn purge_webview_cache_on_version_change() {
    let Some(dir) = dirs::config_dir().map(|d| d.join("tune-widget")) else {
        return;
    };
    let stamp = dir.join("webview-cache-version");
    let current = env!("CARGO_PKG_VERSION");

    if !cache_is_stale(std::fs::read_to_string(&stamp).ok().as_deref(), current) {
        return;
    }

    for path in webview_http_cache_dirs() {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!(cache = %path.display(), "webview_cache_purged"),
            // Absent = rien à faire ; c'est le cas courant, pas une anomalie.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(cache = %path.display(), error = %e, "webview_cache_purge_failed")
            }
        }
    }

    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&stamp, current);
    }
}

/// Bascule mini ↔ complet depuis l'interface elle-même.
///
/// Le menu de la barre système la proposait déjà, mais il faut savoir qu'il
/// existe : le seul endroit où l'utilisateur regarde, c'est la fenêtre qu'il a
/// sous les yeux (Sandro).
#[tauri::command]
async fn set_display_mode(app: tauri::AppHandle, full: bool) -> Result<(), String> {
    set_full_mode(&app, full).await;
    Ok(())
}

/// Le client web servi par ce serveur affiche-t-il la vue mini ?
///
/// La réponse choisit entre une fenêtre et deux. C'est une vraie question posée
/// au serveur en marche, pas une supposition : Sandro pointe son widget vers un
/// GentooPlayer sur une autre machine, mis à jour à son rythme.
///
/// On ne sonde plus la VERSION du serveur. Le seuil 0.9.64 désignait
/// `MiniPlayer.svelte`, parti avec l'ancienne interface le 19/09/2026 : tout
/// serveur de 0.9.64 à la rc9 répondait « oui » et la fenêtre de 380 px
/// recevait l'interface complète. Le client web annonce désormais lui-même la
/// vue mini par `vue-mini.json` (voir `vue_mini::annonce_vue_mini`).
///
/// Tout ce qui n'est pas clair — injoignable, 404, `index.html` — répond
/// `false` et garde l'interface embarquée : le repli doit être ce qui marche.
async fn server_has_mini_layout(http: &reqwest::Client, url: &str) -> bool {
    let sonde = format!("{}/vue-mini.json", url.trim_end_matches('/'));
    let corps = match http
        .get(&sonde)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => {
            tracing::info!(url = %sonde, status = %r.status(), "mini_layout_probe_absent_keeping_two_windows");
            return false;
        }
        Err(e) => {
            tracing::info!(url = %sonde, error = %e, "mini_layout_probe_failed_keeping_two_windows");
            return false;
        }
    };
    let supported = vue_mini::annonce_vue_mini(&corps);
    tracing::info!(url = %sonde, supported, "mini_layout_probe");
    supported
}

/// Point the single window at one layout or the other.
///
/// Returns `false` when the unified path is not available, so the caller can
/// fall back to the two-window behaviour.
async fn navigate_unified(app: &tauri::AppHandle, full: bool) -> bool {
    let (url, http) = {
        let state = app.state::<AppState>();
        let url = state.server_url.read().await.clone();
        (url, state.http.clone())
    };
    if !server_has_mini_layout(&http, &url).await {
        return false;
    }
    let target = if full {
        vue_mini::adresse_complete(&url)
    } else {
        vue_mini::adresse_mini(&url)
    };
    let parsed = match target.parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(url = %target, error = %e, "unified_bad_url");
            return false;
        }
    };
    // Reuse the "full" window as THE window: it already exists with the right
    // close-to-hide behaviour, and it is the one that talks to the server.
    open_full_window(app).await;
    let Some(win) = app.get_webview_window("full") else {
        return false;
    };
    if let Err(e) = win.navigate(parsed) {
        tracing::warn!(url = %target, error = %e, "unified_navigate_failed");
        return false;
    }
    // The two layouts want very different windows. Mini stays on top and out of
    // the taskbar, like the widget it replaces; full behaves like an app.
    if full {
        // Meme borne que a l'ouverture : sans elle, repasser en grand rendait
        // a la fenetre les 800 points de haut qui ne tiennent pas sur un
        // bureau de 720 (#1598).
        let (work_area, scale) = active_work_area(app);
        let (w, h) = fit_to_work_area(work_area, scale, FULL_WINDOW_SIZE, FULL_WINDOW_MIN_SIZE);
        let _ = win.set_max_size(None::<tauri::Size>);
        let _ = win.set_min_size(Some(tauri::LogicalSize::new(
            FULL_WINDOW_MIN_SIZE.0,
            FULL_WINDOW_MIN_SIZE.1,
        )));
        let _ = win.set_size(tauri::LogicalSize::new(w, h));
        let _ = win.set_always_on_top(false);
        autoriser_plein_ecran(&win, true);
    } else {
        // Le mini-lecteur ne va pas en plein écran ; s'il y était (fenêtre
        // complète en plein écran, puis ⌘⇧M), il en sort d'abord.
        if win.is_fullscreen().unwrap_or(false) {
            let _ = win.set_fullscreen(false);
        }
        autoriser_plein_ecran(&win, false);
        // 🔴 La borne MINIMALE d'abord : la fenêtre est construite avec celle
        // de la fenêtre complète (900 × 600), et `set_size` ne descend pas
        // en dessous. La fenêtre « mini » faisait donc 900 × 600.
        let _ = win.set_min_size(Some(tauri::LogicalSize::new(
            vue_mini::MINI_MIN.0,
            vue_mini::MINI_MIN.1,
        )));
        let _ = win.set_max_size(Some(tauri::LogicalSize::new(
            vue_mini::MINI_LARGEUR_MAX,
            vue_mini::MINI_HAUTEUR_MAX,
        )));
        let (w, h) = taille_mini(lire_cfg_bool("mini_replie", false));
        let _ = win.set_size(tauri::LogicalSize::new(w, h));
        let _ = win.set_always_on_top(lire_cfg_bool("mini_epingle", true));
    }
    let _ = win.set_resizable(true);
    let _ = win.set_skip_taskbar(!full);
    let _ = win.set_decorations(true);
    let _ = win.show();
    let _ = win.set_focus();
    // The bundled mini window has no reason to exist in this mode.
    if let Some(mini) = app.get_webview_window("main") {
        let _ = mini.hide();
    }
    tracing::info!(full, "unified_window_mode");
    true
}

/// Switch between the two modes without ever reloading either one.
///
/// Both windows stay alive: showing one hides the other. That is the whole
/// point of keeping two windows at this stage — morphing a single one would
/// mean navigating between the bundled mini interface and the server's URL,
/// and every toggle would cost a reload and lose scroll, search and view.
/// (Once the mini player becomes a layout of the web client, one window that
/// resizes becomes the better design — that is step 3, not this one.)
async fn set_full_mode(app: &tauri::AppHandle, full: bool) {
    // Step 4: one window that switches layout, when the server can serve both.
    // Falls back to the two-window design below on an older server.
    if navigate_unified(app, full).await {
        remember_mode(full);
        return;
    }
    if full {
        open_full_window(app).await;
        if let Some(mini) = app.get_webview_window("main") {
            let _ = mini.hide();
        }
    } else {
        if let Some(win) = app.get_webview_window("full") {
            let _ = win.hide();
        }
        if let Some(mini) = app.get_webview_window("main") {
            let _ = mini.show();
            let _ = mini.set_focus();
        }
    }
    remember_mode(full);
}

/// Which window a tray left-click should act on: the one the user last chose.
fn full_mode_active(app: &tauri::AppHandle) -> bool {
    app.get_webview_window("full")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

fn config_file() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("tune-widget").join("config.json"))
}

/// Lit un booléen de `config.json` (`mini_epingle`, `mini_replie`…).
fn lire_cfg_bool(cle: &str, defaut: bool) -> bool {
    config_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        .and_then(|c| c[cle].as_bool())
        .unwrap_or(defaut)
}

/// Écrit un booléen dans `config.json` en FUSIONNANT : `server_url` et
/// `full_mode` vivent dans le même fichier.
fn ecrire_cfg_bool(cle: &str, valeur: bool) {
    let Some(file) = config_file() else {
        return;
    };
    let mut cfg: serde_json::Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    cfg[cle] = serde_json::json!(valeur);
    if let Some(dir) = file.parent() {
        if std::fs::create_dir_all(dir).is_ok() {
            let _ = std::fs::write(&file, cfg.to_string());
        }
    }
}

/// Taille de la fenêtre mini selon qu'elle est repliée (maquette C) ou non.
fn taille_mini(replie: bool) -> (f64, f64) {
    if replie {
        vue_mini::MINI_REPLIE
    } else {
        vue_mini::MINI_DEPLIE
    }
}

/// Exécute un geste natif demandé par la page (`/__widget/<action>`).
///
/// La page ne parle à la fenêtre QUE par ces quatre adresses, que
/// `vue_mini::decider_navigation` n'accepte que depuis le serveur configuré.
/// Aucune commande Tauri n'est ouverte à la page distante.
async fn executer_action(app: tauri::AppHandle, action: vue_mini::ActionWidget) {
    use vue_mini::ActionWidget;
    if let ActionWidget::Zone(id) = action {
        *app.state::<AppState>().active_zone_id.write().await = Some(id);
        tracing::info!(zone_id = id, "zone_visee_signalee");
        return;
    }
    let Some(win) = app.get_webview_window("full") else {
        return;
    };
    match action {
        ActionWidget::Epingle => {
            let epingle = !lire_cfg_bool("mini_epingle", true);
            let _ = win.set_always_on_top(epingle);
            ecrire_cfg_bool("mini_epingle", epingle);
            tracing::info!(epingle, "mini_epingle");
        }
        ActionWidget::Replier | ActionWidget::Deplier => {
            let replie = matches!(action, ActionWidget::Replier);
            let (w, h) = taille_mini(replie);
            let _ = win.set_size(tauri::LogicalSize::new(w, h));
            ecrire_cfg_bool("mini_replie", replie);
            tracing::info!(replie, "mini_replie");
        }
        ActionWidget::Complet => set_full_mode(&app, true).await,
        // Déjà traitée plus haut, sans fenêtre.
        ActionWidget::Zone(_) => {}
    }
}

/// Décide d'une navigation de la fenêtre qui charge le serveur, et exécute ce
/// qui doit l'être. `true` = le moteur charge l'adresse.
fn garder_navigation(app: &tauri::AppHandle, cible: &tauri::Url) -> bool {
    // Lecture non bloquante : on est sur le fil de l'interface. Le verrou n'est
    // tenu qu'un instant, le temps d'écrire une adresse ; s'il l'est, on
    // refuse plutôt que de charger sans savoir ce qu'est le serveur.
    let serveur = match app.state::<AppState>().server_url.try_read() {
        Ok(s) => s.clone(),
        Err(_) => return false,
    };
    match vue_mini::decider_navigation(&serveur, cible) {
        vue_mini::Decision::Autoriser => true,
        vue_mini::Decision::Action(action) => {
            let app = app.clone();
            tauri::async_runtime::spawn(executer_action(app, action));
            false
        }
        vue_mini::Decision::Externe => {
            ouvrir_dans_le_navigateur(app, cible);
            false
        }
        vue_mini::Decision::Refuser => {
            tracing::warn!(url = %cible, "navigation_refusee");
            false
        }
    }
}

/// Confie un lien http(s) au navigateur du système.
///
/// Le schéma est revérifié ici, quel que soit l'appelant : seul `http`/`https`
/// part (`vue_mini::ouvrable_dans_le_navigateur`). Appel côté Rust : la page
/// distante ne reçoit AUCUNE permission du greffon `opener`.
fn ouvrir_dans_le_navigateur(app: &tauri::AppHandle, cible: &tauri::Url) {
    use tauri_plugin_opener::OpenerExt;
    if !vue_mini::ouvrable_dans_le_navigateur(cible) {
        tracing::warn!(url = %cible, "lien_externe_refuse");
        return;
    }
    if let Err(e) = app.opener().open_url(cible.as_str(), None::<&str>) {
        tracing::warn!(url = %cible, error = %e, "lien_externe_non_ouvert");
    }
}

/// Une demande de nouvelle fenêtre (`target="_blank"`, `window.open`) de la
/// fenêtre qui charge le serveur (web#3142). Jamais de fenêtre du widget : le
/// lien part dans le navigateur du système, ou le geste `/__widget/…` est
/// exécuté.
fn garder_nouvelle_fenetre(
    app: &tauri::AppHandle,
    cible: &tauri::Url,
) -> tauri::webview::NewWindowResponse<tauri::Wry> {
    let serveur = match app.state::<AppState>().server_url.try_read() {
        Ok(s) => s.clone(),
        Err(_) => return tauri::webview::NewWindowResponse::Deny,
    };
    match vue_mini::decider_nouvelle_fenetre(&serveur, cible) {
        vue_mini::Decision::Externe => ouvrir_dans_le_navigateur(app, cible),
        vue_mini::Decision::Action(action) => {
            tauri::async_runtime::spawn(executer_action(app.clone(), action));
        }
        vue_mini::Decision::Autoriser | vue_mini::Decision::Refuser => {
            tracing::warn!(url = %cible, "nouvelle_fenetre_refusee");
        }
    }
    tauri::webview::NewWindowResponse::Deny
}

/// Persist the chosen mode next to the server URL, so a restart comes back the
/// way it was left rather than always in mini.
fn remember_mode(full: bool) {
    let Some(dir) = dirs::config_dir() else {
        return;
    };
    let dir = dir.join("tune-widget");
    let file = dir.join("config.json");
    let mut cfg: serde_json::Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    cfg["full_mode"] = serde_json::json!(full);
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&file, cfg.to_string());
    }
}

fn saved_full_mode() -> bool {
    dirs::config_dir()
        .map(|d| d.join("tune-widget").join("config.json"))
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        .and_then(|c| c["full_mode"].as_bool())
        .unwrap_or(false)
}

/// Taille souhaitee de la grande fenetre, en points logiques.
const FULL_WINDOW_SIZE: (f64, f64) = (1200.0, 800.0);
/// Taille en deca de laquelle la grande fenetre ne descend pas.
const FULL_WINDOW_MIN_SIZE: (f64, f64) = (900.0, 600.0);
/// Marge laissee autour de la fenetre dans la zone de travail de l'ecran.
const WORK_AREA_MARGIN: f64 = 48.0;

/// Ramener une taille voulue a ce que l'ecran peut reellement afficher.
///
/// La grande fenetre demandait 1200x800 points quoi qu'il arrive. Sur un
/// 1920x1080 regle a 150 % — la mise a l'echelle Windows la plus repandue —
/// le bureau n'offre que 1280x720 points : les 800 de hauteur debordent et le
/// bas de l'interface passe sous le bord de l'ecran. C'est la seconde moitie
/// de #1598, celle que le zoom seul ne reglait pas.
///
/// `work_area` arrive en pixels physiques alors que `inner_size` et `set_size`
/// parlent en points logiques : d'ou la division par le facteur d'echelle.
///
/// La taille minimale de la fenetre l'emporte sur la contrainte d'ecran : sur
/// un ecran plus petit qu'elle, mieux vaut une fenetre deplacable qu'une
/// fenetre ecrasee.
fn fit_to_work_area(
    work_area: Option<(u32, u32)>,
    scale: f64,
    want: (f64, f64),
    min: (f64, f64),
) -> (f64, f64) {
    let Some((width, height)) = work_area else {
        return want;
    };
    if !scale.is_finite() || scale <= 0.0 {
        return want;
    }
    let available_w = f64::from(width) / scale - WORK_AREA_MARGIN;
    let available_h = f64::from(height) / scale - WORK_AREA_MARGIN;
    (
        want.0.min(available_w.max(min.0)),
        want.1.min(available_h.max(min.1)),
    )
}

/// Zone de travail de l'ecran ou la fenetre va s'ouvrir, a defaut l'ecran
/// principal. `None` quand aucun ecran n'est identifiable : on garde alors la
/// taille voulue plutot que d'inventer une contrainte.
fn active_work_area(app: &tauri::AppHandle) -> (Option<(u32, u32)>, f64) {
    let monitor = app
        .get_webview_window("full")
        .or_else(|| app.get_webview_window("main"))
        .and_then(|win| win.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    match monitor {
        Some(monitor) => {
            let area = monitor.work_area();
            (
                Some((area.size.width, area.size.height)),
                monitor.scale_factor(),
            )
        }
        None => (None, 1.0),
    }
}

/// Bits de `NSWindowCollectionBehavior` qui règlent le plein écran.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const NS_PLEIN_ECRAN_PRINCIPAL: usize = 1 << 7; // FullScreenPrimary
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const NS_PLEIN_ECRAN_AUXILIAIRE: usize = 1 << 8; // FullScreenAuxiliary
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const NS_PLEIN_ECRAN_AUCUN: usize = 1 << 9; // FullScreenNone

/// Le `collectionBehavior` d'une fenêtre, plein écran permis ou non : les
/// autres bits (Spaces, Exposé…) sont gardés tels quels.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn comportement_plein_ecran(actuel: usize, permis: bool) -> usize {
    let base =
        actuel & !(NS_PLEIN_ECRAN_PRINCIPAL | NS_PLEIN_ECRAN_AUXILIAIRE | NS_PLEIN_ECRAN_AUCUN);
    if permis {
        base | NS_PLEIN_ECRAN_PRINCIPAL
    } else {
        base | NS_PLEIN_ECRAN_AUCUN
    }
}

/// macOS : le bouton vert de la fenêtre complète donne le VRAI plein écran
/// (#9255, fil 2968 : « on voit encore les bords ou les hauts de la fenêtre »).
///
/// Tauri ne pose aucun comportement de plein écran : macOS décide seul, et la
/// même fenêtre, née mini-lecteur (bornée à 760 points, épinglée au-dessus des
/// autres, dans une app « accessoire »), n'y avait pas droit — le bouton vert
/// ne faisait qu'agrandir. On le déclare donc explicitement : `FullScreenPrimary`
/// pour la fenêtre complète, `FullScreenNone` pour le mini-lecteur. Le menu
/// Présentation › Plein écran (⌃⌘F) passe par le même chemin.
fn autoriser_plein_ecran(win: &tauri::WebviewWindow, permis: bool) {
    #[cfg(target_os = "macos")]
    {
        let w = win.clone();
        // `ns_window` et AppKit : sur le fil principal seulement.
        let _ = win.run_on_main_thread(move || {
            let Ok(ptr) = w.ns_window() else {
                return;
            };
            if ptr.is_null() {
                return;
            }
            // SAFETY : `ptr` est le NSWindow vivant de cette fenêtre, lu sur
            // le fil principal ; `collectionBehavior` est un NSUInteger.
            unsafe {
                let fenetre = &*(ptr as *mut objc2::runtime::AnyObject);
                let actuel: usize = objc2::msg_send![fenetre, collectionBehavior];
                let suivant = comportement_plein_ecran(actuel, permis);
                let _: () = objc2::msg_send![fenetre, setCollectionBehavior: suivant];
            }
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (win, permis);
}

/// La barre des menus de macOS (#9255) : les menus d'une app ordinaire, plus
/// Lecture, Aller et Présentation, dont les raccourcis remplacent les
/// raccourcis GLOBAUX d'avant (voir `raccourcis`). Aucun ne demande
/// d'autorisation : ils ne marchent que quand l'app est au premier plan.
#[cfg(target_os = "macos")]
fn barre_des_menus(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    use tauri::menu::{AboutMetadata, PredefinedMenuItem, Submenu};

    let entree =
        |e: &raccourcis::Entree| MenuItem::with_id(app, e.id, e.libelle, true, e.accelerateur);
    let remplir = |sous: &Submenu<tauri::Wry>, menu: raccourcis::Menu| -> tauri::Result<()> {
        for e in raccourcis::entrees_de(menu) {
            sous.append(&entree(e)?)?;
        }
        Ok(())
    };
    let sep = || PredefinedMenuItem::separator(app);

    let app_menu = Submenu::new(app, "Tune Client", true)?;
    app_menu.append(&PredefinedMenuItem::about(
        app,
        Some("À propos de Tune Client"),
        Some(AboutMetadata::default()),
    )?)?;
    app_menu.append(&sep()?)?;
    remplir(&app_menu, raccourcis::Menu::App)?;
    app_menu.append(&sep()?)?;
    app_menu.append(&PredefinedMenuItem::services(app, Some("Services"))?)?;
    app_menu.append(&sep()?)?;
    app_menu.append(&PredefinedMenuItem::hide(app, Some("Masquer Tune Client"))?)?;
    app_menu.append(&PredefinedMenuItem::hide_others(
        app,
        Some("Masquer les autres"),
    )?)?;
    app_menu.append(&PredefinedMenuItem::show_all(app, Some("Tout afficher"))?)?;
    app_menu.append(&sep()?)?;
    app_menu.append(&PredefinedMenuItem::quit(app, Some("Quitter Tune Client"))?)?;

    // Sans menu Édition, ⌘C / ⌘V / ⌘A ne marchent pas dans une page web
    // sous macOS : ce sont des équivalents de menu, pas des touches de la page.
    let edition = Submenu::with_items(
        app,
        "Édition",
        true,
        &[
            &PredefinedMenuItem::undo(app, Some("Annuler"))?,
            &PredefinedMenuItem::redo(app, Some("Rétablir"))?,
            &sep()?,
            &PredefinedMenuItem::cut(app, Some("Couper"))?,
            &PredefinedMenuItem::copy(app, Some("Copier"))?,
            &PredefinedMenuItem::paste(app, Some("Coller"))?,
            &PredefinedMenuItem::select_all(app, Some("Tout sélectionner"))?,
        ],
    )?;

    let lecture = Submenu::new(app, "Lecture", true)?;
    remplir(&lecture, raccourcis::Menu::Lecture)?;
    let aller = Submenu::new(app, "Aller", true)?;
    remplir(&aller, raccourcis::Menu::Aller)?;
    let presentation = Submenu::new(app, "Présentation", true)?;
    remplir(&presentation, raccourcis::Menu::Presentation)?;
    presentation.append(&sep()?)?;
    presentation.append(&PredefinedMenuItem::fullscreen(app, Some("Plein écran"))?)?;

    let fenetre = Submenu::with_items(
        app,
        "Fenêtre",
        true,
        &[
            &PredefinedMenuItem::minimize(app, Some("Placer dans le Dock"))?,
            &PredefinedMenuItem::maximize(app, Some("Agrandir"))?,
            &sep()?,
            &PredefinedMenuItem::close_window(app, Some("Fermer la fenêtre"))?,
        ],
    )?;

    Menu::with_items(
        app,
        &[
            &app_menu,
            &edition,
            &lecture,
            &aller,
            &presentation,
            &fenetre,
        ],
    )
}

/// Open (or focus) the full Tune interface in a real application window.
///
/// The interface is NOT bundled: the window loads the server's own web UI over
/// http. That is deliberate — bundling would mean republishing the app on every
/// web change and, worse, shipping a UI that can drift from the API of the
/// server it drives. Sandro points his widget at a GentooPlayer on another
/// machine, so that drift is a certainty, not a risk.
///
/// It also replaces what this menu entry used to do: hand the URL to the
/// system browser, hardcoded to `localhost:8888` — the wrong machine entirely
/// for anyone whose server is not the desktop.
async fn open_full_window(app: &tauri::AppHandle) {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    if let Some(win) = app.get_webview_window("full") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }

    let url = {
        let state = app.state::<AppState>();
        let guard = state.server_url.read().await;
        guard.clone()
    };
    let parsed = match url.parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "full_window_bad_server_url");
            return;
        }
    };

    let (work_area, scale) = active_work_area(app);
    let (width, height) =
        fit_to_work_area(work_area, scale, FULL_WINDOW_SIZE, FULL_WINDOW_MIN_SIZE);

    match WebviewWindowBuilder::new(app, "full", WebviewUrl::External(parsed))
        .title("Tune Client")
        .inner_size(width, height)
        .min_inner_size(FULL_WINDOW_MIN_SIZE.0, FULL_WINDOW_MIN_SIZE.1)
        .resizable(true)
        .decorations(true)
        // Ctrl/Cmd + molette et Ctrl/Cmd + « - = + ». Sans cet appel, wry passe
        // `false` a `ICoreWebView2Settings::IsZoomControlEnabled` : sous Windows
        // la molette est morte, exactement ce que decrit #1598. Sur macOS et
        // Linux, c'est ce meme drapeau qui fait injecter par Tauri le script
        // equivalent (`plugin:webview|set_webview_zoom`), d'ou la capacite
        // `full-window` qui autorise cette commande sur cette fenetre.
        .zoom_hotkeys_enabled(true)
        // La fenêtre charge le serveur configuré, et RIEN d'autre : les liens
        // externes partent dans le navigateur, les gestes `/__widget/…` sont
        // exécutés sans être chargés (voir `vue_mini::decider_navigation`).
        .on_navigation({
            let app = app.clone();
            move |cible| garder_navigation(&app, cible)
        })
        // `target="_blank"` et `window.open` : sans ce gestionnaire, WebView2
        // ne faisait rien du tout (« Site web de la station », web#3142).
        .on_new_window({
            let app = app.clone();
            move |cible, _| garder_nouvelle_fenetre(&app, &cible)
        })
        .build()
    {
        Ok(win) => {
            autoriser_plein_ecran(&win, true);
            // Closing hides instead of destroying: the web interface stays
            // loaded, so coming back is instant and nothing is lost. Quitting
            // is the tray menu's job, as it already was.
            let handle = win.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = handle.hide();
                }
            });
            tracing::info!(url = %url, "full_window_opened");
        }
        Err(e) => tracing::error!(url = %url, error = %e, "full_window_open_failed"),
    }
}

fn main() {
    // En tout premier : ce qui échoue ensuite doit laisser une trace.
    // La garde est conservée jusqu'à la fin de main, sinon les derniers
    // messages seraient perdus à l'extinction.
    let _log_guard = init_logging().map(|(path, guard)| {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            journal = %path.display(),
            "tune_client_starting"
        );
        guard
    });

    // Avant toute construction de fenêtre : le moteur de rendu verrouille les
    // fichiers de son cache dès qu'il démarre.
    purge_webview_cache_on_version_change();

    let builder = tauri::Builder::default()
        // Instance unique (#8934), déclarée EN PREMIER comme le demande le
        // greffon : un second lancement (raccourci du Bureau, menu Démarrer)
        // ne crée pas un second widget, il ramène la fenêtre de celui-ci.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tracing::info!("seconde_instance_ramene_la_fenetre");
            tauri::async_runtime::spawn(montrer_le_widget(app.clone()));
        }))
        // Ouvre les liens http(s) dans le navigateur du système, appelé côté
        // Rust seulement : aucune fenêtre ne reçoit ses permissions.
        .plugin(tauri_plugin_opener::init());
    // Windows seulement : les raccourcis globaux et les touches média (#8531).
    #[cfg(windows)]
    let builder = builder.plugin(raccourcis_globaux::greffon());
    let run = builder
        .manage({
            let mut saved_url = "http://localhost:8888".to_string();
            if let Some(dir) = dirs::config_dir() {
                let cfg_file = dir.join("tune-widget").join("config.json");
                if let Ok(data) = std::fs::read_to_string(&cfg_file) {
                    if let Ok(cfg) = serde_json::from_str::<serde_json::Value>(&data) {
                        if let Some(url) = cfg["server_url"].as_str() {
                            saved_url = url.to_string();
                        }
                    }
                }
            }
            // L'URL retenue explique à elle seule le cas « le widget a lancé un
            // serveur local » : demarrer_serveur_si_absent ne lance le serveur
            // que si l'option est active, cette adresse locale et muette.
            tracing::info!(server_url = %saved_url, "server_url_resolved");
            AppState {
                server_url: RwLock::new(saved_url),
                choix_demande: std::sync::atomic::AtomicBool::new(false),
                active_zone_id: RwLock::new(None),
                http: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap(),
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_zones,
            play_pause,
            next_track,
            prev_track,
            set_volume,
            select_zone,
            search,
            play_item,
            set_server_url,
            get_server_url,
            http_get,
            http_post,
            http_put,
            set_display_mode,
            get_demarrer_serveur_local,
            set_demarrer_serveur_local,
            decouvrir_serveurs,
            choisir_serveur,
            choix_serveur_demande,
        ])
        // macOS : la barre des menus et ses raccourcis (#9255). Ailleurs, pas
        // de barre de menus dans la fenêtre : le menu de la zone de
        // notification reste l'entrée.
        .on_menu_event(|app, event| {
            if let Some(action) = raccourcis::action_de(event.id().as_ref()) {
                tauri::async_runtime::spawn(executer_action_menu(app.clone(), action));
            }
        })
        .setup(|app| {
            // Une vraie app macOS (#9255, fil 2968) : une entrée dans le Dock
            // et la barre des menus, au lieu de l'app « accessoire » cachée
            // derrière son icône de la barre de menus. Le clic sur l'icône du
            // Dock ramène la fenêtre (`RunEvent::Reopen`, plus bas).
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Regular);
                let barre = barre_des_menus(app.handle())?;
                app.set_menu(barre)?;
            }

            // Tray menu
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let open_web =
                MenuItem::with_id(app, "open_web", "Fenêtre complète", true, None::<&str>)?;
            let open_mini =
                MenuItem::with_id(app, "open_mini", "Mini-lecteur", true, None::<&str>)?;
            // La case n'existe que là où le widget sait lancer le serveur
            // (Windows, macOS) : sous Linux, c'est un service système.
            let choisir = MenuItem::with_id(
                app,
                "choisir_serveur",
                "Choisir un serveur…",
                true,
                None::<&str>,
            )?;
            // L'état du serveur et son arrêt / démarrage (#9244, fil 2966) :
            // pas d'icône propre à Tune Server, c'est celle-ci qui le montre.
            // Textes provisoires, remplacés par la surveillance en ~2 s.
            let etat_serveur_item =
                MenuItem::with_id(app, "etat_serveur", "Serveur : …", false, None::<&str>)?;
            let action_serveur = MenuItem::with_id(
                app,
                "action_serveur",
                "Arrêter le serveur",
                false,
                None::<&str>,
            )?;
            let menu = Menu::with_items(
                app,
                &[
                    &etat_serveur_item,
                    &action_serveur,
                    &tauri::menu::PredefinedMenuItem::separator(app)?,
                    &open_web,
                    &open_mini,
                    &choisir,
                ],
            )?;
            app.manage(MenuServeur {
                etat: etat_serveur_item,
                action: action_serveur,
                occupe: std::sync::atomic::AtomicBool::new(false),
                affiche: std::sync::Mutex::new(None),
            });
            if ouverture_session::PRIS_EN_CHARGE {
                let ouverture = CheckMenuItem::with_id(
                    app,
                    "ouverture_session",
                    "Ouvrir Tune Client à l'ouverture de session",
                    true,
                    ouverture_session::active(),
                    None::<&str>,
                )?;
                menu.append(&ouverture)?;
                app.manage(CaseOuvertureSession(ouverture));
            }
            if serveur_local::PRIS_EN_CHARGE {
                let demarrer_serveur = CheckMenuItem::with_id(
                    app,
                    "demarrer_serveur",
                    "Démarrer le serveur Tune s'il ne tourne pas",
                    true,
                    lire_cfg_bool(serveur_local::CLE_CONFIG, false),
                    None::<&str>,
                )?;
                menu.append(&demarrer_serveur)?;
                app.manage(CaseDemarrageServeur(demarrer_serveur));
            }
            menu.append(&quit)?;

            // macOS : une « image modèle » (noir + transparence) que le
            // système peint en blanc ou en noir selon le mode clair/sombre,
            // comme toutes les icônes de la barre des menus (#9255). Ailleurs,
            // l'icône en couleur : Windows ne repeint rien.
            #[cfg(target_os = "macos")]
            let (tray_icon_bytes, modele): (&[u8], bool) =
                (include_bytes!("../icons/tray-template.png"), true);
            #[cfg(not(target_os = "macos"))]
            let (tray_icon_bytes, modele): (&[u8], bool) =
                (include_bytes!("../icons/tray-icon.png"), false);
            let tray_img = tauri::image::Image::from_bytes(tray_icon_bytes).expect("tray icon");

            let _tray = TrayIconBuilder::new()
                .icon(tray_img)
                .icon_as_template(modele)
                .tooltip("Tune Client")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    "open_web" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            demarrer_serveur_si_absent(&app, false).await;
                            set_full_mode(&app, true).await;
                        });
                    }
                    "demarrer_serveur" => {
                        // La case a déjà basculé quand l'évènement arrive.
                        if let Some(case) = app.try_state::<CaseDemarrageServeur>() {
                            let actif = case.0.is_checked().unwrap_or(false);
                            ecrire_cfg_bool(serveur_local::CLE_CONFIG, actif);
                            tracing::info!(actif, "option_demarrer_serveur_local");
                        }
                    }
                    "action_serveur" => {
                        tauri::async_runtime::spawn(executer_action_serveur(app.clone()));
                    }
                    "ouverture_session" => {
                        if let Some(case) = app.try_state::<CaseOuvertureSession>() {
                            // La case a déjà basculé ; on la recale ensuite sur
                            // ce que le système a réellement retenu.
                            let voulu = case.0.is_checked().unwrap_or(false);
                            if let Err(e) = ouverture_session::regler(voulu) {
                                tracing::warn!(voulu, error = %e, "ouverture_session_non_reglee");
                            }
                            let reel = ouverture_session::active();
                            let _ = case.0.set_checked(reel);
                            tracing::info!(voulu, reel, "ouverture_session");
                        }
                    }
                    "open_mini" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            set_full_mode(&app, false).await;
                        });
                    }
                    // « choisir_serveur » : traité UNE fois, par le gestionnaire
                    // de l'app (`Builder::on_menu_event`), qui reçoit aussi les
                    // évènements de ce menu.
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // `button_state` matters: a tray Click fires TWICE per
                    // physical click — once on press, once on release. Matching
                    // both ran this handler twice, so the window was shown and
                    // hidden again within the same click and nothing appeared.
                    // Clicking repeatedly "worked" only when the timing left it
                    // on the visible half (Sandro, 9 Aug 2026: "je dois cliquer
                    // de façon répétée et frénétique"). Acting on release only
                    // gives exactly one toggle per click.
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        // Act on the window of the CURRENT mode: someone who
                        // chose the full window expects the tray icon to bring
                        // that back, not the mini player they left behind.
                        let app = tray.app_handle();
                        let label = if full_mode_active(app) {
                            "full"
                        } else {
                            "main"
                        };
                        let visible = app
                            .get_webview_window(label)
                            .and_then(|w| w.is_visible().ok())
                            .unwrap_or(false);
                        if visible {
                            if let Some(win) = app.get_webview_window(label) {
                                let _ = win.hide();
                            }
                        } else {
                            // Showing goes through set_full_mode rather than
                            // straight to the window: that is where the server
                            // is asked whether it can serve both layouts. A
                            // widget started before its server was up therefore
                            // still lands on the unified window at the first
                            // click, instead of being stuck on the bundled mini
                            // until the next toggle.
                            let h = app.clone();
                            let full = saved_full_mode();
                            tauri::async_runtime::spawn(async move {
                                set_full_mode(&h, full).await;
                            });
                        }
                    }
                })
                .build(app)?;

            #[cfg(windows)]
            raccourcis_globaux::enregistrer(app.handle());

            // #8934 — un lancement par l'utilisateur MONTRE le widget, dans le
            // mode qu'il avait laissé. Avant, seule l'icône de la zone de
            // notification apparaissait (souvent rangée derrière « ^ » sous
            // Windows) : « quand je clique dessus il ne se passe rien ».
            //
            // Si l'option le demande, le serveur local est lancé d'abord, pour
            // que la fenêtre s'ouvre sur un serveur qui répond ; le
            // mini-lecteur embarqué est montré pendant l'attente, pour qu'un
            // démarrage lent ne ressemble pas à un lancement raté.
            // L'état du serveur dans le menu, tenu à jour tant que Tune
            // Client tourne (#9244).
            tauri::async_runtime::spawn(surveiller_serveur(app.handle().clone()));

            // #9244 — ouvert par la session (`--session`) : pas de fenêtre, ni
            // liste des serveurs ; seulement l'icône, et le serveur local si
            // l'option le demande. Le clic sur l'icône (ou sur le Dock) montre
            // la fenêtre comme d'habitude.
            if ouverture_session::lance_par_la_session(std::env::args()) {
                tracing::info!("ouvert_par_la_session_sans_fenetre");
                let h = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    demarrer_serveur_si_absent(&h, false).await;
                    rafraichir_menu_serveur(&h).await;
                });
                return Ok(());
            }

            let h = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                demarrer_serveur_si_absent(&h, true).await;
                // Serveur muet : on cherche sur le réseau (#9255). Quand la
                // liste des serveurs est montrée, on la laisse au premier plan.
                if !assurer_un_serveur(&h).await {
                    return;
                }
                // PAS `montrer_le_widget` : si le serveur vient d'être lancé,
                // le mini-lecteur embarqué est visible pendant l'attente, et
                // il faut maintenant basculer sur le mode retenu (vue unifiée
                // servie par le serveur), pas seulement lui donner le focus.
                set_full_mode(&h, saved_full_mode()).await;
            });

            Ok(())
        })
        .build(tauri::generate_context!());

    // Un échec ici est le cas qu'on cherchait justement à instrumenter : sous
    // Windows, une fenêtre qui ne s'ouvre pas vient presque toujours du runtime
    // WebView2 absent. `expect` paniquait vers une sortie d'erreur qu'une
    // application graphique n'affiche à personne.
    let app = match run {
        Ok(app) => app,
        Err(e) => {
            tracing::error!(
                error = %e,
                "widget_run_failed : impossible de démarrer l'interface. Sous Windows, \
                 vérifier que « Microsoft Edge WebView2 Runtime » est installé."
            );
            std::process::exit(1);
        }
    };
    app.run(|_app, _event| {
        // macOS ne lance jamais une seconde instance d'un .app : un double-clic
        // sur le widget déjà lancé arrive ici en « reopen ». Même réponse qu'à
        // une seconde instance ailleurs (#8934).
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = _event {
            tauri::async_runtime::spawn(montrer_le_widget(_app.clone()));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        adresse_complete_de, cache_is_stale, fit_to_work_area, taille_mini, FULL_WINDOW_MIN_SIZE,
        FULL_WINDOW_SIZE,
    };

    #[test]
    fn la_fenetre_complete_seule_va_en_plein_ecran() {
        use super::comportement_plein_ecran;
        // Bit « Managed » (1 << 2) et « ParticipatesInCycle » (1 << 5) gardés.
        let autres = (1 << 2) | (1 << 5);
        assert_eq!(comportement_plein_ecran(autres, true), autres | (1 << 7));
        assert_eq!(comportement_plein_ecran(autres, false), autres | (1 << 9));
        // Bascule : l'ancien réglage de plein écran est retiré, pas cumulé.
        let deja_aucun = autres | (1 << 9) | (1 << 8);
        assert_eq!(
            comportement_plein_ecran(deja_aucun, true),
            autres | (1 << 7)
        );
        assert_eq!(
            comportement_plein_ecran(autres | (1 << 7), false),
            autres | (1 << 9)
        );
    }

    #[test]
    fn une_adresse_tapee_recoit_son_schema() {
        assert_eq!(
            adresse_complete_de("192.168.1.18:8888"),
            "http://192.168.1.18:8888"
        );
        assert_eq!(adresse_complete_de(" http://nas:8888/ "), "http://nas:8888");
        assert_eq!(
            adresse_complete_de("https://tune.example"),
            "https://tune.example"
        );
    }

    #[test]
    fn la_grande_fenetre_ne_depasse_plus_le_bureau() {
        // 1920x1080 a 100 % : il y a la place, on ne rabote rien.
        assert_eq!(
            fit_to_work_area(
                Some((1920, 1040)),
                1.0,
                FULL_WINDOW_SIZE,
                FULL_WINDOW_MIN_SIZE
            ),
            (1200.0, 800.0)
        );
        // Le cas de Sandro : 1920x1080 a 150 %, soit 1280x720 points une fois
        // la barre des taches deduite. Les 800 points de haut ne tiennent pas.
        let (w, h) = fit_to_work_area(
            Some((1920, 1040)),
            1.5,
            FULL_WINDOW_SIZE,
            FULL_WINDOW_MIN_SIZE,
        );
        assert!(
            h < 1040.0 / 1.5,
            "la hauteur doit tenir dans les {} points du bureau, obtenu {h}",
            1040.0 / 1.5
        );
        assert!((w - 1200.0).abs() < 0.001, "largeur inattendue : {w}");
        assert!(
            (h - (1040.0 / 1.5 - 48.0)).abs() < 0.001,
            "hauteur inattendue : {h}"
        );
        // Ecran plus petit que la taille minimale de la fenetre : la minimale
        // gagne, on ne fabrique pas une fenetre ecrasee.
        assert_eq!(
            fit_to_work_area(
                Some((1024, 640)),
                1.0,
                FULL_WINDOW_SIZE,
                FULL_WINDOW_MIN_SIZE
            ),
            (976.0, 600.0)
        );
        // Aucun ecran identifiable, ou facteur d'echelle aberrant : on garde la
        // taille voulue plutot que d'inventer une contrainte.
        assert_eq!(
            fit_to_work_area(None, 1.0, FULL_WINDOW_SIZE, FULL_WINDOW_MIN_SIZE),
            FULL_WINDOW_SIZE
        );
        assert_eq!(
            fit_to_work_area(
                Some((1920, 1040)),
                0.0,
                FULL_WINDOW_SIZE,
                FULL_WINDOW_MIN_SIZE
            ),
            FULL_WINDOW_SIZE
        );
    }

    #[test]
    fn the_cache_is_kept_only_for_the_version_that_wrote_it() {
        // Même version : on ne touche à rien, sinon la purge se redéclencherait
        // à chaque démarrage.
        assert!(!cache_is_stale(Some("0.1.4"), "0.1.4"));
        // Le témoin est écrit sans retour à la ligne, mais un éditeur ou une
        // copie manuelle peut en ajouter un : ce n'est pas un changement de
        // version.
        assert!(!cache_is_stale(Some("0.1.4\n"), "0.1.4"));
        // Version différente : c'est le cas d'une mise à jour.
        assert!(cache_is_stale(Some("0.1.3"), "0.1.4"));
        // Aucun témoin : première exécution après l'arrivée de ce correctif,
        // donc très probablement au-dessus d'un cache périmé (#1704).
        assert!(cache_is_stale(None, "0.1.4"));
        // Témoin illisible ou tronqué : on purge plutôt que de parier.
        assert!(cache_is_stale(Some(""), "0.1.4"));
    }

    #[test]
    fn la_fenetre_mini_se_replie_en_bandeau() {
        assert_eq!(taille_mini(false), (360.0, 560.0));
        assert_eq!(taille_mini(true), (360.0, 140.0));
    }
}
