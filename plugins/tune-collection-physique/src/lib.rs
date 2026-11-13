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
//! | `GET /collection[?format=&etat=&dossier=&genre=&style=]` | les disques, leur état (et pour « nulle part » sa `raison`, #7981), les compteurs (par état, raison, format, dossier, genre, style) |
//! | `GET /parcourir` | les dossiers Discogs, les genres et les styles de la collection, avec leurs compteurs (#7929) |
//! | `GET /disques/{release}/services` | « Chercher sur les services » : les albums les plus proches chez chaque service connecté (#7928) |
//! | `GET /disques/{release}/albums[?q=]` | « Associer à un album » (#8672) : la recherche dans la bibliothèque (préremplie artiste + titre) et, pour « plusieurs correspondants », les candidats |
//! | `PUT /disques/{release}/association` | `{album_id}` : associe le disque à cet album, à la main (#8672) |
//! | `DELETE /disques/{release}/association` | « Dissocier » : le disque revient au rapprochement automatique (#8672) |
//! | `GET /cd?mb_release=&code_barre=` | un CD inséré est-il dans la collection, et copié ? |
//! | `GET /pochettes/{release}[?taille=originale]` | la pochette, servie depuis le cache local (`ETag`, 304) |
//! | `DELETE /cache[?quoi=pochettes]` | efface le cache : pochettes et mémoire, ou les pochettes seules |
//!
//! # Le cache (01/11/2026)
//!
//! Rien de ce que l'écran affiche ne vient de Discogs au moment où il
//! l'affiche : la liste, les fiches et les liens sont dans la mémoire
//! (`collection.json`, relue au démarrage), les pochettes dans `pochettes/`
//! ([`pochettes`]). Une synchronisation ne relit que le haut de la liste
//! quand le reste n'a pas bougé (`instance_id`, `date_added`).
//!
//! Refus : `{"error": "collection_physique.<code>"}`. Un code, jamais une
//! phrase : l'écran traduit. Chaque réponse de collection porte
//! l'attribution que demandent les conditions de Discogs.
//!
//! Gratuit. Opt-in : il dort tant que l'utilisateur ne l'a pas installé
//! depuis le catalogue (Réglages › Extensions).

pub mod associations;
pub mod correspondance;
pub mod discogs;
pub mod memoire;
pub mod pochettes;
pub mod recherche;
pub mod synchro;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

use crate::correspondance::{AlbumLocal, Index, Par, Rapprochement};
use crate::discogs::{Disque, Famille, code_barre_normalise, nom_utilisateur_valide};
use crate::memoire::Memoire;
use crate::synchro::code;
pub use crate::synchro::{Cadence, Hote, Image, Jeton, Moteur, Panne, Reponse};

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
        "pochettes": etat.moteur.etat_pochettes(),
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

/// Pourquoi un disque est « nulle part » (#7981), pour que l'écran le dise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Raison {
    /// Ni la bibliothèque ni les services interrogés n'ont d'album qui
    /// réponde.
    AucunCandidat,
    /// Plusieurs albums de la bibliothèque répondent : on ne choisit pas.
    PlusieursCandidats,
    /// Aucun service de streaming connecté : seule la bibliothèque a été
    /// regardée.
    ServiceNonInterroge,
}

impl Raison {
    const TOUTES: [Raison; 3] = [
        Raison::AucunCandidat,
        Raison::PlusieursCandidats,
        Raison::ServiceNonInterroge,
    ];

    fn code(self) -> &'static str {
        match self {
            Raison::AucunCandidat => "aucun_candidat",
            Raison::PlusieursCandidats => "plusieurs_candidats",
            Raison::ServiceNonInterroge => "service_non_interroge",
        }
    }
}

/// Un album local tel que l'écran le lit.
fn album_json(a: &AlbumLocal, par: Option<Par>) -> Value {
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
}

/// Les associations manuelles (#8672). Illisibles : aucune, et on le dit au
/// journal — l'écran de la collection ne tombe pas pour autant.
fn associations_manuelles(etat: &Etat) -> HashMap<u64, i64> {
    associations::lire(&etat.backend).unwrap_or_else(|e| {
        tracing::warn!(target: "plugin", erreur = %e, "collection_physique_associations_illisibles");
        HashMap::new()
    })
}

/// Un disque tel que l'écran le lit, et son état.
fn vue(
    d: &Disque,
    m: &Memoire,
    index: &Index<'_>,
    services: &[String],
    manuelles: &HashMap<u64, i64>,
) -> (Value, EtatDisque) {
    let codes = m
        .fiches
        .get(&d.release_id)
        .map(|f| f.codes_barres.clone())
        .unwrap_or_default();
    let mb = m.liens_mb.get(&d.release_id).map(|l| &l.liens);
    let rapprochement = index.rapprocher(d, &codes, mb);
    // L'association manuelle prime (#8672) ; vers un album qui a quitté la
    // bibliothèque, elle ne compte pas.
    let manuel = manuelles.get(&d.release_id).and_then(|&id| index.album(id));
    let local = manuel
        .map(|a| (a, Par::Manuel))
        .or_else(|| rapprochement.trouve());
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
    let raison = (etat == EtatDisque::NullePart).then_some(match rapprochement {
        Rapprochement::Plusieurs(_) => Raison::PlusieursCandidats,
        _ if services.is_empty() => Raison::ServiceNonInterroge,
        _ => Raison::AucunCandidat,
    });
    let copie = d.famille.se_copie().then_some(local.is_some());
    let bibliotheque = local.map(|(a, par)| album_json(a, Some(par)));
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
        // La pochette est servie par `GET /pochettes/{release_id}`, depuis
        // le cache local : l'adresse Discogs ne sort plus vers l'écran.
        "pochette": d.pochette.is_some() || d.vignette.is_some(),
        "bibliotheque": bibliotheque,
        "streaming": trouves,
        "etat": etat.code(),
        // « nulle part » seulement : pourquoi (#7981) ; `null` sinon.
        "raison": raison.map(Raison::code),
        "candidats_locaux": match rapprochement {
            Rapprochement::Plusieurs(n) if manuel.is_none() => Some(n),
            _ => None,
        },
        "copie": copie,
        "proposer_extraction": copie == Some(false),
        "dossier": d.dossier,
        "genres": d.genres,
        "styles": d.styles,
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
    /// L'identifiant d'un dossier Discogs (#7929).
    dossier: Option<u64>,
    genre: Option<String>,
    style: Option<String>,
}

/// Le disque est-il dans ce genre / ce style ? Sans filtre, oui.
fn porte(noms: &[String], filtre: Option<&str>) -> bool {
    filtre.is_none_or(|f| noms.iter().any(|n| n.eq_ignore_ascii_case(f)))
}

/// Les compteurs de navigation (#7929) : par dossier (avec son nom), par
/// genre et par style, comptés sur les EXEMPLAIRES de la liste comme les
/// autres compteurs, du plus fourni au moins fourni.
fn compteurs_de_navigation(m: &Memoire) -> Value {
    let mut dossiers: std::collections::BTreeMap<u64, u64> = Default::default();
    let mut genres: std::collections::BTreeMap<String, u64> = Default::default();
    let mut styles: std::collections::BTreeMap<String, u64> = Default::default();
    for d in &m.disques {
        if let Some(id) = d.dossier {
            *dossiers.entry(id).or_default() += 1;
        }
        for g in &d.genres {
            *genres.entry(g.clone()).or_default() += 1;
        }
        for s in &d.styles {
            *styles.entry(s.clone()).or_default() += 1;
        }
    }
    let mut par_dossier: Vec<Value> = dossiers
        .iter()
        .map(|(id, n)| {
            let nom = m
                .dossiers
                .iter()
                .find(|f| f.id == *id)
                .map(|f| f.nom.clone());
            json!({"id": id, "nom": nom, "nombre": n})
        })
        .collect();
    // Un dossier vide chez Discogs reste montré (0) : l'utilisateur l'a créé.
    for f in &m.dossiers {
        if !dossiers.contains_key(&f.id) {
            par_dossier.push(json!({"id": f.id, "nom": f.nom, "nombre": 0}));
        }
    }
    par_dossier.sort_by(|a, b| {
        b["nombre"]
            .as_u64()
            .cmp(&a["nombre"].as_u64())
            .then_with(|| a["id"].as_u64().cmp(&b["id"].as_u64()))
    });
    let liste = |t: std::collections::BTreeMap<String, u64>| {
        let mut v: Vec<(String, u64)> = t.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter()
            .map(|(nom, n)| json!({"nom": nom, "nombre": n}))
            .collect::<Vec<_>>()
    };
    json!({
        "par_dossier": par_dossier,
        "par_genre": liste(genres),
        "par_style": liste(styles),
    })
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
    let genre = f.genre.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let style = f.style.as_deref().map(str::trim).filter(|s| !s.is_empty());
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
    let manuelles = associations_manuelles(&etat);
    let mut par_etat = serde_json::Map::new();
    let mut par_format = serde_json::Map::new();
    for e in EtatDisque::TOUS {
        par_etat.insert(e.code().into(), json!(0));
    }
    for fam in Famille::TOUTES {
        par_format.insert(fam.code().into(), json!(0));
    }
    let mut par_raison = serde_json::Map::new();
    for r in Raison::TOUTES {
        par_raison.insert(r.code().into(), json!(0));
    }
    let mut cd_non_copies = 0u64;
    let navigation = compteurs_de_navigation(&m);
    let mut disques = Vec::new();
    for d in &m.disques {
        let (v, e) = vue(d, &m, &index, &services, &manuelles);
        incrementer(&mut par_etat, e.code());
        incrementer(&mut par_format, d.famille.code());
        if let Some(r) = v["raison"].as_str() {
            incrementer(&mut par_raison, r);
        }
        if v["proposer_extraction"] == true {
            cd_non_copies += 1;
        }
        if famille.is_some_and(|fam| fam != d.famille)
            || filtre_etat.is_some_and(|fe| fe != e)
            || f.dossier.is_some_and(|id| d.dossier != Some(id))
            || !porte(&d.genres, genre)
            || !porte(&d.styles, style)
        {
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
            "par_raison": par_raison,
            "cd_non_copies": cd_non_copies,
            "par_dossier": navigation["par_dossier"],
            "par_genre": navigation["par_genre"],
            "par_style": navigation["par_style"],
        },
        "disques": disques,
        "attribution": attribution(),
    }))
    .into_response()
}

/// `GET /parcourir` (#7929) : les dossiers, genres et styles de la
/// collection, avec leurs compteurs, pour les filtres de l'écran.
async fn parcourir(State(etat): State<Etat>) -> Response {
    let nom = nom_suivi(&etat.backend);
    let m = etat.moteur.memoire();
    if nom.is_none() || m.nom_utilisateur != nom {
        return Json(json!({"par_dossier": [], "par_genre": [], "par_style": []})).into_response();
    }
    Json(compteurs_de_navigation(&m)).into_response()
}

/// `GET /disques/{release}/services` (#7928) : « Chercher sur les
/// services ». 404 `disque_inconnu` pour une release hors de la liste.
async fn chercher_sur_les_services(State(etat): State<Etat>, Path(release): Path<u64>) -> Response {
    let nom = nom_suivi(&etat.backend);
    if nom.is_none() || etat.moteur.memoire().nom_utilisateur != nom {
        return refus(StatusCode::NOT_FOUND, "disque_inconnu");
    }
    let Some(r) = etat.moteur.chercher_sur_les_services(release).await else {
        return refus(StatusCode::NOT_FOUND, "disque_inconnu");
    };
    Json(json!({
        "release_id": release,
        "services": r.services,
        "sans_reponse": r.sans_reponse,
        "meilleure": r.meilleure(),
        "propositions": r.propositions,
    }))
    .into_response()
}

/// Le disque `release` de la collection suivie, s'il y est.
fn disque_suivi(etat: &Etat, release: u64) -> Option<(Memoire, Disque)> {
    let nom = nom_suivi(&etat.backend);
    let m = etat.moteur.memoire();
    if nom.is_none() || m.nom_utilisateur != nom {
        return None;
    }
    let d = m.disques.iter().find(|d| d.release_id == release)?.clone();
    Some((m, d))
}

/// La recherche que « Associer à un album » propose : le premier artiste
/// (sauf « Various ») et le titre sans sa mention d'édition. Le premier
/// seulement : la recherche veut TOUS les mots, et l'album local ne porte
/// souvent que l'artiste principal (« Janice McClain / The Ritchie Family »).
fn requete_proposee(d: &Disque) -> String {
    let titre = correspondance::titre_sans_edition(&d.titre).trim();
    let artiste = d
        .artistes
        .first()
        .map(String::as_str)
        .unwrap_or(&d.artiste)
        .trim();
    if artiste.is_empty() || correspondance::artiste_normalise(artiste) == "various" {
        titre.to_string()
    } else {
        format!("{artiste} {titre}")
    }
}

#[derive(Deserialize)]
struct QuestionAlbums {
    q: Option<String>,
}

/// `GET /disques/{release}/albums[?q=]` (#8672) : « Associer à un album ».
/// `requete` : la recherche faite (`q`, sinon celle que l'écran propose,
/// artiste + titre) ; `albums` : ses résultats dans la bibliothèque ;
/// `candidats` : pour « plusieurs correspondants », les albums entre
/// lesquels le rapprochement n'a pas choisi (vide sinon).
async fn albums_a_associer(
    State(etat): State<Etat>,
    Path(release): Path<u64>,
    Query(q): Query<QuestionAlbums>,
) -> Response {
    let Some((_, d)) = disque_suivi(&etat, release) else {
        return refus(StatusCode::NOT_FOUND, "disque_inconnu");
    };
    let albums = match albums_et_services(&etat).await {
        Ok((a, _)) => a,
        Err(r) => return r.into_response(),
    };
    let index = Index::new(&albums);
    let candidats = index.candidats(&d);
    let candidats: Vec<Value> = if candidats.len() > 1 {
        candidats.iter().map(|a| album_json(a, None)).collect()
    } else {
        Vec::new()
    };
    let requete =
        q.q.map(|s| s.trim().to_string())
            .unwrap_or_else(|| requete_proposee(&d));
    let trouves: Vec<Value> = associations::chercher(&albums, &requete)
        .into_iter()
        .map(|a| album_json(a, None))
        .collect();
    Json(json!({
        "release_id": release,
        "requete": requete,
        "proposee": requete_proposee(&d),
        "candidats": candidats,
        "albums": trouves,
        "association": associations_manuelles(&etat).get(&release),
    }))
    .into_response()
}

/// Le disque tel que l'écran le lit, après une association ou une
/// dissociation.
async fn disque_relu(etat: &Etat, release: u64) -> Response {
    let Some((m, d)) = disque_suivi(etat, release) else {
        return refus(StatusCode::NOT_FOUND, "disque_inconnu");
    };
    let (albums, services) = match albums_et_services(etat).await {
        Ok(x) => x,
        Err(r) => return r.into_response(),
    };
    let index = Index::new(&albums);
    let (v, _) = vue(&d, &m, &index, &services, &associations_manuelles(etat));
    Json(json!({"disque": v, "attribution": attribution()})).into_response()
}

#[derive(Deserialize)]
struct CorpsAssociation {
    album_id: i64,
}

/// `PUT /disques/{release}/association` (#8672) : `{album_id}`. 404
/// `disque_inconnu` hors de la collection, 404 `album_inconnu` pour un album
/// qui n'est pas (ou plus) un album local de la bibliothèque.
async fn associer(
    State(etat): State<Etat>,
    Path(release): Path<u64>,
    corps: axum::body::Bytes,
) -> Response {
    let Ok(c) = serde_json::from_slice::<CorpsAssociation>(&corps) else {
        return refus(StatusCode::UNPROCESSABLE_ENTITY, "corps_invalide");
    };
    if disque_suivi(&etat, release).is_none() {
        return refus(StatusCode::NOT_FOUND, "disque_inconnu");
    }
    let albums = match albums_et_services(&etat).await {
        Ok((a, _)) => a,
        Err(r) => return r.into_response(),
    };
    if !albums.iter().any(|a| a.id == c.album_id) {
        return refus(StatusCode::NOT_FOUND, "album_inconnu");
    }
    if let Err(e) =
        associations::associer(&etat.backend, release, c.album_id, synchro::maintenant())
    {
        tracing::warn!(target: "plugin", erreur = %e, "collection_physique_association_non_ecrite");
        return refus(StatusCode::INTERNAL_SERVER_ERROR, code::ECRITURE);
    }
    disque_relu(&etat, release).await
}

/// `DELETE /disques/{release}/association` (#8672) : « Dissocier ». Le
/// disque revient au rapprochement automatique. Sans association, rien à
/// défaire : la réponse est la même.
async fn dissocier(State(etat): State<Etat>, Path(release): Path<u64>) -> Response {
    if disque_suivi(&etat, release).is_none() {
        return refus(StatusCode::NOT_FOUND, "disque_inconnu");
    }
    if let Err(e) = associations::dissocier(&etat.backend, release) {
        tracing::warn!(target: "plugin", erreur = %e, "collection_physique_dissociation_non_ecrite");
        return refus(StatusCode::INTERNAL_SERVER_ERROR, code::ECRITURE);
    }
    disque_relu(&etat, release).await
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
    let (v, _) = vue(d, &m, &index, &services, &associations_manuelles(&etat));
    Json(json!({
        "dans_la_collection": true,
        "copie": v["copie"] == true,
        "disque": v,
        "attribution": attribution(),
    }))
    .into_response()
}

/// `no-store` partout, sauf là où la route a posé sa propre politique
/// (les pochettes).
async fn sans_cache(mut r: Response) -> Response {
    if !r.headers().contains_key(header::CACHE_CONTROL) {
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    r
}

/// Une pochette se garde un jour sans revalider, puis se revalide par son
/// `ETag` (304, aucun octet).
pub const CACHE_POCHETTE: &str = "private, max-age=86400";

#[derive(Deserialize)]
struct QuestionPochette {
    taille: Option<String>,
}

/// L'`ETag` d'un fichier rangé : sa taille et sa date d'écriture. Une
/// pochette réécrite (cache effacé puis rangé de nouveau) change d'`ETag`.
fn etag(meta: &std::fs::Metadata) -> String {
    let date = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", meta.len(), date)
}

/// `GET /pochettes/{release}` : la pochette depuis le cache local,
/// téléchargée d'abord (par la file) si elle n'y est pas encore.
async fn pochette(
    State(etat): State<Etat>,
    Path(release): Path<u64>,
    Query(q): Query<QuestionPochette>,
    entetes: axum::http::HeaderMap,
) -> Response {
    let taille = match q.taille.as_deref() {
        None | Some("") | Some("vignette") => pochettes::Taille::Vignette,
        Some("originale") => pochettes::Taille::Originale,
        Some(_) => return refus(StatusCode::UNPROCESSABLE_ENTITY, "taille_inconnue"),
    };
    let chemin = match etat.moteur.pochette(release, taille).await {
        Ok(c) => c,
        Err(pochettes::Absence::Introuvable) => {
            return refus(StatusCode::NOT_FOUND, "pochette_absente");
        }
        // #7927 : le CDN freine ou refuse. L'écran le dit et réessaie
        // après `Retry-After`, au lieu de garder une case vide.
        Err(pochettes::Absence::EnAttente(reste)) => {
            let mut r = refus(StatusCode::SERVICE_UNAVAILABLE, "pochettes_limitees");
            if let Some(s) = reste
                && let Ok(v) = HeaderValue::from_str(&s.to_string())
            {
                r.headers_mut().insert(header::RETRY_AFTER, v);
            }
            return r;
        }
    };
    let Ok(meta) = tokio::fs::metadata(&chemin).await else {
        return refus(StatusCode::NOT_FOUND, "pochette_absente");
    };
    let tag = etag(&meta);
    let communs = [
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_POCHETTE),
        ),
        (
            header::ETAG,
            HeaderValue::from_str(&tag).unwrap_or_else(|_| HeaderValue::from_static("\"0\"")),
        ),
    ];
    let deja = entetes
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim() == tag || v.trim() == "*");
    if deja {
        return (StatusCode::NOT_MODIFIED, communs).into_response();
    }
    let Ok(octets) = tokio::fs::read(&chemin).await else {
        return refus(StatusCode::NOT_FOUND, "pochette_absente");
    };
    let type_mime = match chemin.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    };
    (
        StatusCode::OK,
        communs,
        [(header::CONTENT_TYPE, HeaderValue::from_static(type_mime))],
        octets,
    )
        .into_response()
}

#[derive(Deserialize)]
struct QuoiEffacer {
    quoi: Option<String>,
}

/// `DELETE /cache` : « Effacer le cache » des réglages du greffon.
/// `?quoi=pochettes` n'efface que les pochettes ; sans paramètre, la
/// mémoire aussi (la synchronisation suivante relit tout chez Discogs).
async fn effacer_cache(State(etat): State<Etat>, Query(q): Query<QuoiEffacer>) -> Response {
    let tout = match q.quoi.as_deref() {
        None | Some("") | Some("tout") => true,
        Some("pochettes") => false,
        Some(_) => return refus(StatusCode::UNPROCESSABLE_ENTITY, "quoi_inconnu"),
    };
    if let Err(e) = etat.moteur.effacer_le_cache(tout) {
        tracing::warn!(target: "plugin", erreur = %e, "collection_physique_cache_non_efface");
        return refus(StatusCode::INTERNAL_SERVER_ERROR, code::ECRITURE);
    }
    Json(json!({"efface": if tout { "tout" } else { "pochettes" }})).into_response()
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
        .route("/parcourir", get(parcourir))
        .route(
            "/disques/{release}/services",
            get(chercher_sur_les_services),
        )
        .route("/disques/{release}/albums", get(albums_a_associer))
        .route(
            "/disques/{release}/association",
            put(associer).delete(dissocier),
        )
        .route("/pochettes/{release}", get(pochette))
        .route("/cache", delete(effacer_cache))
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
            // Les pochettes qui manquent encore se rangent (reprise après
            // un redémarrage).
            moteur.lancer_pochettes();
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
