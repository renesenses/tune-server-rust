//! « Ma collection physique » — greffon natif du catalogue (#7195, go de
//! Bertrand du 27/10/2026).
//!
//! La collection Discogs de l'utilisateur (vinyles, CD, SACD, cassettes…),
//! comparée à sa bibliothèque et aux services de streaming. Pour chaque
//! disque : « dans ma bibliothèque » (avec la qualité, et pour un CD s'il est
//! copié), « en streaming » (service, album), ou « nulle part ».
//!
//! # Le compte et le jeton
//!
//! * Collection PUBLIQUE : le nom d'utilisateur Discogs suffit.
//! * Collection PRIVÉE : le jeton personnel Discogs de l'utilisateur, saisi
//!   dans Réglages › Clés d'API (#4805) et rangé SCELLÉ dans le coffre de
//!   Tune (`tune_core::cles_api`, `tune_core::coffre`). Ce greffon ne voit
//!   jamais sa valeur : l'hôte l'ajoute à chaque appel Discogs. Aucune route
//!   d'ici ne la rend, aucune ne la journalise ; `GET /reglages` dit
//!   seulement QUEL jeton existe (`aucun`, `integre`, `personnel`).
//!
//! # Les routes
//!
//! Montées par l'hôte sous `/api/v1/ext/collection-physique` :
//!
//! | Route | Rôle |
//! |---|---|
//! | `GET /reglages` | `{nom_utilisateur, jeton}` |
//! | `PUT /reglages` | `{nom_utilisateur}` (`null` : oublier le compte) |
//! | `POST /reglages/identite` | le nom du compte du jeton personnel, retenu |
//! | `POST /synchroniser` | lance la synchronisation (202) |
//! | `GET /etat` | l'avancement (liste, streaming, bibliothèque), l'affinage MusicBrainz `{en_cours, restants}`, la dernière synchronisation, la dernière erreur |
//! | `GET /collection[?format=&etat=]` | les disques, leur état, les compteurs |
//! | `GET /cd?mb_release=&code_barre=` | un CD inséré est-il dans la collection, et copié ? |
//!
//! Refus : `{"error": "collection_physique.<code>"}`. Un code, jamais une
//! phrase : l'écran traduit. Chaque réponse de collection porte
//! l'attribution que demandent les conditions de Discogs.
//!
//! Gratuit. Opt-in : il dort tant que l'utilisateur ne l'a pas installé
//! depuis le catalogue (Réglages › Extensions).

pub mod correspondance;
pub mod discogs;
pub mod memoire;
pub mod synchro;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

use crate::correspondance::{AlbumLocal, Index};
use crate::discogs::{Disque, Famille, code_barre_normalise, nom_utilisateur_valide};
use crate::memoire::Memoire;
use crate::synchro::code;
pub use crate::synchro::{Cadence, Hote, Jeton, Moteur, Panne, Reponse};

/// Le nom du greffon, et donc son préfixe `/api/v1/ext/collection-physique`.
pub const NOM: &str = "collection-physique";

/// Le réglage du nom d'utilisateur Discogs suivi.
pub const REGLAGE_NOM: &str = "collection_physique_nom_discogs";

/// L'attribution que demandent les conditions d'usage de l'API Discogs.
pub const ATTRIBUTION: &str = "Data provided by Discogs";
pub const LIEN_DISCOGS: &str = "https://www.discogs.com";

/// La première synchronisation après le démarrage, puis tous les jours.
pub const PREMIERE_SYNCHRO: Duration = Duration::from_secs(120);
pub const SYNCHRO_TOUS_LES: Duration = Duration::from_secs(24 * 3600);

/// Ce que l'hôte fournit.
#[derive(Clone)]
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    pub hote: Arc<dyn Hote>,
}

#[derive(Clone)]
struct Etat {
    backend: Arc<dyn DbBackend>,
    moteur: Arc<Moteur>,
}

/// Le nom d'utilisateur suivi, s'il y en a un.
pub fn nom_suivi(backend: &Arc<dyn DbBackend>) -> Option<String> {
    SettingsRepo::with_backend(backend.clone())
        .get(REGLAGE_NOM)
        .ok()
        .flatten()
        .map(|n| n.trim().to_string())
        .filter(|n| nom_utilisateur_valide(n))
}

fn refus(statut: StatusCode, code: &str) -> Response {
    (
        statut,
        Json(json!({"error": format!("collection_physique.{code}")})),
    )
        .into_response()
}

fn reglages(etat: &Etat) -> Response {
    Json(json!({
        "nom_utilisateur": nom_suivi(&etat.backend),
        "jeton": etat.moteur.hote.jeton_discogs(),
    }))
    .into_response()
}

async fn lire_reglages(State(etat): State<Etat>) -> Response {
    reglages(&etat)
}

/// Un refus à rendre : son statut et son code. Petit, pour qu'un `Result`
/// qui le porte reste petit (`clippy::result_large_err`).
struct Refus(StatusCode, &'static str);

impl IntoResponse for Refus {
    fn into_response(self) -> Response {
        refus(self.0, self.1)
    }
}

/// Retient `nom` (ou l'oublie), et oublie la liste d'un autre compte.
fn retenir(etat: &Etat, nom: Option<String>) -> Result<(), Refus> {
    let s = SettingsRepo::with_backend(etat.backend.clone());
    let ecrit = match &nom {
        Some(n) => s.set(REGLAGE_NOM, n),
        None => s.delete(REGLAGE_NOM),
    };
    ecrit
        .and_then(|_| etat.moteur.changer_de_compte(nom))
        .map_err(|e| {
            tracing::warn!(target: "plugin", erreur = %e, "collection_physique_reglage_non_ecrit");
            Refus(StatusCode::INTERNAL_SERVER_ERROR, code::ECRITURE)
        })
}

#[derive(Deserialize)]
struct CorpsReglages {
    nom_utilisateur: Option<String>,
}

async fn poser_reglages(State(etat): State<Etat>, corps: axum::body::Bytes) -> Response {
    let Ok(c) = serde_json::from_slice::<CorpsReglages>(&corps) else {
        return refus(StatusCode::UNPROCESSABLE_ENTITY, "corps_invalide");
    };
    let nom = c
        .nom_utilisateur
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    if let Some(n) = &nom
        && !nom_utilisateur_valide(n)
    {
        return refus(StatusCode::UNPROCESSABLE_ENTITY, "nom_invalide");
    }
    if let Err(r) = retenir(&etat, nom) {
        return r.into_response();
    }
    reglages(&etat)
}

async fn identite(State(etat): State<Etat>) -> Response {
    match etat.moteur.identite().await {
        Ok(nom) => {
            if let Err(r) = retenir(&etat, Some(nom)) {
                return r.into_response();
            }
            reglages(&etat)
        }
        Err(c @ code::JETON_PERSONNEL_ABSENT) => refus(StatusCode::CONFLICT, c),
        Err(c @ code::JETON_REFUSE) => refus(StatusCode::UNPROCESSABLE_ENTITY, c),
        Err(c) => refus(StatusCode::BAD_GATEWAY, c),
    }
}

async fn synchroniser(State(etat): State<Etat>) -> Response {
    let Some(nom) = nom_suivi(&etat.backend) else {
        return refus(StatusCode::CONFLICT, code::AUCUN_COMPTE);
    };
    let lance = etat.moteur.lancer(nom);
    (
        StatusCode::ACCEPTED,
        Json(json!({"lance": lance, "avancement": etat.moteur.avancement()})),
    )
        .into_response()
}

/// L'affinage MusicBrainz pour l'écran : `{en_cours, restants}`.
fn affinage(etat: &Etat, a_jour: bool) -> Value {
    let a = etat.moteur.affinage();
    json!({
        "en_cours": a.en_cours,
        "restants": if a_jour { a.restants } else { 0 },
    })
}

async fn lire_etat(State(etat): State<Etat>) -> Response {
    let m = etat.moteur.memoire();
    let nom = nom_suivi(&etat.backend);
    let a_jour = nom.is_some() && m.nom_utilisateur == nom;
    Json(json!({
        "nom_utilisateur": nom,
        "avancement": etat.moteur.avancement(),
        "disques": if a_jour { m.disques.len() } else { 0 },
        "tronquee": a_jour && m.tronquee,
        "liste_lue_le": if a_jour { m.liste_lue_le } else { None },
        "derniere_synchro": if a_jour { m.derniere_synchro } else { None },
        "derniere_erreur": m.derniere_erreur,
        "affinage_musicbrainz": affinage(&etat, a_jour),
    }))
    .into_response()
}

/// L'état d'un disque à l'écran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EtatDisque {
    Bibliotheque,
    Streaming,
    NullePart,
    /// Pas dans la bibliothèque, et les services connectés n'ont pas encore
    /// TOUS répondu pour lui : on ne dit pas « nulle part » trop tôt.
    AVerifier,
}

impl EtatDisque {
    const TOUS: [EtatDisque; 4] = [
        EtatDisque::Bibliotheque,
        EtatDisque::Streaming,
        EtatDisque::NullePart,
        EtatDisque::AVerifier,
    ];

    fn code(self) -> &'static str {
        match self {
            EtatDisque::Bibliotheque => "bibliotheque",
            EtatDisque::Streaming => "streaming",
            EtatDisque::NullePart => "nulle_part",
            EtatDisque::AVerifier => "a_verifier",
        }
    }
}

/// Un disque tel que l'écran le lit, et son état.
fn vue(d: &Disque, m: &Memoire, index: &Index<'_>, services: &[String]) -> (Value, EtatDisque) {
    let codes = m
        .fiches
        .get(&d.release_id)
        .map(|f| f.codes_barres.clone())
        .unwrap_or_default();
    let mb = m.liens_mb.get(&d.release_id).map(|l| &l.liens);
    let local = index.trouver(d, &codes, mb);
    let streaming = m.streaming.get(&d.release_id);
    let trouves = streaming.map(|s| s.trouves.clone()).unwrap_or_default();
    let etat = if local.is_some() {
        EtatDisque::Bibliotheque
    } else if !trouves.is_empty() {
        EtatDisque::Streaming
    } else if services.is_empty() || streaming.is_some_and(|s| s.services == services) {
        EtatDisque::NullePart
    } else {
        EtatDisque::AVerifier
    };
    let copie = d.famille.se_copie().then_some(local.is_some());
    let bibliotheque = local.map(|(a, par)| {
        json!({
            "album_id": a.id,
            "titre": a.titre,
            "artiste": a.artiste,
            "qualite": a.qualite,
            "format": a.format,
            "sample_rate": a.sample_rate,
            "bit_depth": a.bit_depth,
            "cover_path": a.pochette,
            "par": par,
        })
    });
    let v = json!({
        "instance_id": d.instance_id,
        "release_id": d.release_id,
        "master_id": d.master_id,
        "artiste": d.artiste,
        "titre": d.titre,
        "annee": d.annee,
        "labels": d.labels,
        "numeros_catalogue": d.numeros_catalogue,
        "formats": d.formats,
        "precisions": d.precisions,
        "format": d.famille.code(),
        "codes_barres": codes,
        "ajoute_le": d.ajoute_le,
        "lien_discogs": d.lien_discogs(),
        "vignette": d.vignette,
        "bibliotheque": bibliotheque,
        "streaming": trouves,
        "etat": etat.code(),
        "copie": copie,
        "proposer_extraction": copie == Some(false),
    });
    (v, etat)
}

fn attribution() -> Value {
    json!({"texte": ATTRIBUTION, "lien": LIEN_DISCOGS})
}

#[derive(Deserialize)]
struct FiltresCollection {
    format: Option<String>,
    etat: Option<String>,
}

async fn albums_et_services(etat: &Etat) -> Result<(Vec<AlbumLocal>, Vec<String>), Refus> {
    let albums = etat.moteur.hote.albums_locaux().await.map_err(|e| {
        tracing::warn!(target: "plugin", erreur = %e, "collection_physique_bibliotheque_illisible");
        Refus(
            StatusCode::INTERNAL_SERVER_ERROR,
            code::BIBLIOTHEQUE_ILLISIBLE,
        )
    })?;
    let connectes = etat.moteur.hote.services_connectes().await;
    let services = synchro::SERVICES
        .iter()
        .filter(|s| connectes.iter().any(|c| c == *s))
        .map(|s| s.to_string())
        .collect();
    Ok((albums, services))
}

async fn collection(State(etat): State<Etat>, Query(f): Query<FiltresCollection>) -> Response {
    let famille = match f.format.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(c) => match Famille::depuis_code(c) {
            Some(fam) => Some(fam),
            None => return refus(StatusCode::UNPROCESSABLE_ENTITY, "format_inconnu"),
        },
    };
    let filtre_etat = match f.etat.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(c) => match EtatDisque::TOUS.into_iter().find(|e| e.code() == c) {
            Some(e) => Some(e),
            None => return refus(StatusCode::UNPROCESSABLE_ENTITY, "etat_inconnu"),
        },
    };
    let nom = nom_suivi(&etat.backend);
    let m = etat.moteur.memoire();
    let statut = match &nom {
        None => "aucun_compte",
        Some(_) if m.nom_utilisateur != nom || m.liste_lue_le.is_none() => "a_synchroniser",
        Some(_) => "ok",
    };
    if statut != "ok" {
        return Json(json!({
            "statut": statut,
            "nom_utilisateur": nom,
            "disques": [],
            "compteurs": {"total": 0},
            "attribution": attribution(),
        }))
        .into_response();
    }
    let (albums, services) = match albums_et_services(&etat).await {
        Ok(x) => x,
        Err(r) => return r.into_response(),
    };
    let index = Index::new(&albums);
    let mut par_etat = serde_json::Map::new();
    let mut par_format = serde_json::Map::new();
    for e in EtatDisque::TOUS {
        par_etat.insert(e.code().into(), json!(0));
    }
    for fam in Famille::TOUTES {
        par_format.insert(fam.code().into(), json!(0));
    }
    let mut cd_non_copies = 0u64;
    let mut disques = Vec::new();
    for d in &m.disques {
        let (v, e) = vue(d, &m, &index, &services);
        incrementer(&mut par_etat, e.code());
        incrementer(&mut par_format, d.famille.code());
        if v["proposer_extraction"] == true {
            cd_non_copies += 1;
        }
        if famille.is_some_and(|fam| fam != d.famille) || filtre_etat.is_some_and(|fe| fe != e) {
            continue;
        }
        disques.push(v);
    }
    Json(json!({
        "statut": statut,
        "nom_utilisateur": nom,
        "services": services,
        "tronquee": m.tronquee,
        "liste_lue_le": m.liste_lue_le,
        "derniere_synchro": m.derniere_synchro,
        "affinage_musicbrainz": affinage(&etat, true),
        "compteurs": {
            "total": m.disques.len(),
            "par_etat": par_etat,
            "par_format": par_format,
            "cd_non_copies": cd_non_copies,
        },
        "disques": disques,
        "attribution": attribution(),
    }))
    .into_response()
}

fn incrementer(m: &mut serde_json::Map<String, Value>, cle: &str) {
    let n = m.get(cle).and_then(Value::as_u64).unwrap_or(0) + 1;
    m.insert(cle.to_string(), json!(n));
}

#[derive(Deserialize)]
struct QuestionCd {
    mb_release: Option<String>,
    code_barre: Option<String>,
}

/// Un CD inséré est-il un disque de la collection, et est-il déjà copié ?
/// L'écran du CD s'en sert pour proposer l'extraction d'un disque possédé
/// mais pas encore copié.
async fn cd(State(etat): State<Etat>, Query(q): Query<QuestionCd>) -> Response {
    let mb = q
        .mb_release
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let code = q.code_barre.as_deref().and_then(code_barre_normalise);
    if mb.is_none() && code.is_none() {
        return refus(StatusCode::UNPROCESSABLE_ENTITY, "cd_sans_identifiant");
    }
    let nom = nom_suivi(&etat.backend);
    let m = etat.moteur.memoire();
    if nom.is_none() || m.nom_utilisateur != nom {
        return Json(json!({"dans_la_collection": false, "copie": false, "disque": null}))
            .into_response();
    }
    let trouve = m.disques.iter().find(|d| {
        d.famille.se_copie()
            && (mb.as_ref().is_some_and(|r| {
                m.liens_mb
                    .get(&d.release_id)
                    .is_some_and(|l| l.liens.releases.iter().any(|x| x == r))
            }) || code.as_ref().is_some_and(|c| {
                m.fiches.get(&d.release_id).is_some_and(|f| {
                    f.codes_barres
                        .iter()
                        .any(|x| code_barre_normalise(x).as_ref() == Some(c))
                })
            }))
    });
    let Some(d) = trouve else {
        return Json(json!({"dans_la_collection": false, "copie": false, "disque": null}))
            .into_response();
    };
    let (albums, services) = match albums_et_services(&etat).await {
        Ok(x) => x,
        Err(r) => return r.into_response(),
    };
    let index = Index::new(&albums);
    let (v, _) = vue(d, &m, &index, &services);
    Json(json!({
        "dans_la_collection": true,
        "copie": v["copie"] == true,
        "disque": v,
        "attribution": attribution(),
    }))
    .into_response()
}

async fn sans_cache(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Le routeur du greffon, prêt à être monté sous
/// `/api/v1/ext/collection-physique`.
pub fn router(backend: Arc<dyn DbBackend>, moteur: Arc<Moteur>) -> Router<()> {
    let etat = Etat { backend, moteur };
    Router::new()
        .route("/reglages", get(lire_reglages).put(poser_reglages))
        .route("/reglages/identite", post(identite))
        .route("/synchroniser", post(synchroniser))
        .route("/etat", get(lire_etat))
        .route("/collection", get(collection))
        .route("/cd", get(cd))
        .layer(middleware::map_response(sans_cache))
        .with_state(etat)
}

pub struct CollectionPhysiquePlugin {
    services: HostServices,
    cadence: Cadence,
    tache: Option<tokio::task::JoinHandle<()>>,
    moteur: Option<Arc<Moteur>>,
}

impl CollectionPhysiquePlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            services,
            cadence: Cadence::default(),
            tache: None,
            moteur: None,
        }
    }
}

#[async_trait]
impl TunePlugin for CollectionPhysiquePlugin {
    fn name(&self) -> &str {
        // En littéral : `scripts/plugin-public-catalog.py` le lit tel quel.
        "collection-physique"
    }

    fn display_name(&self) -> &str {
        "Ma collection physique"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn description(&self) -> &str {
        "Votre collection Discogs (vinyles, CD, cassettes) comparée à votre bibliothèque et à Qobuz et TIDAL"
    }

    /// Au catalogue, à installer : il appelle Discogs pour le compte de
    /// l'utilisateur, ce qui ne se fait pas sans qu'il l'ait demandé.
    fn default_enabled(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let dossier: PathBuf = ctx.data_dir.clone();
        let moteur = Arc::new(Moteur::new(
            self.services.hote.clone(),
            dossier,
            self.cadence.clone(),
        ));
        ctx.register_router(router(self.services.backend.clone(), moteur.clone()));
        let backend = self.services.backend.clone();
        self.moteur = Some(moteur.clone());
        self.tache = Some(tokio::spawn(async move {
            // L'affinage MusicBrainz reprend tout de suite où il s'était
            // arrêté ; la synchronisation, elle, attend un peu.
            moteur.lancer_affinage();
            tokio::time::sleep(PREMIERE_SYNCHRO).await;
            loop {
                if let Some(nom) = nom_suivi(&backend) {
                    moteur.lancer(nom);
                }
                tokio::time::sleep(SYNCHRO_TOUS_LES).await;
            }
        }));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        if let Some(t) = self.tache.take() {
            t.abort();
        }
        if let Some(m) = self.moteur.take() {
            m.arreter();
        }
        Ok(())
    }
}
