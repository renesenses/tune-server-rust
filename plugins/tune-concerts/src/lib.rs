//! Les concerts des artistes de la bibliothèque, en [`TunePlugin`] (#2363).
//!
//! Extrait mot pour mot du cœur toujours-compilé, sans changer un
//! comportement :
//!
//! - `tune-core/src/cloud/concert_alerts.rs` — la tâche de fond qui pousse
//!   toutes les 24 h les artistes de la bibliothèque vers
//!   `mozaiklabs.fr/api/v1/premium/concerts/subscribe`. Elle était démarrée
//!   **sans condition** par `background.rs`, dans tous les serveurs, y compris
//!   ceux dont personne n'a jamais demandé la fonction.
//! - `GET /api/v1/system/concerts` — la route de lecture, remontée ici sur
//!   `/api/v1/ext/concerts/upcoming` (le préfixe vient de `name()` : un plugin
//!   ne choisit jamais le sien).
//!
//! Bertrand a tranché le 29/08 : la fonction sera un plugin. Le cœur nu cesse
//! donc de parler à un service tiers, et la tâche de fond ne tourne plus que
//! chez ceux qui ont installé le plugin.
//!
//! # Ce que ce plugin ne fait PAS, et pourquoi
//!
//! **Il n'est pas au catalogue** ([`ConcertsPlugin::catalogued`] rend `false`).
//! Aucun écran ne consomme encore ces routes — `git grep -i concert` dans
//! `tune-web-client` ne rend rien de la fonction. Offrir « Installer » sur une
//! fonction que rien n'expose dépense la confiance de l'utilisateur et ne rend
//! rien : il installe, il redémarre comme on le lui demande, et rien
//! n'apparaît (#2090). À rebrancher au catalogue le jour où l'écran existe.
//!
//! **Il ne rendra rien tant que le cloud n'aura pas de source.** La seule
//! source branchée aujourd'hui est MusicBrainz, dont l'entité `event` est une
//! archive : 0 date future sur Coldplay, Taylor Swift et Metallica réunis.
//! Ce n'est pas un défaut de ce plugin — la table `concert_events` est vide
//! côté cloud, et le rester est le sujet du lot 1, ailleurs.
//!
//! **Il ne collecte aucune position.** Le filtre géographique (rayon / pays /
//! partout, arbitré le 29/08) est le lot 2, et il commence côté cloud : la
//! route `upcoming` accepte `city` et `country` aujourd'hui sans rien en
//! faire. Envoyer une position que personne ne lit ne servirait personne.
//!
//! **Il ne pose pas de portillon premium.** L'arbitrage « réservé aux
//! premium » est acté, mais `tune-core/src/license.rs` n'a aucune variante
//! `Feature` pour les concerts, et en ajouter une touche aussi le catalogue
//! côté client et côté cloud. C'est le lot 5, et il doit sortir *en même temps*
//! que l'écran — sinon on pose un refus que personne ne peut voir.

use std::sync::Arc;

use async_trait::async_trait;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::TuneEvent;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

const CONCERTS_API: &str = "https://mozaiklabs.fr/api/v1/premium/concerts";

/// Services de l'hôte remis au plugin à la construction.
///
/// Passés explicitement plutôt que tirés du [`PluginContext`], comme
/// `tune-dj`, `tune-karaoke` et `tune-bandcamp` : la vraie dépendance du
/// plugin — la base — est ainsi visible au point de câblage, dans
/// `tune-server/src/plugins.rs`.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
}

pub struct ConcertsPlugin {
    backend: Arc<dyn DbBackend>,
    /// La tâche d'abonnement périodique, pour l'arrêter au `teardown`.
    ///
    /// Le cœur ne gardait aucune poignée : `tokio::spawn` et plus rien. Une
    /// tâche de plugin doit pouvoir s'arrêter quand le plugin s'arrête, sinon
    /// elle survit à son propriétaire et continue d'appeler le cloud.
    tache: Option<tokio::task::JoinHandle<()>>,
}

impl ConcertsPlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            backend: services.backend,
            tache: None,
        }
    }
}

#[async_trait]
impl TunePlugin for ConcertsPlugin {
    fn name(&self) -> &str {
        "concerts"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Concerts à venir des artistes de la bibliothèque"
    }

    /// Opt-in, comme `dj`, `karaoke` et `bandcamp`.
    fn default_enabled(&self) -> bool {
        false
    }

    /// Hors catalogue tant qu'aucun écran ne consomme ces routes — voir l'en-
    /// tête du module. Le plugin reste compilé, testé, et se charge si
    /// `plugin_concerts_installed` est posé à la main.
    fn catalogued(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(self.backend.clone()));
        self.tache = Some(lancer_synchronisation(self.backend.clone()));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        if let Some(t) = self.tache.take() {
            t.abort();
        }
        Ok(())
    }

    /// Ce plugin n'observe pas la lecture : il interroge le cloud sur une
    /// horloge. Surcharge explicite en no-op pour ne pas recevoir tout le bus
    /// pour rien.
    async fn on_event(&mut self, _event: &TuneEvent) {}
}

// ---------------------------------------------------------------------------
// Routes — montées par l'hôte sous /api/v1/ext/concerts
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct EtatConcerts {
    backend: Arc<dyn DbBackend>,
}

pub fn router(backend: Arc<dyn DbBackend>) -> Router<()> {
    Router::new()
        .route("/upcoming", get(concerts_a_venir))
        .with_state(EtatConcerts { backend })
}

/// `GET /api/v1/ext/concerts/upcoming` — remplace `GET /system/concerts`.
///
/// Le corps d'erreur de l'ancienne route était une chaîne technique anglaise
/// (`{"concerts": [], "error": "concerts: HTTP 500"}`) qu'une interface
/// traduite en 11 langues aurait affichée telle quelle. On rend désormais un
/// **code stable**, traduisible côté client, et le détail part au journal.
async fn concerts_a_venir(
    axum::extract::State(etat): axum::extract::State<EtatConcerts>,
) -> Json<Value> {
    let instance_id = SettingsRepo::with_backend(etat.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    if instance_id.is_empty() {
        return Json(json!({"concerts": [], "code": "concerts.no_instance_id"}));
    }

    let client = match tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "concerts_client_build_failed");
            return Json(json!({"concerts": [], "code": "concerts.unavailable"}));
        }
    };

    match recuperer_concerts(&client, &instance_id).await {
        Ok(concerts) => Json(json!({"concerts": concerts})),
        Err(e) => {
            warn!(error = %e, "concerts_fetch_failed");
            Json(json!({"concerts": [], "code": "concerts.unavailable"}))
        }
    }
}

// ---------------------------------------------------------------------------
// Le cloud — repris tel quel de tune-core/src/cloud/concert_alerts.rs
// ---------------------------------------------------------------------------

/// Pousse les artistes de la bibliothèque (ceux qui ont un MusicBrainz ID)
/// comme abonnements de concerts. Rend le nombre d'artistes abonnés.
///
/// La borne de 200 est celle du cloud (`'artists' => 'required|array|max:200'`) :
/// les deux sont cohérentes, ce n'est pas un défaut. Elle mordra sur les
/// grandes bibliothèques bien identifiées le jour où le verrou d'identification
/// MBID sera levé, pas avant.
pub async fn synchroniser_abonnements(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<usize, String> {
    let rows = backend
        .query_many(
            "SELECT DISTINCT musicbrainz_id, name FROM artists \
             WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id != '' \
             LIMIT 200",
            &[],
        )
        .map_err(|e| format!("query: {e}"))?;

    if rows.is_empty() {
        debug!("concert_alerts_no_artists");
        return Ok(0);
    }

    let artists: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "musicbrainz_artist_id": r.first().and_then(|v| v.as_string()),
                "artist_name": r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            })
        })
        .collect();

    let body = json!({
        "instance_id": instance_id,
        "artists": artists,
    });

    let resp = http_client
        .post(format!("{CONCERTS_API}/subscribe"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("concert subscribe: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("concert subscribe: HTTP {}", resp.status()));
    }

    let result: Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let count = result["subscribed"].as_i64().unwrap_or(0) as usize;
    info!(count, "concert_subscriptions_synced");
    Ok(count)
}

/// Récupère les concerts à venir pour les artistes auxquels cette instance
/// s'est abonnée.
pub async fn recuperer_concerts(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<Value>, String> {
    let resp = http_client
        .get(format!("{CONCERTS_API}/upcoming"))
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("concerts: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("concerts: HTTP {}", resp.status()));
    }

    let data: Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let concerts = data["concerts"].as_array().cloned().unwrap_or_default();
    info!(count = concerts.len(), "upcoming_concerts_fetched");
    Ok(concerts)
}

/// La tâche périodique : abonnement toutes les 24 h, 2 min après le démarrage.
///
/// Le double garde-fou de l'original est conservé : le réglage
/// `community_sync_enabled` **et** un `instance_id` non vide. Ce qui change,
/// c'est qu'elle ne démarre plus que si le plugin est installé — avant, elle
/// tournait dans tous les serveurs.
fn lancer_synchronisation(backend: Arc<dyn DbBackend>) -> tokio::task::JoinHandle<()> {
    let client = match tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "concert_alerts_client_build_failed");
            return tokio::spawn(async {});
        }
    };

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;

        loop {
            let settings = SettingsRepo::with_backend(backend.clone());
            let enabled = settings
                .get("community_sync_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false);

            if enabled {
                let instance_id = settings
                    .get("instance_id")
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                if !instance_id.is_empty() {
                    if let Err(e) = synchroniser_abonnements(&backend, &client, &instance_id).await
                    {
                        warn!(error = %e, "concert_subscriptions_sync_failed");
                    }
                } else {
                    debug!("concert_alerts_skipped_no_instance_id");
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
        }
    })
}
