//! #8066 — remplacer un titre de service indisponible dans une playlist
//! LOCALE (Tune), à sa place.
//!
//! * `GET  /playlists/{id}/tracks/{entry_id}/replacements` — les candidats ;
//! * `POST /playlists/{id}/tracks/{entry_id}/replace` — remplacer, par le
//!   candidat choisi (`{"source_id": "…"}`) ou, sans corps, par le seul
//!   candidat sûr ;
//! * `POST /playlists/{id}/repair` — « Réparer la playlist » : chaque titre
//!   indisponible est remplacé s'il a un seul candidat sûr ; le rapport dit
//!   les remplacés, ceux à choisir et ceux sans candidat.
//!
//! La ligne est réécrite EN PLACE (`PlaylistRepo::replace_service_entry`) :
//! même `entry_id`, même rang. La playlist est à Tune : aucun service n'est
//! écrit. L'indisponibilité est celle du client web
//! ([`remplacement_indisponibles::est_indisponible`] sur la fiche que le
//! service rend du titre).

// Les refus sont des réponses HTTP complètes (statut, code, message) : les
// emballer pour la taille de `Err` n'apporterait rien ici.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use tune_core::db::playlist_repo::{PlaylistRepo, ServiceEntry};
use tune_core::streaming::StreamingService;
use tune_core::streaming::remplacement_indisponibles::{
    self as remplacement, Candidat, Decision, SEUIL_SUR, TitreCherche,
};
use tune_core::streaming::traits::StreamTrack;

use crate::routes::active_profile::ActiveProfile;
use crate::routes::corps_json_optionnel::CorpsJsonOptionnel;
use crate::state::AppState;

type Service = Arc<RwLock<Box<dyn StreamingService>>>;

const DELAI_FICHE: Duration = Duration::from_secs(8);
const DELAI_RECHERCHE: Duration = Duration::from_secs(15);

pub(super) fn erreur(statut: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        statut,
        Json(json!({ "error": message.into(), "code": code })),
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub(super) struct CorpsRemplacement {
    #[serde(default)]
    source_id: Option<String>,
}

async fn service_utilisable(state: &AppState, nom: &str) -> Result<Service, Response> {
    let svc = { state.services.lock().await.get(nom) };
    let Some(svc) = svc else {
        return Err(erreur(
            StatusCode::BAD_REQUEST,
            "service_inconnu",
            format!("service inconnu : {nom}"),
        ));
    };
    if !svc.read().await.utilisable().await {
        return Err(erreur(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_non_connecte",
            format!("{nom} n'est pas connecté : impossible d'y chercher un remplaçant"),
        ));
    }
    Ok(svc)
}

/// La fiche que le service rend du titre, ou `None` (erreur, délai).
async fn fiche(svc: &Service, source_id: &str) -> Option<StreamTrack> {
    let svc = svc.read().await;
    match tokio::time::timeout(DELAI_FICHE, svc.get_track(source_id)).await {
        Ok(Ok(p)) if p.id == source_id => Some(p),
        _ => None,
    }
}

fn cible(ligne: &ServiceEntry, fiche: Option<&StreamTrack>) -> TitreCherche {
    match fiche {
        Some(f) => {
            let mut c = TitreCherche::from(f);
            // La ligne garde ce que la playlist a enregistré : en repli.
            if c.album.is_none() {
                c.album = ligne.album.clone();
            }
            if c.duration_ms.is_none() {
                c.duration_ms = ligne.duration_ms.filter(|d| *d > 0).map(|d| d as u64);
            }
            c
        }
        None => TitreCherche {
            source_id: ligne.source_id.clone(),
            title: ligne.title.clone(),
            artist: ligne.artist.clone().unwrap_or_default(),
            album: ligne.album.clone(),
            duration_ms: ligne.duration_ms.filter(|d| *d > 0).map(|d| d as u64),
            isrc: None,
        },
    }
}

async fn candidats(svc: &Service, cible: &TitreCherche) -> Result<Vec<Candidat>, Response> {
    let svc = svc.read().await;
    match tokio::time::timeout(
        DELAI_RECHERCHE,
        remplacement::chercher_candidats(&**svc, cible),
    )
    .await
    {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(erreur(
            StatusCode::BAD_GATEWAY,
            "recherche_echouee",
            format!("recherche chez {} : {e}", svc.name()),
        )),
        Err(_) => Err(erreur(
            StatusCode::GATEWAY_TIMEOUT,
            "recherche_echouee",
            format!("recherche chez {} : délai dépassé", svc.name()),
        )),
    }
}

fn candidat_json(service: &str, c: &Candidat) -> Value {
    c.en_json(service)
}

fn entree_depuis(service: &str, p: &StreamTrack) -> ServiceEntry {
    ServiceEntry {
        source: service.to_string(),
        source_id: p.id.clone(),
        title: p.title.clone(),
        artist: Some(p.artist.clone()).filter(|a| !a.is_empty()),
        album: p.album.clone(),
        album_source_id: p.album_id.clone(),
        duration_ms: Some(p.duration_ms as i64).filter(|d| *d > 0),
        cover_url: p.cover_path.clone(),
    }
}

/// La ligne `entry_id` de la playlist, si c'est un titre de service :
/// `(rang, titre)`.
fn ligne_de_service(
    state: &AppState,
    playlist_id: i64,
    entry_id: i64,
) -> Result<(usize, ServiceEntry), Response> {
    let entrees = PlaylistRepo::with_backend(state.backend.clone())
        .get_entries(playlist_id)
        .map_err(|e| erreur(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e))?;
    let Some((rang, e)) = entrees.iter().enumerate().find(|(_, e)| e.id == entry_id) else {
        return Err(erreur(
            StatusCode::NOT_FOUND,
            "not_found",
            "ligne absente de la playlist",
        ));
    };
    match e.service() {
        Some(s) => Ok((rang, s.clone())),
        None => Err(erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            "pas_un_titre_de_service",
            "seul un titre de service se remplace par sa nouvelle référence",
        )),
    }
}

fn reponse_candidats(
    entry_id: i64,
    rang: usize,
    ligne: &ServiceEntry,
    indisponible: Option<bool>,
    c: &[Candidat],
) -> Value {
    json!({
        "entry_id": entry_id,
        "position": rang,
        "source": ligne.source,
        "source_id": ligne.source_id,
        "title": ligne.title,
        "unavailable": indisponible,
        "threshold": SEUIL_SUR,
        "decision": remplacement::decider(c),
        "candidates": c.iter().map(|x| candidat_json(&ligne.source, x)).collect::<Vec<_>>(),
    })
}

pub(super) async fn candidats_de_la_ligne(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path((id, entry_id)): Path<(i64, i64)>,
) -> Response {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    if let Err(r) = super::owned_or_404_response(&repo, id, profile.id()) {
        return r;
    }
    let (rang, ligne) = match ligne_de_service(&state, id, entry_id) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let svc = match service_utilisable(&state, &ligne.source).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let f = fiche(&svc, &ligne.source_id).await;
    let indisponible = f.as_ref().map(remplacement::est_indisponible);
    match candidats(&svc, &cible(&ligne, f.as_ref())).await {
        Ok(c) => Json(reponse_candidats(entry_id, rang, &ligne, indisponible, &c)).into_response(),
        Err(r) => r,
    }
}

/// Écrit le remplacement et rend la ligne nouvelle.
fn ecrire(
    state: &AppState,
    playlist_id: i64,
    entry_id: i64,
    ancien: &ServiceEntry,
    nouveau: &StreamTrack,
) -> Result<ServiceEntry, Response> {
    let entree = entree_depuis(&ancien.source, nouveau);
    match PlaylistRepo::with_backend(state.backend.clone()).replace_service_entry(
        playlist_id,
        entry_id,
        &ancien.source_id,
        &entree,
    ) {
        Ok(true) => Ok(entree),
        Ok(false) => Err(erreur(
            StatusCode::CONFLICT,
            "ligne_modifiee",
            "la ligne a changé pendant le remplacement : rien n'a été écrit",
        )),
        Err(e) => Err(erreur(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            e,
        )),
    }
}

pub(super) async fn remplacer_la_ligne(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path((id, entry_id)): Path<(i64, i64)>,
    CorpsJsonOptionnel(corps): CorpsJsonOptionnel<CorpsRemplacement>,
) -> Response {
    let corps = corps.unwrap_or_default();
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    if let Err(r) = super::owned_or_404_response(&repo, id, profile.id()) {
        return r;
    }
    let (rang, ligne) = match ligne_de_service(&state, id, entry_id) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let svc = match service_utilisable(&state, &ligne.source).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let nouveau: StreamTrack = match corps.source_id.as_deref().map(str::trim) {
        Some(choisi) if !choisi.is_empty() => {
            if choisi == ligne.source_id {
                return erreur(
                    StatusCode::BAD_REQUEST,
                    "meme_titre",
                    "le remplaçant est le titre lui-même",
                );
            }
            match fiche(&svc, choisi).await {
                Some(p) if !remplacement::est_indisponible(&p) => p,
                Some(_) => {
                    return erreur(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "remplacant_indisponible",
                        "le titre choisi est lui aussi indisponible",
                    );
                }
                None => {
                    return erreur(
                        StatusCode::NOT_FOUND,
                        "remplacant_introuvable",
                        format!("{} ne connaît pas le titre {choisi}", ligne.source),
                    );
                }
            }
        }
        _ => {
            let f = fiche(&svc, &ligne.source_id).await;
            let c = match candidats(&svc, &cible(&ligne, f.as_ref())).await {
                Ok(c) => c,
                Err(r) => return r,
            };
            match remplacement::candidat_direct(&c) {
                Some(sur) => sur.piste.clone(),
                None => {
                    let (statut, code, message) = match remplacement::decider(&c) {
                        Decision::Aucun => (
                            StatusCode::NOT_FOUND,
                            "aucun_candidat",
                            "aucun titre de remplacement trouvé",
                        ),
                        _ => (
                            StatusCode::CONFLICT,
                            "choix_requis",
                            "plusieurs candidats ou aucun sûr : choisir le remplaçant",
                        ),
                    };
                    let mut v = reponse_candidats(entry_id, rang, &ligne, None, &c);
                    v["error"] = json!(message);
                    v["code"] = json!(code);
                    return (statut, Json(v)).into_response();
                }
            }
        }
    };
    match ecrire(&state, id, entry_id, &ligne, &nouveau) {
        Ok(entree) => {
            let mut track = entree.to_json();
            track["entry_id"] = json!(entry_id);
            tracing::info!(playlist_id = id, entry_id, ancien = %ligne.source_id, nouveau = %nouveau.id, "playlist_titre_remplace_8066");
            Json(json!({
                "replaced": true,
                "entry_id": entry_id,
                "position": rang,
                "old_source_id": ligne.source_id,
                "track": track,
            }))
            .into_response()
        }
        Err(r) => r,
    }
}

/// Le rapport de « Réparer la playlist ».
#[derive(Default)]
pub(super) struct Rapport {
    pub remplaces: Vec<Value>,
    pub a_choisir: Vec<Value>,
    pub sans_candidat: Vec<Value>,
    pub erreurs: Vec<Value>,
    pub examines: usize,
}

impl Rapport {
    pub fn json(self) -> Value {
        json!({
            "threshold": SEUIL_SUR,
            "examined": self.examines,
            "replaced": self.remplaces,
            "to_choose": self.a_choisir,
            "no_candidate": self.sans_candidat,
            "errors": self.erreurs,
        })
    }
}

pub(super) async fn reparer_la_playlist(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> Response {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    if let Err(r) = super::owned_or_404_response(&repo, id, profile.id()) {
        return r;
    }
    let entrees = match repo.get_entries(id) {
        Ok(e) => e,
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e),
    };
    let mut rapport = Rapport::default();
    for (rang, e) in entrees.iter().enumerate() {
        let Some(ligne) = e.service() else { continue };
        let resume = |extra: Value| {
            let mut v = json!({
                "entry_id": e.id,
                "position": rang,
                "source": ligne.source,
                "source_id": ligne.source_id,
                "title": ligne.title,
                "artist_name": ligne.artist,
            });
            if let (Some(o), Value::Object(x)) = (v.as_object_mut(), extra) {
                o.extend(x);
            }
            v
        };
        // Un service absent ou déconnecté ne dit rien de ses titres.
        let svc = { state.services.lock().await.get(&ligne.source) };
        let Some(svc) = svc else { continue };
        if !svc.read().await.utilisable().await {
            continue;
        }
        let Some(f) = fiche(&svc, &ligne.source_id).await else {
            continue;
        };
        if !remplacement::est_indisponible(&f) {
            continue;
        }
        rapport.examines += 1;
        let c = match candidats(&svc, &cible(ligne, Some(&f))).await {
            Ok(c) => c,
            Err(_) => {
                rapport
                    .erreurs
                    .push(resume(json!({"error": "recherche_echouee"})));
                continue;
            }
        };
        let tous = || {
            c.iter()
                .map(|x| candidat_json(&ligne.source, x))
                .collect::<Vec<_>>()
        };
        match remplacement::candidat_direct(&c) {
            Some(sur) => match ecrire(&state, id, e.id, ligne, &sur.piste) {
                Ok(_) => rapport.remplaces.push(resume(json!({
                    "new_source_id": sur.piste.id,
                    "score": sur.score,
                    "replacement": candidat_json(&ligne.source, sur),
                }))),
                Err(_) => rapport
                    .erreurs
                    .push(resume(json!({"error": "ligne_modifiee"}))),
            },
            None if c.is_empty() => rapport.sans_candidat.push(resume(json!({}))),
            None => rapport
                .a_choisir
                .push(resume(json!({ "candidates": tous() }))),
        }
    }
    Json(rapport.json()).into_response()
}
