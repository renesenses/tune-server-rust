//! Plugin runtime wiring.
//!
//! `tune-core` owns the plugin *contract* ([`TunePlugin`], [`PluginLoader`]);
//! this module owns the *lifecycle*: which plugins exist, when they are set
//! up, and how what they register reaches the rest of the server.
//!
//! ## Loading model
//!
//! Plugins are compiled in, behind cargo features — there is no `libloading`
//! and no wasm runtime. `docs/ARCHITECTURE-CIBLE-v0.9.md` is explicit that
//! dynamic loading is a target, not the current state; adding it here would
//! mean pinning an ABI across rustc releases, which the roadmap defers.
//!
//! Adding a plugin is therefore three lines: a feature in `Cargo.toml`, an
//! optional dependency, and an arm in [`register_builtin_plugins`].
//!
//! ## Startup order
//!
//! [`init`] must run **after** `register_local_outputs` (so a plugin output
//! never races the local-device scan for the same zone row) and **before**
//! `routes::router` (which needs the plugin routers to mount them).

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::plugin_sdk::{PluginLoader, PluginRegistrations, TunePlugin};

use crate::state::AppState;

/// Routers contributed by plugins, ready to be mounted by
/// [`crate::routes::router`]. `(plugin name, router)`.
pub type PluginRouters = Vec<(String, axum::Router<()>)>;

/// Build an empty, fully-configured loader.
///
/// Synchronous so `AppState::new` can own it. Plugins are attached later, in
/// [`init`], because `PluginLoader::register` is async.
pub fn build_loader(
    event_bus: &tune_core::event_bus::EventBus,
    backend: Arc<dyn tune_core::db::backend::DbBackend>,
    license: Arc<tune_core::license::LicenseManager>,
) -> PluginLoader {
    // La licence descend jusqu'aux greffons (`PluginContext::license`) pour
    // qu'un greffon payant ADAPTE sa réponse au lieu de se voir fermer la porte
    // par l'hôte : « Concerts » servira un jour une version réduite aux comptes
    // gratuits, et cette décision doit tenir chez lui, en un seul endroit.
    PluginLoader::new(plugins_data_root())
        .with_event_bus(event_bus.clone())
        .with_db(backend)
        .with_license(license)
}

/// Where plugins keep their private state. Each plugin gets
/// `{root}/{plugin_name}/`, created by `setup_all`.
fn plugins_data_root() -> std::path::PathBuf {
    std::env::var("TUNE_PLUGINS_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("TUNE_PLUGINS_DIR")
                .map(|d| std::path::PathBuf::from(d).join("data"))
                .unwrap_or_else(|_| std::path::PathBuf::from("plugins/data"))
        })
}

/// The compiled-in plugin set.
///
/// Empty in this tree, and it should stay that way for anything whose source
/// is not in this repository. A `path` dependency pointing outside the repo
/// breaks `cargo check` for every clone — cargo resolves optional path
/// dependencies while writing the lockfile, so `optional = true` does not save
/// you and neither does leaving the feature off. Such a plugin composes from
/// its own workspace instead, via [`PluginBuilder`] and
/// [`crate::bootstrap::run`]; this function is only for source that lives here.
///
/// ```ignore
/// // 1. Cargo.toml — feature + optional dependency
/// //      myplugin = ["dep:tune-myplugin"]
/// //      tune-myplugin = { path = "../../plugins/tune-myplugin", optional = true }
/// // 2. here:
/// #[cfg(feature = "myplugin")]
/// loader
///     .register(Box::new(tune_myplugin::MyPlugin::new(
///         tune_myplugin::HostServices {
///             backend: state.backend.clone(),
///             http_client: state.http_client.clone(),
///         },
///     )))
///     .await;
/// ```
///
/// Host services are passed explicitly at construction rather than pulled
/// from [`PluginContext`], which deliberately exposes only the DB, the event
/// bus and a data directory — so a plugin's real dependencies are visible at
/// the wiring site.
#[allow(unused_variables)]
async fn register_builtin_plugins(loader: &PluginLoader, state: &AppState) {
    crate::premium_audio_plugins::register(loader, state).await;
    // P5 (#917): DJ mode, extracted from the always-on core into a native
    // in-tree plugin. Host services are passed explicitly at construction so
    // DJ's real dependency (the DB backend) is visible here at the wiring site.
    #[cfg(feature = "dj")]
    loader
        .register(Box::new(tune_dj::DjPlugin::new(tune_dj::HostServices {
            backend: state.backend.clone(),
        })))
        .await;

    // Karaoke: native synced-lyrics plugin (model B). Reuses
    // `tune_core::lyrics` — no LRC parse/fetch reimplemented. Host services are
    // passed explicitly so its real dependencies (DB backend, HTTP client, and
    // the playback manager for `/now`) are visible here at the wiring site.
    #[cfg(feature = "karaoke")]
    loader
        .register(Box::new(tune_karaoke::KaraokePlugin::new(
            tune_karaoke::HostServices {
                backend: state.backend.clone(),
                client: state.http_client.clone(),
                playback: state.playback.clone(),
            },
        )))
        .await;

    // Bandcamp (#1768) : recherche, découverte et lecture, sorties du cœur
    // toujours-compilé. Le lot 1 n'avait besoin d'AUCUN service — il ne parlait
    // qu'HTTP à un tiers. Le lot 2 mémorise le pseudo et le `fan_id` résolu de
    // l'acheteur : il lui faut la base, passée explicitement ici comme pour les
    // deux autres plugins.
    #[cfg(feature = "bandcamp")]
    loader
        .register(Box::new(tune_bandcamp::BandcampPlugin::new(
            tune_bandcamp::HostServices {
                backend: state.backend.clone(),
            },
        )))
        .await;

    // Concerts (#2363) : la tâche d'abonnement 24 h et la route de lecture,
    // sorties du cœur toujours-compilé. Elle y était démarrée SANS condition
    // par `background.rs` — elle ne tourne désormais que chez ceux qui ont
    // installé le plugin. Sa dépendance explicite est la base : lire les artistes
    // de la bibliothèque et `instance_id`. La licence lui arrive par le contexte
    // (`PluginContext::license`) : c'est lui qui décide ce qu'il sert (Premium).
    #[cfg(feature = "concerts")]
    loader
        .register(Box::new(tune_concerts::ConcertsPlugin::new(
            tune_concerts::HostServices {
                backend: state.backend.clone(),
            },
        )))
        .await;

    // Lecture directe d'un CD audio (#4863). L'orchestrateur est passé pour
    // que le greffon y inscrive sa source PCM `cd` et lance la file du disque ;
    // le gestionnaire de lecture, pour la longueur de file et l'état des zones.
    // #2466 : le scan ciblé du dossier d'un album extrait, le même que
    // `POST /system/scan?path=`.
    #[cfg(feature = "cd")]
    loader
        .register(Box::new(tune_cd::CdPlugin::new(tune_cd::HostServices {
            backend: state.backend.clone(),
            orchestrator: state.orchestrator.clone(),
            playback: state.playback.clone(),
            scan: Some(std::sync::Arc::new(ScanDuDisque {
                state: state.clone(),
            })),
        })))
        .await;

    // Tune Circle, étape T1 (#5018). La base seule : le greffon y relit, à
    // chaque appel, la session SSO du serveur (`mozaik_access_token`,
    // `mozaik_refresh_token`, `mozaik_base_url`) — la même que lisent
    // `library_sync` et les routes `/cloud/*`. T2 (#5325) : la licence, pour
    // que `GET /library-sync` dise Premium avec le même juge que la synchro.
    // T4 (#5327) : le gestionnaire de lecture, pour l'écoute chez un contact.
    #[cfg(feature = "circle")]
    loader
        .register(Box::new(tune_circle::CirclePlugin::new(
            tune_circle::HostServices {
                backend: state.backend.clone(),
                license: state.license.clone(),
                // T3 (#5326) : le profil d'une requête et la résolution d'une
                // collection intelligente, par les juges du serveur.
                hote: std::sync::Arc::new(HoteDuCercle {
                    state: state.clone(),
                }),
                // T5 (#5328) : rejouer une playlist de cercle chez
                // l'utilisateur, puis la jouer sur une zone.
                services: state.services.clone(),
                orchestrator: state.orchestrator.clone(),
                playback: state.playback.clone(),
                // #6521 : les vignettes partagées avec le cercle.
                artwork_dir: crate::routes::library::artwork_cache_dir(),
            },
        )))
        .await;

    // Entrée audio en direct (#5051). Comme `cd` : l'orchestrateur pour y
    // inscrire la source PCM EN DIRECT `entree-audio` et lancer la zone, le
    // gestionnaire de lecture pour la file et l'état des zones.
    #[cfg(feature = "entree-audio")]
    loader
        .register(Box::new(tune_entree_audio::EntreeAudioPlugin::new(
            tune_entree_audio::HostServices {
                backend: state.backend.clone(),
                orchestrator: state.orchestrator.clone(),
                playback: state.playback.clone(),
            },
        )))
        .await;

    // Assistant IA, étape 1 (chantier assistant-ia). Toujours compilé, gratuit.
    // La base ; le DOSSIER de la base, où vit la clé maîtresse qui scelle la
    // clé d'API de l'utilisateur (hors de la base elle-même) ; et les critères
    // du moteur des règles intelligentes, pour que le schéma imposé au modèle
    // soit celui du moteur et rien d'autre.
    #[cfg(feature = "assistant-ia")]
    loader
        .register(Box::new(tune_assistant_ia::AssistantIaPlugin::new(
            tune_assistant_ia::HostServices {
                backend: state.backend.clone(),
                dossier_secret: dossier_de_la_base(),
                criteres: criteres_du_moteur(),
                diagnostic: Some(collecte_diagnostic(state.clone())),
            },
        )))
        .await;

    // « Ma collection physique » (#7195). Le greffon ne parle à aucun tiers
    // lui-même : Discogs (jeton résolu ici, jamais remis au greffon),
    // MusicBrainz, les services et la bibliothèque passent par son hôte.
    #[cfg(feature = "collection-physique")]
    loader
        .register(Box::new(
            tune_collection_physique::CollectionPhysiquePlugin::new(
                tune_collection_physique::HostServices {
                    backend: state.backend.clone(),
                    hote: std::sync::Arc::new(HoteDeLaCollection {
                        state: state.clone(),
                    }),
                },
            ),
        ))
        .await;

    // Pont Roon (#3914) — Premium. La licence est passée pour que le greffon
    // refuse lui-même (402) ; le cache d'illustrations, pour y ranger les
    // images de l'archive sous le même condensat que tout le reste.
    #[cfg(feature = "pont-roon")]
    loader
        .register(Box::new(tune_pont_roon::PontRoonPlugin::new(
            tune_pont_roon::HostServices {
                backend: state.backend.clone(),
                license: state.license.clone(),
                dossier_cache: crate::routes::library::artwork_cache_dir(),
            },
        )))
        .await;

    // « Mon thème » (tune-web-client#2527, rc10) : le thème de chaque profil,
    // rangé dans `settings`. Le profil d'une requête est jugé par l'hôte,
    // avec l'extracteur de toutes les routes.
    #[cfg(feature = "mon-theme")]
    loader
        .register(Box::new(tune_mon_theme::MonThemePlugin::new(
            tune_mon_theme::HostServices {
                backend: state.backend.clone(),
                hote: std::sync::Arc::new(HoteDuTheme {
                    state: state.clone(),
                }),
            },
        )))
        .await;
}

/// L'hôte de « Mon thème » (#2527) : le profil d'une requête, jugé comme
/// partout (`X-Profile-Id` permis seulement à son titulaire quand
/// l'authentification est active, puis l'appelant, puis le profil global).
#[cfg(feature = "mon-theme")]
struct HoteDuTheme {
    state: AppState,
}

#[cfg(feature = "mon-theme")]
#[async_trait::async_trait]
impl tune_mon_theme::Hote for HoteDuTheme {
    async fn profil_actif(&self, parts: &mut axum::http::request::Parts) -> i64 {
        use axum::extract::FromRequestParts;
        match tune_http_types::ActiveProfile::from_request_parts(parts, &self.state).await {
            Ok(p) => p.id(),
            Err(never) => match never {},
        }
    }
}

/// L'hôte de « Ma collection physique » (#7195).
///
/// * Discogs : le jeton est résolu ICI, à chaque appel, par le juge des Clés
///   d'API (`cles_api::cle` : le personnel, scellé dans le coffre, avant
///   l'intégré). Il part dans l'en-tête `Authorization` et nulle part
///   ailleurs : ni dans l'URL, ni dans un journal, ni dans une réponse au
///   greffon. Chaque appel attend le limiteur Discogs PARTAGÉ et lui rend
///   compte de la réponse (un 429 le ralentit pour tout le serveur).
/// * MusicBrainz : le transport et le limiteur partagés.
/// * Services : Qobuz et TIDAL s'ils sont UTILISABLES (activés et
///   connectés), par leur recherche ordinaire.
/// * Bibliothèque : les albums LOCAUX, masqués exclus.
#[cfg(feature = "collection-physique")]
struct HoteDeLaCollection {
    state: AppState,
}

#[cfg(feature = "collection-physique")]
impl HoteDeLaCollection {
    async fn service(
        &self,
        nom: &str,
    ) -> Option<Arc<tokio::sync::RwLock<Box<dyn tune_core::streaming::traits::StreamingService>>>>
    {
        let service = self.state.services.lock().await.get(nom)?;
        let utilisable = service.read().await.utilisable().await;
        utilisable.then_some(service)
    }
}

#[cfg(feature = "collection-physique")]
#[async_trait::async_trait]
impl tune_collection_physique::Hote for HoteDeLaCollection {
    fn jeton_discogs(&self) -> tune_collection_physique::Jeton {
        use tune_collection_physique::Jeton;
        use tune_core::cles_api::{Origine, Service};
        match crate::routes::system::cles_api::cle(&self.state, Service::Discogs) {
            None => Jeton::Aucun,
            Some(c) if c.origine == Origine::Personnelle => Jeton::Personnel,
            Some(_) => Jeton::Integre,
        }
    }

    async fn discogs(
        &self,
        chemin: &str,
        params: &[(&str, String)],
    ) -> Result<tune_collection_physique::Reponse, tune_collection_physique::Panne> {
        use tune_collection_physique::{Panne, Reponse};
        use tune_core::http::fetch::{CLE_DISCOGS, DISCOGS};
        let jeton = crate::routes::system::cles_api::cle(
            &self.state,
            tune_core::cles_api::Service::Discogs,
        );
        DISCOGS.acquire(CLE_DISCOGS).await;
        let mut req = self
            .state
            .http_client
            .get(format!(
                "{}{chemin}",
                tune_core::metadata::discogs_propositions::BASE_DISCOGS
            ))
            .query(params)
            .header(
                reqwest::header::USER_AGENT,
                tune_core::metadata::musicbrainz_release::MB_UA,
            )
            .timeout(std::time::Duration::from_secs(20));
        if let Some(j) = &jeton {
            req = req.header(
                reqwest::header::AUTHORIZATION,
                format!("Discogs token={}", j.valeur()),
            );
        }
        let r = req.send().await.map_err(|e| {
            tracing::debug!(
                target: "plugin",
                erreur = %tune_core::http::client::decrire_erreur_http(&e),
                "collection_physique_discogs_sans_reponse"
            );
            Panne
        })?;
        DISCOGS.constater_reponse(CLE_DISCOGS, &r);
        match r.status().as_u16() {
            200..=299 => r
                .json::<serde_json::Value>()
                .await
                .map(Reponse::Json)
                .map_err(|_| Panne),
            404 => Ok(Reponse::Introuvable),
            401 | 403 => Ok(Reponse::Refusee),
            429 => Ok(Reponse::Limite),
            statut => {
                tracing::debug!(target: "plugin", statut, "collection_physique_discogs_statut");
                Err(Panne)
            }
        }
    }

    async fn musicbrainz(
        &self,
        chemin: &str,
        params: &[(&str, String)],
    ) -> Result<tune_collection_physique::Reponse, tune_collection_physique::Panne> {
        use tune_collection_physique::{Panne, Reponse};
        use tune_core::metadata::musicbrainz_release::{
            RefusMusicBrainz, parcourir_sur_musicbrainz, rate_limit_delay,
        };
        rate_limit_delay().await;
        match parcourir_sur_musicbrainz(chemin, params).await {
            Ok(v) => Ok(Reponse::Json(v)),
            Err(RefusMusicBrainz::Statut(400 | 404)) => Ok(Reponse::Introuvable),
            Err(RefusMusicBrainz::Statut(429 | 503)) => Ok(Reponse::Limite),
            Err(_) => Err(Panne),
        }
    }

    async fn services_connectes(&self) -> Vec<String> {
        let mut rendus = Vec::new();
        for nom in tune_collection_physique::synchro::SERVICES {
            if self.service(nom).await.is_some() {
                rendus.push(nom.to_string());
            }
        }
        rendus
    }

    async fn chercher_albums(
        &self,
        service: &str,
        requete: &str,
    ) -> Result<
        Vec<tune_collection_physique::correspondance::AlbumDeService>,
        tune_collection_physique::Panne,
    > {
        let Some(s) = self.service(service).await else {
            return Ok(Vec::new());
        };
        let resultats = s.read().await.search(requete, 10).await.map_err(|e| {
            tracing::debug!(target: "plugin", service, erreur = %e, "collection_physique_recherche_echouee");
            tune_collection_physique::Panne
        })?;
        Ok(resultats
            .albums
            .into_iter()
            .map(
                |a| tune_collection_physique::correspondance::AlbumDeService {
                    id: a.id,
                    titre: a.title,
                    artiste: a.artist,
                },
            )
            .collect())
    }

    async fn albums_locaux(
        &self,
    ) -> Result<Vec<tune_collection_physique::correspondance::AlbumLocal>, String> {
        let repo = tune_core::db::album_repo::AlbumRepo::with_backend(self.state.backend.clone());
        const PAGE: i64 = 1_000;
        let mut rendus = Vec::new();
        let mut offset = 0;
        loop {
            // `include_hidden = false` : un album masqué n'est pas « dans ma
            // bibliothèque » pour l'écran.
            let page = repo
                .list_filtered(PAGE, offset, "title", "asc", None, None, None, false, None)
                .map_err(|e| e.to_string())?;
            let n = page.len() as i64;
            for a in page {
                let local = a.source.is_empty() || a.source == "local";
                if !local {
                    continue;
                }
                let qualite = a.quality();
                rendus.push(tune_collection_physique::correspondance::AlbumLocal {
                    id: a.id.unwrap_or_default(),
                    titre: a.title,
                    artiste: a.artist_name,
                    code_barre: a.barcode,
                    mb_release: a.musicbrainz_release_id,
                    mb_groupe: a.musicbrainz_release_group_id,
                    qualite,
                    format: a.format,
                    sample_rate: a.sample_rate,
                    bit_depth: a.bit_depth,
                });
            }
            if n < PAGE {
                return Ok(rendus);
            }
            offset += PAGE;
        }
    }
}

#[cfg(feature = "assistant-ia")]
/// La passerelle de l'Assistant IA (étape 2) : l'ancien `/ai/query` et la
/// traduction des ambiances y prennent la clé (coffre du greffon, après
/// migration de l'ancienne clé en clair), le plafond et le fournisseur de
/// l'assistant. Même base et même dossier de clé maîtresse que le greffon.
pub(crate) fn passerelle_assistant(
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
) -> tune_assistant_ia::Passerelle {
    tune_assistant_ia::Passerelle::new(backend.clone(), &dossier_de_la_base())
}

/// Le diagnostic de l'assistant (23/10/2026) : ce que l'hôte rassemble sur
/// une zone, en LECTURE SEULE. Le greffon filtre, anonymise et pseudonymise
/// avant tout envoi, et montre le texte exact avant chaque envoi.
#[cfg(feature = "assistant-ia")]
fn collecte_diagnostic(state: AppState) -> tune_assistant_ia::diagnostic::Collecte {
    Arc::new(move |zone_id| {
        let state = state.clone();
        Box::pin(async move { Ok(contexte_diagnostic(&state, zone_id).await) })
    })
}

#[cfg(feature = "assistant-ia")]
async fn contexte_diagnostic(
    state: &AppState,
    zone_id: Option<i64>,
) -> tune_assistant_ia::diagnostic::ContexteBrut {
    use axum::response::IntoResponse;
    use std::collections::BTreeMap;
    use tune_core::db::backend::SqlValue;

    let mut noms: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ajouter = |cat: &str, v: Option<String>| {
        if let Some(v) = v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            noms.entry(cat.to_string()).or_default().push(v);
        }
    };
    for z in tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default()
    {
        ajouter("zone", Some(z.name));
    }
    for d in state.scanner.devices().await {
        ajouter("appareil", Some(d.name));
        ajouter("hote", Some(d.host));
    }
    ajouter("hote", Some(tune_core::discovery::system_hostname()));
    // Les titres récents : ils peuvent apparaître dans le journal.
    let texte = |v: Option<&SqlValue>| v.and_then(|x| x.as_string());
    for l in state
        .backend
        .query_many(
            "SELECT title, artist_name, album_title FROM listen_history ORDER BY id DESC LIMIT 100",
            &[],
        )
        .unwrap_or_default()
    {
        ajouter("piste", texte(l.first()));
        ajouter("artiste", texte(l.get(1)));
        ajouter("album", texte(l.get(2)));
    }
    let zone = match zone_id {
        Some(id) => {
            let rep = crate::routes::zones::get_zone(
                axum::extract::State(state.clone()),
                axum::extract::Path(id),
            )
            .await
            .into_response();
            if rep.status().is_success() {
                axum::body::to_bytes(rep.into_body(), 4 << 20)
                    .await
                    .ok()
                    .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o).ok())
            } else {
                None
            }
        }
        None => None,
    };
    if let Some(np) = zone.as_ref().and_then(|z| z.get("current_track")) {
        let champ = |k: &str| np.get(k).and_then(|v| v.as_str()).map(String::from);
        ajouter("piste", champ("title"));
        ajouter("artiste", champ("artist").or_else(|| champ("artist_name")));
        ajouter("album", champ("album").or_else(|| champ("album_title")));
    }
    let journal = crate::routes::system::diagnostics::journal_pour_assistant(1000).await;
    tune_assistant_ia::diagnostic::ContexteBrut {
        zone,
        journal,
        systeme: serde_json::json!({
            "version": tune_core::version(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        }),
        noms,
    }
}

#[cfg(feature = "assistant-ia")]
/// Le dossier de la base retenue au démarrage (Docker : `/data`), sinon celui
/// de `TUNE_DB_PATH`, sinon le dossier courant — là où `tune.db` se pose.
fn dossier_de_la_base() -> std::path::PathBuf {
    let base = crate::chemins_de_donnees::base_retenue()
        .map(String::from)
        .or_else(|| std::env::var("TUNE_DB_PATH").ok())
        .unwrap_or_else(|| "tune.db".into());
    std::path::Path::new(&base)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

#[cfg(feature = "assistant-ia")]
/// Les critères du moteur des règles intelligentes, dans la forme du greffon.
fn criteres_du_moteur() -> Vec<tune_assistant_ia::CritereMoteur> {
    tune_smart_http::criteres::criteres_exposes()
        .into_iter()
        .map(|c| tune_assistant_ia::CritereMoteur {
            collection: c.collection.map(String::from),
            piste: c.piste.map(String::from),
            famille: c.famille.to_string(),
            operateurs: c.operateurs.iter().map(|s| s.to_string()).collect(),
        })
        .collect()
}

/// Builds the plugins an out-of-tree binary wants registered.
///
/// Takes `&AppState` because a plugin's host services — `services`, `backend`,
/// `http_client` — only exist once state is built. Called once, from [`init`].
///
/// This is what makes a closed-source plugin possible: it composes from a
/// separate workspace that depends on `tune-server` as a library, so this
/// repository never names it and a plain clone still builds. See
/// [`crate::bootstrap::run`].
pub type PluginBuilder = Box<dyn FnOnce(&AppState) -> Vec<Box<dyn TunePlugin>> + Send>;

/// Set every plugin up, install what they registered, and start event
/// dispatch. Returns the routers for [`crate::routes::router`] to mount.
///
/// `extra` is registered alongside the compiled-in set, on equal terms: same
/// protocol gate, same `plugin_{name}_enabled` switch, same registration
/// draining. An out-of-tree plugin is not a second-class one.
pub async fn init(
    state: &AppState,
    api_base_url: &str,
    extra: Vec<Box<dyn TunePlugin>>,
) -> PluginRouters {
    let mut loader = state.plugins.lock().await;

    register_builtin_plugins(&loader, state).await;
    for plugin in extra {
        info!(plugin_name = %plugin.name(), "plugin_registered_out_of_tree");
        loader.register(plugin).await;
    }
    // Publish the registered set BEFORE `setup_all`, which retains only what
    // loaded — afterwards the loader no longer knows what this binary carries.
    // Published unconditionally, ahead of the early return: an empty list and
    // an unpublished one must mean the same thing to a reader, which they do
    // (`OnceLock::get() -> None` degrades to the empty slice).
    let _ = state.plugin_names.set(loader.registered_names().await);

    if loader.plugin_count().await == 0 {
        return Vec::new();
    }

    // #5370 — la page d'attente du démarrage nomme le greffon en cours ; le
    // chargeur, lui, journalise la durée de chacun (`plugin_loaded`,
    // `plugin_setup_slow`).
    let loaded = loader
        .setup_all_observed(api_base_url, &|name| {
            crate::boot_status::set_current(Some(name))
        })
        .await;
    crate::boot_status::set_current(None);

    // Publish the dormant set first, and unconditionally: opt-in plugins
    // (DJ/Karaoke) are the whole reason `loaded` can be empty while there is
    // still something for the plugin manager to list. Do it before the
    // early-return below or a fresh install — nothing loaded yet — would hide
    // them from the manager and leave the user no way to install them.
    let _ = state.plugin_available.set(loader.unloaded_plugins());

    if loaded.is_empty() {
        return Vec::new();
    }

    // Publish the introspection snapshot for the REST handlers. `setup_all`
    // already retained only what loaded, so this is a faithful list; the point
    // of copying it out is contention. Event dispatch holds the loader lock
    // across every plugin's `on_event`, so a handler taking the same lock would
    // hang for as long as the slowest plugin — while holding `state.plugins`,
    // which delays shutdown too. Nothing registers after init, so one snapshot
    // serves every request with no lock at all.
    let _ = state.plugin_info.set(loader.loaded_plugins().await);

    let registrations = loader.take_registrations();
    let routers = install(state, registrations).await;

    loader.start_event_dispatch();
    info!(plugins = ?loaded, "plugins_ready");

    routers
}

/// Apply a drained [`PluginRegistrations`]: outputs into the registry, zones
/// into the DB, routers handed back to the caller.
///
/// Also called after a successful `POST /plugins/{name}/retry` (#5403): the
/// outputs and zones go live at once, but the routers can only be mounted by
/// the next start — the caller says so with `restart_required`.
pub(crate) async fn install(state: &AppState, registrations: PluginRegistrations) -> PluginRouters {
    // `routers` exists unconditionally here: tune-server always enables
    // tune-core's `plugin-http` feature (see its Cargo.toml).
    let PluginRegistrations {
        outputs,
        output_providers,
        routers,
        zones,
    } = registrations;

    // Providers DISCOVER their devices, so they cannot go into the registry
    // here: there is nothing to register yet. They are handed to the same
    // background poller that serves out-of-tree providers passed through
    // `RunOptions::output_providers` — one path, one lifecycle, so an in-tree
    // plugin and an out-of-tree crate behave identically.
    //
    // Spawned BEFORE the fixed outputs below on purpose: `spawn_output_providers`
    // only starts a task, it registers nothing synchronously, so it cannot race
    // the registry lock taken just after.
    if !output_providers.is_empty() {
        info!(
            providers = output_providers.len(),
            "plugin_output_providers_spawned"
        );
        crate::discovery_setup::spawn_output_providers(state, output_providers);
    }

    // device_ids this plugin actually got into the registry. A zone is only
    // created for one of these — see the zone loop below.
    let mut claimed: HashSet<String> = HashSet::new();

    if !outputs.is_empty() {
        let mut registry = state.outputs.lock().await;
        for output in outputs {
            let device_id = output.device_id().to_string();
            // A plugin output claiming an id that discovery already owns would
            // silently replace a real device. Refuse rather than shadow it.
            if registry.contains(&device_id) {
                warn!(
                    device_id = %device_id,
                    "plugin_output_device_id_conflict — not registered"
                );
                continue;
            }
            info!(
                device_id = %device_id,
                name = %output.name(),
                output_type = %output.output_type(),
                "plugin_output_registered"
            );
            registry.register(output);
            claimed.insert(device_id);
        }
    }

    for zone in zones {
        // Only bind a zone to a device this plugin just claimed. Two reasons:
        // an id whose output was refused above belongs to *another* output, so
        // creating the zone anyway would quietly point the plugin's zone at
        // someone else's device; and an id no output ever claimed leaves an
        // orphan zone with nothing behind it.
        if !claimed.contains(&zone.device_id) {
            warn!(
                name = %zone.name,
                device_id = %zone.device_id,
                "plugin_zone_skipped — no output of this plugin claimed that device_id"
            );
            continue;
        }

        let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        match repo.get_or_create(&zone.name, Some(&zone.output_type), &zone.device_id) {
            Ok((zone_id, true)) => {
                info!(
                    zone_id,
                    name = %zone.name,
                    device_id = %zone.device_id,
                    "plugin_zone_created"
                );
            }
            Ok((_, false)) => {
                // Already there from a previous boot — just mark it reachable.
                let _ = repo.set_online_by_device(&zone.device_id, true);
            }
            Err(e) => {
                warn!(
                    name = %zone.name,
                    device_id = %zone.device_id,
                    error = %e,
                    "plugin_zone_create_failed"
                );
            }
        }
    }

    routers
}

/// Tear every plugin down. Called on graceful shutdown, before the process
/// exits, so a plugin can finish flushing whatever it holds open.
pub async fn shutdown(plugins: &Arc<Mutex<PluginLoader>>) {
    let mut loader = plugins.lock().await;
    if loader.plugin_count().await == 0 {
        return;
    }
    info!("plugins_shutting_down");
    loader.teardown_all().await;
}

// ---------------------------------------------------------------------------
// Wasm-plugin facade for the REST routes.
//
// `plugins_host` only exists under `plugins-wasm` (release builds all enable
// it); these always-compiled wrappers let `routes/{plugins,marketplace}.rs`
// call install/uninstall without sprinkling feature cfgs through handlers. In
// a build without the runtime, install honestly fails instead of pretending
// (the old stub set a settings flag and called that "installed").
// ---------------------------------------------------------------------------

/// Persist a downloaded marketplace archive on disk. Returns the manifest id.
#[cfg(feature = "plugins-wasm")]
pub(crate) fn persist_wasm_archive(data: &[u8]) -> Result<String, String> {
    crate::plugins_host::persist_plugin_archive(data)
}

#[cfg(not(feature = "plugins-wasm"))]
pub(crate) fn persist_wasm_archive(_data: &[u8]) -> Result<String, String> {
    Err("plugin runtime not built (plugins-wasm feature disabled)".to_string())
}

/// What an uninstall did on disk (see `plugins_host::Removal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retrait {
    /// Nothing on disk (including builds without the runtime).
    Absent,
    /// The plugin's folder was deleted.
    Supprime,
    /// Shipped with the installation, read-only: untouched, to be disabled
    /// and hidden by the caller ([`masquer_greffon`]).
    FourniEnLectureSeule,
}

/// A removal that failed, reduced to a stable code the API can translate. The
/// OS message and path stay in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetraitImpossible {
    pub code: &'static str,
}

/// Remove an installed wasm plugin, wherever it lives (user root, then the
/// shipped root). A shipped, read-only folder is not an error: it comes back
/// as [`Retrait::FourniEnLectureSeule`].
#[cfg(feature = "plugins-wasm")]
pub(crate) fn remove_wasm_dir(id: &str) -> Result<Retrait, RetraitImpossible> {
    use crate::plugins_host::Removal;
    match crate::plugins_host::remove_plugin_dir(id) {
        Ok(Removal::Absent) => Ok(Retrait::Absent),
        Ok(Removal::Removed) => Ok(Retrait::Supprime),
        Ok(Removal::ReadOnlyShipped) => Ok(Retrait::FourniEnLectureSeule),
        Err(e) => {
            tracing::warn!(id = %id, code = e.code, detail = %e.detail, "plugin_uninstall_failed");
            Err(RetraitImpossible { code: e.code })
        }
    }
}

#[cfg(not(feature = "plugins-wasm"))]
pub(crate) fn remove_wasm_dir(_id: &str) -> Result<Retrait, RetraitImpossible> {
    Ok(Retrait::Absent)
}

/// Every wasm plugin on disk, all roots merged (one entry per id). Empty in a
/// build without the runtime.
#[cfg(feature = "plugins-wasm")]
pub(crate) async fn scan_wasm_plugins() -> Result<Vec<tune_core::plugins::PluginInfo>, String> {
    crate::plugins_host::scan_all_roots().await
}

#[cfg(not(feature = "plugins-wasm"))]
pub(crate) async fn scan_wasm_plugins() -> Result<Vec<tune_core::plugins::PluginInfo>, String> {
    Ok(Vec::new())
}

/// The folder holding the manifest of `id`, in the first root that has one.
#[cfg(feature = "plugins-wasm")]
pub(crate) fn wasm_plugin_dir(id: &str) -> Option<std::path::PathBuf> {
    crate::plugins_host::plugin_roots()
        .into_iter()
        .map(|root| root.join(id))
        .find(|dir| dir.join("manifest.json").exists())
}

#[cfg(not(feature = "plugins-wasm"))]
pub(crate) fn wasm_plugin_dir(_id: &str) -> Option<std::path::PathBuf> {
    None
}

/// Message translation key for a [`RetraitImpossible::code`]; a code we do not
/// know falls to the generic one, never to a raw OS message.
pub(crate) fn cle_erreur_de_retrait(code: &str) -> &'static str {
    match code {
        "permission_denied" => "plugin.uninstall.permission_denied",
        "read_only" => "plugin.uninstall.read_only",
        "unsafe_id" => "plugin.uninstall.unsafe_id",
        _ => "plugin.uninstall.failed",
    }
}

/// 4xx/5xx answer for a failed uninstall: stable `code`, and a sentence in the
/// caller's language (`detail` and `message` carry the same text, for clients
/// that read either) — no path, no OS error number.
pub(crate) fn reponse_retrait_impossible(
    lang: &str,
    e: &RetraitImpossible,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let texte = crate::i18n::t(lang, cle_erreur_de_retrait(e.code));
    let statut = if e.code == "unsafe_id" {
        axum::http::StatusCode::BAD_REQUEST
    } else {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        statut,
        axum::Json(serde_json::json!({
            "error": "uninstall_failed",
            "code": e.code,
            "detail": texte,
            "message": texte,
        })),
    )
        .into_response()
}

fn cle_masque(id: &str) -> String {
    format!("plugin_{id}_hidden")
}

/// Has this (shipped, undeletable) plugin been uninstalled by the user? The
/// flag lives in the settings database, i.e. under the data folder.
pub(crate) fn greffon_masque(settings: &SettingsRepo, id: &str) -> bool {
    settings.get(&cle_masque(id)).ok().flatten().as_deref() == Some("true")
}

/// Uninstall of a shipped read-only plugin: persist "hidden" and "disabled" so
/// it neither shows up nor loads again — and so the generic uninstall cleanup,
/// which drops the `enabled` key (default: enabled), cannot revive it.
pub(crate) fn masquer_greffon(settings: &SettingsRepo, id: &str) {
    settings.set(&cle_masque(id), "true").ok();
    settings.set(&format!("plugin_{id}_enabled"), "false").ok();
}

/// Installing a plugin again brings back a hidden shipped one.
pub(crate) fn demasquer_greffon(settings: &SettingsRepo, id: &str) {
    settings.delete(&cle_masque(id)).ok();
}

/// Probe-child dispatch for the wasm runtime (see
/// `plugins_host::maybe_run_wasm_probe`). Always-compiled facade so the
/// binary entry points can call it without feature cfgs; a build without
/// `plugins-wasm` has nothing to probe.
#[cfg(feature = "plugins-wasm")]
pub fn maybe_run_wasm_probe() {
    crate::plugins_host::maybe_run_wasm_probe();
}

#[cfg(not(feature = "plugins-wasm"))]
pub fn maybe_run_wasm_probe() {}

/// #2466 — le scan ciblé que le greffon `cd` demande après une extraction :
/// celui de `POST /system/scan?path=` (incrémental, purge bornée au dossier).
#[cfg(feature = "cd")]
struct ScanDuDisque {
    state: AppState,
}

#[cfg(feature = "cd")]
#[async_trait::async_trait]
impl tune_cd::extraction::ScanCible for ScanDuDisque {
    async fn scanner(&self, dossier: String) -> bool {
        let cible = tune_core::scanner::walker::normalize_path(&dossier);
        crate::routes::system::scan::spawn_library_scan_par(
            self.state.clone(),
            false,
            Some(cible),
            crate::routes::system::scan::ORIGINE_EXTRACTION_CD,
        )
        .await
    }
}

/// L'hôte des rayons de Tune Circle (T3, #5326) : ce que le greffon ne peut pas
/// juger seul, sans copie de la logique du serveur.
#[cfg(feature = "circle")]
struct HoteDuCercle {
    state: AppState,
}

#[cfg(feature = "circle")]
#[async_trait::async_trait]
impl tune_circle::ensembles::Hote for HoteDuCercle {
    /// Le même extracteur que toutes les routes : `X-Profile-Id` permis,
    /// appelant authentifié, puis profil global.
    async fn profil_actif(&self, parts: &mut axum::http::request::Parts) -> i64 {
        use axum::extract::FromRequestParts;
        match tune_http_types::ActiveProfile::from_request_parts(parts, &self.state).await {
            Ok(p) => p.id(),
            Err(never) => match never {},
        }
    }

    async fn collection_intelligente(
        &self,
        id: i64,
        profile_id: i64,
    ) -> Result<Option<tune_circle::ensembles::Membres>, String> {
        use axum::extract::FromRef;
        let smart = tune_smart_http::SmartHttpState::from_ref(&self.state);
        let Some(c) =
            tune_smart_http::smart_collections::resoudre_pour_partage(&smart, id, profile_id)
                .await?
        else {
            return Ok(None);
        };
        Ok(Some(membres_de_collection(c)))
    }
}

/// Une collection résolue, dans la forme du greffon : les albums de la
/// bibliothèque par identifiant, ceux des services en références.
#[cfg(feature = "circle")]
fn membres_de_collection(
    c: tune_smart_http::smart_collections::CollectionPartagee,
) -> tune_circle::ensembles::Membres {
    use tune_smart_http::smart_collections::AlbumPartage;
    let mut m = tune_circle::ensembles::Membres {
        nom: c.nom,
        ..Default::default()
    };
    for a in c.albums {
        match a {
            AlbumPartage::Local(id) => m.albums.push(id),
            AlbumPartage::DeService {
                source,
                source_id,
                title,
                artist_name,
            } => m.references.push(tune_circle::ensembles::Reference {
                genre: "album".into(),
                titre: title,
                artiste: artist_name,
                album: None,
                service: source,
                id_de_service: source_id,
            }),
        }
    }
    m
}
