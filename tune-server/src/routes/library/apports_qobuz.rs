//! #8077 — compléter un album LOCAL depuis l'album Qobuz qui lui correspond,
//! sans jamais écraser une donnée locale, en base seulement.
//!
//! Le jugement (rapprochement, pistes, garde « compléter seulement ») est dans
//! [`tune_core::library::apports_service`], le stockage dans
//! [`tune_core::db::apports_service`]. Ici : lire l'album, interroger Qobuz
//! SOUS LE LIMITEUR ([`tune_core::http::fetch::QOBUZ`]), ranger, exposer.
//!
//! # Routes (voir `docs/apports-qobuz-contrat.md`)
//!
//! - `GET /library/albums/{id}/apports-qobuz` : le rapprochement et les
//!   apports ; calculés à la demande s'ils ne l'ont jamais été
//!   (`?rafraichir=true` : recalculés, sauf après un retrait).
//! - `DELETE /library/albums/{id}/apports-qobuz` : retire TOUS les apports
//!   Qobuz de l'album (champs et crédits) ; la passe de fond ne les remet pas.
//! - `POST /library/albums/{id}/apports-qobuz/confirmer` : applique un
//!   rapprochement `a_confirmer` (ou celui d'un album Qobuz désigné), et
//!   relève un retrait.
//! - `POST /library/apports-qobuz/passe` : lance la passe de fond.
//!
//! La fiche album (`GET /library/albums/{id}`) porte `apports_qobuz`, et
//! lance le calcul en arrière-plan si l'album n'a jamais été examiné ; les
//! pistes (`GET /library/albums/{id}/tracks`) portent leur `apports_qobuz`
//! (ISRC) ; les crédits repris sortent des routes de crédits avec
//! `source: "qobuz"`.
//!
//! Aucun appel sans compte Qobuz activé ET connecté.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::RwLock;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::apports_service::{self as stock, Rapprochement};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::models::{Album, Track};
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::http::fetch::{CLE_QOBUZ, QOBUZ};
use tune_core::library::apports_service::{
    self as ap, AlbumLocal, EtatLocalAlbum, EtatLocalPiste, Jugement, Methode, PisteLocale,
    PochetteLocale, Statut,
};
use tune_core::streaming::StreamingService;
use tune_core::streaming::traits::FicheAlbumService;
use tune_core::taches_de_fond::{Tache, attendre_son_tour};

use crate::state::AppState;

/// Le service interrogé.
pub(crate) const SERVICE: &str = "qobuz";

/// Délai au-delà duquel une demande à l'ouverture n'attend plus Qobuz.
const DELAI: Duration = Duration::from_secs(20);

/// L'identifiant de la passe de fond au registre des tâches.
pub(crate) const TACHE_APPORTS_QOBUZ: &str = "apports_qobuz";

/// Albums examinés au plus par passe de fond.
const ALBUMS_PAR_PASSE: i64 = 500;

/// Pause entre deux albums de la passe de fond, en plus du limiteur : la
/// passe laisse la place aux demandes de l'utilisateur.
const PAUSE_ENTRE_ALBUMS: Duration = Duration::from_secs(2);

type Service = Arc<RwLock<Box<dyn StreamingService>>>;

/// Les albums dont un calcul est en vol : deux ouvertures de la fiche ne
/// lancent qu'un calcul.
static EN_VOL: LazyLock<Mutex<HashSet<i64>>> = LazyLock::new(Mutex::default);

async fn service(state: &AppState) -> Option<Service> {
    let svc = state.services.lock().await.get(SERVICE)?;
    if !svc.read().await.utilisable().await {
        return None;
    }
    Some(svc)
}

/// Un appel Qobuz sous le limiteur partagé.
async fn sous_le_limiteur() {
    QOBUZ.acquire(CLE_QOBUZ).await;
}

// ── Ce que la bibliothèque sait déjà ─────────────────────────────────────

/// Les rôles crédités par une autre source que Qobuz, par piste : lignes de
/// `track_credits`, balises `performer` / `producer` de `track_metadata`,
/// compositeur de la piste.
fn roles_locaux(state: &AppState, pistes: &[Track]) -> HashMap<i64, BTreeSet<String>> {
    let mut out: HashMap<i64, BTreeSet<String>> = HashMap::new();
    for p in pistes {
        let Some(id) = p.id else { continue };
        let roles = out.entry(id).or_default();
        for r in state
            .backend
            .query_many(
                "SELECT role FROM track_credits WHERE track_id = ? \
                 AND (source IS NULL OR source <> 'qobuz')",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap_or_default()
        {
            if let Some(role) = r.first().and_then(|v| v.as_string()) {
                roles.insert(role);
            }
        }
        if p.composer.as_deref().is_some_and(|c| !c.trim().is_empty()) {
            roles.insert("composer".into());
        }
    }
    let ids: Vec<i64> = pistes.iter().filter_map(|p| p.id).collect();
    let meta = TrackMetadataRepo::with_backend(state.backend.clone());
    for cle in ["performer", "producer"] {
        for (id, v) in meta.get_key_for_tracks(cle, &ids).unwrap_or_default() {
            if !v.trim().is_empty() {
                out.entry(id).or_default().insert(cle.to_string());
            }
        }
    }
    out
}

fn pochette_locale(album: &Album) -> PochetteLocale {
    match album
        .cover_path
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        None => PochetteLocale::Absente,
        Some(c) => {
            match tune_core::library::artwork::dimensions_de_l_image(std::path::Path::new(c)) {
                Some((l, h)) => PochetteLocale::Cote(l.min(h)),
                None => PochetteLocale::Inconnue,
            }
        }
    }
}

fn etat_local(
    state: &AppState,
    album: &Album,
    pistes: &[Track],
) -> (EtatLocalAlbum, Vec<EtatLocalPiste>) {
    let ids: Vec<i64> = pistes.iter().filter_map(|p| p.id).collect();
    let copyright = TrackMetadataRepo::with_backend(state.backend.clone())
        .get_key_for_tracks("copyright", &ids)
        .unwrap_or_default()
        .values()
        .any(|v| !v.trim().is_empty());
    let id = album.id.unwrap_or(0);
    let livret = !super::livrets::livrets_des_dossiers(&super::livrets::dossiers_des_pistes(
        super::livrets::chemins_des_pistes(state, id),
    ))
    .is_empty();
    let genres = album.genre.as_deref().is_some_and(|g| !g.trim().is_empty())
        || album
            .genres
            .as_deref()
            .is_some_and(|g| !matches!(g.trim(), "" | "[]" | "null"));
    let local = EtatLocalAlbum {
        label: album.label.clone(),
        date_de_sortie: album.release_date.clone(),
        annee: album.year,
        upc: album.barcode.clone(),
        copyright,
        genres,
        livret,
        pochette: pochette_locale(album),
    };
    let mut roles = roles_locaux(state, pistes);
    let pistes = pistes
        .iter()
        .filter_map(|p| {
            let id = p.id?;
            Some(EtatLocalPiste {
                id,
                isrc: p.isrc.clone(),
                roles: roles.remove(&id).unwrap_or_default(),
            })
        })
        .collect();
    (local, pistes)
}

fn pistes_a_rapprocher(pistes: &[Track]) -> Vec<PisteLocale> {
    pistes
        .iter()
        .filter_map(|p| {
            Some(PisteLocale {
                id: p.id?,
                disque: i64::from(p.disc_number.max(1)),
                numero: i64::from(p.track_number),
                duree_ms: p.duration_ms,
                isrc: p.isrc.clone(),
            })
        })
        .collect()
}

/// Le code-barres de la release MusicBrainz gardée en base pour le MBID de
/// l'album (#4805) : la voie « MBID » du rapprochement, sans réseau.
fn code_barres_du_mbid(state: &AppState, album: &Album) -> Option<String> {
    let mbid = album.musicbrainz_release_id.as_deref()?.trim();
    let release = tune_core::metadata::musicbrainz_release_cache::lire(
        &state.backend,
        mbid,
        "",
        chrono::Utc::now(),
    )?;
    release["barcode"]
        .as_str()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
}

// ── Rapprocher, juger, ranger ────────────────────────────────────────────

/// Ce qu'une recherche a donné : l'album Qobuz, la méthode, et le doute
/// éventuel.
struct Trouve {
    id: String,
    methode: Methode,
    doute: Option<&'static str>,
}

async fn rechercher(
    state: &AppState,
    svc: &Service,
    album: &Album,
    pistes: &[Track],
) -> Result<Option<Trouve>, String> {
    let svc = svc.read().await;
    let codes: [(Option<String>, Methode); 2] = [
        (
            album.barcode.clone().filter(|b| !b.trim().is_empty()),
            Methode::Upc,
        ),
        (code_barres_du_mbid(state, album), Methode::Mbid),
    ];
    for (upc, methode) in codes {
        if let Some(upc) = upc {
            sous_le_limiteur().await;
            if let Some(a) = svc
                .album_par_codes(Some(&upc), &[])
                .await
                .map_err(|e| e.to_string())?
            {
                return Ok(Some(Trouve {
                    id: a.id,
                    methode,
                    doute: None,
                }));
            }
        }
    }
    let isrcs: Vec<String> = pistes
        .iter()
        .filter_map(|p| p.isrc.clone())
        .take(3)
        .collect();
    if !isrcs.is_empty() {
        sous_le_limiteur().await;
        if let Some(a) = svc
            .album_par_codes(None, &isrcs)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(Some(Trouve {
                id: a.id,
                methode: Methode::Isrc,
                doute: None,
            }));
        }
    }
    let local = AlbumLocal {
        titre: album.title.clone(),
        artiste: album.artist_name.clone().unwrap_or_default(),
        nombre_pistes: pistes.len() as u32,
        est_compilation: album.is_compilation,
    };
    if local.artiste.trim().is_empty() && !local.est_compilation {
        return Ok(None);
    }
    sous_le_limiteur().await;
    let candidats = svc
        .albums_par_titre(&local.artiste, &local.titre)
        .await
        .map_err(|e| e.to_string())?;
    Ok(match ap::juger_par_titre(&local, &candidats) {
        Jugement::Sur(id) => Some(Trouve {
            id,
            methode: Methode::Titre,
            doute: None,
        }),
        Jugement::Douteux { id, motif } => Some(Trouve {
            id,
            methode: Methode::Titre,
            doute: Some(motif),
        }),
        Jugement::Aucun => None,
    })
}

async fn fiche(svc: &Service, id: &str) -> Result<Option<FicheAlbumService>, String> {
    sous_le_limiteur().await;
    svc.read()
        .await
        .fiche_album_service(id)
        .await
        .map_err(|e| e.to_string())
}

fn rapprochement(
    album_id: i64,
    t: Option<&Trouve>,
    statut: Statut,
    motif: Option<&str>,
    locales: usize,
    rapprochees: usize,
) -> Rapprochement {
    Rapprochement {
        album_id,
        source: ap::SOURCE.to_string(),
        service_album_id: t.map(|t| t.id.clone()),
        methode: t.map(|t| t.methode.as_str().to_string()),
        statut,
        motif: motif.map(str::to_string),
        pistes_locales: locales as i64,
        pistes_rapprochees: rapprochees as i64,
        verifie_le: stock::maintenant(),
    }
}

/// Apparie les pistes, juge, et range : les apports pour un rapprochement
/// SÛR, le rapprochement seul sinon.
fn appliquer(
    state: &AppState,
    album: &Album,
    pistes: &[Track],
    t: &Trouve,
    f: &FicheAlbumService,
) -> Result<Rapprochement, String> {
    let album_id = album.id.unwrap_or(0);
    let paires = ap::rapprocher_pistes(&pistes_a_rapprocher(pistes), &f.pistes);
    let statut = match t.doute {
        Some(_) => Statut::AConfirmer,
        None => ap::statut_apres_pistes(t.methode, pistes.len(), paires.len()),
    };
    let motif = t
        .doute
        .or((statut == Statut::AConfirmer).then_some("pistes_differentes"));
    let r = rapprochement(album_id, Some(t), statut, motif, pistes.len(), paires.len());
    // LA garde du rapprochement douteux : rien n'est appliqué hors d'un
    // rapprochement sûr.
    if r.statut != Statut::Applique {
        stock::ranger_rapprochement(&state.backend, &r)?;
        return Ok(r);
    }
    let (local, etat_pistes) = etat_local(state, album, pistes);
    let apports = ap::calculer(&local, &etat_pistes, f, &paires);
    let ids: Vec<i64> = pistes.iter().filter_map(|p| p.id).collect();
    stock::ranger_apports(&state.backend, &r, &apports, &ids)?;
    tracing::info!(
        album_id,
        methode = t.methode.as_str(),
        champs = apports.champs.len(),
        pistes_creditees = apports.credits.len(),
        "apports_qobuz_ranges"
    );
    Ok(r)
}

fn album_et_pistes(state: &AppState, album_id: i64) -> Result<Option<(Album, Vec<Track>)>, String> {
    let Some(album) = AlbumRepo::with_backend(state.backend.clone())
        .get(album_id)
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let pistes = TrackRepo::with_backend(state.backend.clone())
        .list_by_album(album_id)
        .map_err(|e| e.to_string())?;
    Ok(Some((album, pistes)))
}

/// Calcule les apports d'un album, s'il n'a jamais été examiné (ou
/// `rafraichir`). Un retrait n'est jamais relevé ici. Sans compte Qobuz
/// connecté : ce qui est rangé, sans appel.
pub(crate) async fn calculer(
    state: &AppState,
    album_id: i64,
    rafraichir: bool,
) -> Result<Option<Rapprochement>, String> {
    let deja = stock::lire_rapprochement(&state.backend, album_id, ap::SOURCE)?;
    if deja
        .as_ref()
        .is_some_and(|r| r.statut == Statut::Retire || !rafraichir)
    {
        return Ok(deja);
    }
    let Some(svc) = service(state).await else {
        return Ok(deja);
    };
    let Some((album, pistes)) = album_et_pistes(state, album_id)? else {
        return Ok(None);
    };
    let Some(trouve) = rechercher(state, &svc, &album, &pistes).await? else {
        let r = rapprochement(album_id, None, Statut::Aucun, None, pistes.len(), 0);
        stock::ranger_rapprochement(&state.backend, &r)?;
        return Ok(Some(r));
    };
    let Some(f) = fiche(&svc, &trouve.id).await? else {
        let r = rapprochement(
            album_id,
            Some(&trouve),
            Statut::Aucun,
            Some("fiche_introuvable"),
            pistes.len(),
            0,
        );
        stock::ranger_rapprochement(&state.backend, &r)?;
        return Ok(Some(r));
    };
    appliquer(state, &album, &pistes, &trouve, &f).map(Some)
}

/// Lance le calcul en arrière-plan si l'album n'a jamais été examiné et que
/// Qobuz est connecté. Ne bloque jamais la fiche.
pub(crate) async fn lancer_si_jamais_examine(state: &AppState, album_id: i64) {
    if !matches!(
        stock::lire_rapprochement(&state.backend, album_id, ap::SOURCE),
        Ok(None)
    ) {
        return;
    }
    if service(state).await.is_none() {
        return;
    }
    if !EN_VOL
        .lock()
        .map(|mut s| s.insert(album_id))
        .unwrap_or(false)
    {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = tokio::time::timeout(DELAI, calculer(&state, album_id, false))
            .await
            .unwrap_or_else(|_| Err("delai_depasse".into()))
        {
            tracing::info!(album_id, error = %e, "apports_qobuz_echec");
        }
        if let Ok(mut s) = EN_VOL.lock() {
            s.remove(&album_id);
        }
    });
}

// ── Exposer ──────────────────────────────────────────────────────────────

/// La vue du rapprochement et de ses apports : `champs` (album) et `pistes`
/// (par id de piste), chaque valeur avec sa `source`.
pub(crate) fn vue(state: &AppState, r: &Rapprochement) -> Value {
    let mut champs = Map::new();
    let mut pistes: Map<String, Value> = Map::new();
    for c in stock::champs(&state.backend, r.album_id, &r.source).unwrap_or_default() {
        let v = json!({"valeur": c.valeur, "source": r.source});
        if c.track_id == 0 {
            champs.insert(c.champ, v);
        } else {
            pistes
                .entry(c.track_id.to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .map(|o| o.insert(c.champ, v));
        }
    }
    json!({
        "source": r.source,
        "statut": r.statut.as_str(),
        "methode": r.methode,
        "service_album_id": r.service_album_id,
        "service_url": r.service_album_id.as_ref().map(|id| format!("https://open.qobuz.com/album/{id}")),
        "motif": r.motif,
        "pistes_locales": r.pistes_locales,
        "pistes_rapprochees": r.pistes_rapprochees,
        "verifie_le": r.verifie_le,
        "champs": champs,
        "pistes": pistes,
    })
}

/// La clé `apports_qobuz` de la fiche album, depuis la base seule.
pub(crate) fn annoter_album(state: &AppState, album_id: i64, j: &mut Value) {
    if let Ok(Some(r)) = stock::lire_rapprochement(&state.backend, album_id, ap::SOURCE)
        && let Some(o) = j.as_object_mut()
    {
        o.insert("apports_qobuz".into(), vue(state, &r));
    }
}

/// La clé `apports_qobuz` de chaque piste qui en a (ISRC repris).
pub(crate) fn annoter_pistes(state: &AppState, album_id: i64, items: &mut [Value]) {
    let Ok(champs) = stock::champs(&state.backend, album_id, ap::SOURCE) else {
        return;
    };
    let mut par_piste: HashMap<i64, Map<String, Value>> = HashMap::new();
    for c in champs.into_iter().filter(|c| c.track_id != 0) {
        par_piste
            .entry(c.track_id)
            .or_default()
            .insert(c.champ, json!({"valeur": c.valeur, "source": ap::SOURCE}));
    }
    if par_piste.is_empty() {
        return;
    }
    for item in items.iter_mut() {
        let Some(id) = item.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if let (Some(m), Some(o)) = (par_piste.remove(&id), item.as_object_mut()) {
            o.insert("apports_qobuz".into(), Value::Object(m));
        }
    }
}

// ── Routes ───────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub(super) struct Rafraichir {
    #[serde(default)]
    rafraichir: bool,
}

/// Le corps d'un refus : `{"error": <code stable>}` (`rien_a_confirmer`,
/// `qobuz_non_connecte`, `album_qobuz_introuvable`) ou le message d'une panne.
/// Le client traduit le code (#7077 : jamais de phrase dans l'API).
fn erreur(code: StatusCode, message: impl Into<String>) -> Response {
    (code, Json(json!({"error": message.into()}))).into_response()
}

fn reponse(state: &AppState, r: Option<Rapprochement>, connecte: bool) -> Response {
    match r {
        Some(r) => {
            let mut v = vue(state, &r);
            v["service_connecte"] = json!(connecte);
            Json(v).into_response()
        }
        None => {
            Json(json!({"source": ap::SOURCE, "statut": Value::Null, "service_connecte": connecte}))
                .into_response()
        }
    }
}

pub(super) async fn lire(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<Rafraichir>,
) -> Response {
    match AlbumRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let connecte = service(&state).await.is_some();
    match tokio::time::timeout(DELAI, calculer(&state, id, q.rafraichir)).await {
        Ok(Ok(r)) => reponse(&state, r, connecte),
        // Une panne n'est pas une absence : rien n'est rangé, ce qui l'était
        // est rendu.
        Ok(Err(e)) => {
            tracing::info!(album_id = id, error = %e, "apports_qobuz_echec");
            reponse(
                &state,
                stock::lire_rapprochement(&state.backend, id, ap::SOURCE)
                    .ok()
                    .flatten(),
                connecte,
            )
        }
        Err(_) => reponse(
            &state,
            stock::lire_rapprochement(&state.backend, id, ap::SOURCE)
                .ok()
                .flatten(),
            connecte,
        ),
    }
}

pub(super) async fn retirer(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let pistes = match album_et_pistes(&state, id) {
        Ok(Some((_, p))) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let ids: Vec<i64> = pistes.iter().filter_map(|p| p.id).collect();
    match stock::retirer(&state.backend, id, ap::SOURCE, &ids) {
        Ok(r) => Json(json!({"statut": Statut::Retire.as_str(), "champs_retires": r.champs, "credits_retires": r.credits})).into_response(),
        Err(e) => erreur(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize, Default)]
pub(super) struct Confirmation {
    #[serde(default)]
    service_album_id: Option<String>,
}

pub(super) async fn confirmer(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    corps: Option<Json<Confirmation>>,
) -> Response {
    let (album, pistes) = match album_et_pistes(&state, id) {
        Ok(Some(ap)) => ap,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let demande = corps
        .and_then(|Json(c)| c.service_album_id)
        .filter(|s| !s.trim().is_empty());
    let range = stock::lire_rapprochement(&state.backend, id, ap::SOURCE)
        .ok()
        .flatten();
    let Some(cible) = demande.or_else(|| range.and_then(|r| r.service_album_id)) else {
        return erreur(StatusCode::BAD_REQUEST, "rien_a_confirmer");
    };
    let Some(svc) = service(&state).await else {
        return erreur(StatusCode::CONFLICT, "qobuz_non_connecte");
    };
    let f = match fiche(&svc, &cible).await {
        Ok(Some(f)) => f,
        Ok(None) => return erreur(StatusCode::NOT_FOUND, "album_qobuz_introuvable"),
        Err(e) => return erreur(StatusCode::BAD_GATEWAY, e),
    };
    let t = Trouve {
        id: cible,
        methode: Methode::Confirme,
        doute: None,
    };
    match appliquer(&state, &album, &pistes, &t, &f) {
        Ok(r) => reponse(&state, Some(r), true),
        Err(e) => erreur(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Lance la passe de fond : les albums jamais examinés, un à un, sous le
/// limiteur, en cédant à la pause « Enrichissement » et à la lecture. Une
/// seule passe à la fois. `false` : déjà en cours, ou Qobuz non connecté.
pub(crate) async fn lancer_passe(state: &AppState) -> bool {
    if service(state).await.is_none() {
        return false;
    }
    let Some(garde) = state.background_tasks.try_begin(
        TACHE_APPORTS_QOBUZ,
        "Compléments Qobuz des albums…",
        "enrichment",
    ) else {
        return false;
    };
    let state = state.clone();
    tokio::spawn(async move {
        let _garde = garde;
        let ids = stock::albums_a_examiner(&state.backend, ap::SOURCE, ALBUMS_PAR_PASSE)
            .unwrap_or_default();
        let total = ids.len() as u64;
        for (n, id) in ids.into_iter().enumerate() {
            attendre_son_tour(Tache::Enrichissement).await;
            if service(&state).await.is_none() {
                break;
            }
            if let Err(e) = calculer(&state, id, false).await {
                tracing::info!(album_id = id, error = %e, "apports_qobuz_passe_echec");
            }
            state.background_tasks.update_progress(
                TACHE_APPORTS_QOBUZ,
                n as u64 + 1,
                total,
                "albums",
            );
            tokio::time::sleep(PAUSE_ENTRE_ALBUMS).await;
        }
    });
    true
}

pub(super) async fn passe(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"lancee": lancer_passe(&state).await}))
}

/// Au démarrage : une passe quinze minutes après, puis toutes les six heures,
/// tant que Qobuz est connecté.
pub(crate) fn armer_la_passe(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15 * 60)).await;
        loop {
            lancer_passe(&state).await;
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}
