//! #8066 — remplacer un titre indisponible dans une playlist DU SERVICE
//! (Qobuz, TIDAL), par l'API du service, à sa place.
//!
//! * `GET  /{service}/playlists/{playlist_id}/tracks/{index}/replacements`
//! * `POST /{service}/playlists/{playlist_id}/tracks/{index}/replace` —
//!   `{"track_id": "<titre vu à ce rang>", "source_id": "<remplaçant>"}` ;
//!   sans `source_id`, le seul candidat sûr ;
//! * `POST /{service}/playlists/{playlist_id}/repair` — « Réparer la
//!   playlist » et son rapport.
//!
//! `index` est le rang dans `GET /{service}/playlists/{playlist_id}/tracks`.
//! Toute écriture exige que la playlist soit modifiable par l'utilisateur
//! (`StreamingService::playlist_modifiable` = `Some(true)`) ; sinon 403
//! `playlist_non_modifiable`, sans un appel d'écriture.

// Les refus sont des réponses HTTP complètes (statut, code, message) : les
// emballer pour la taille de `Err` n'apporterait rien ici.
#![allow(clippy::result_large_err)]

use super::*;
use tune_core::streaming::remplacement_indisponibles::{
    self as remplacement, Candidat, Decision, SEUIL_SUR, TitreCherche,
};
use tune_core::streaming::traits::StreamTrack;

type Svc = Arc<RwLock<Box<dyn StreamingService>>>;

fn erreur(statut: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        statut,
        Json(json!({ "error": message.into(), "code": code })),
    )
        .into_response()
}

async fn service(state: &StreamingHttpState, nom: &str) -> Result<Svc, Response> {
    let svc = get_svc_actif(state, nom)
        .await
        .map_err(|(s, m)| erreur(s, "service_indisponible", m))?;
    if !svc.read().await.utilisable().await {
        return Err(erreur(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_non_connecte",
            format!("{nom} n'est pas connecté"),
        ));
    }
    Ok(svc)
}

async fn pistes(svc: &Svc, playlist_id: &str) -> Result<Vec<StreamTrack>, Response> {
    svc.read()
        .await
        .get_playlist_tracks(playlist_id)
        .await
        .map_err(|e| erreur(StatusCode::BAD_GATEWAY, "lecture_echouee", e.to_string()))
}

async fn candidats(svc: &Svc, cible: &TitreCherche) -> Result<Vec<Candidat>, Response> {
    let s = svc.read().await;
    remplacement::chercher_candidats(&**s, cible)
        .await
        .map_err(|e| erreur(StatusCode::BAD_GATEWAY, "recherche_echouee", e.to_string()))
}

/// Refus clair si l'utilisateur ne peut pas modifier la playlist.
async fn exiger_modifiable(svc: &Svc, nom: &str, playlist_id: &str) -> Result<(), Response> {
    let s = svc.read().await;
    if s.playlist_modifiable(playlist_id).await.ok().flatten() == Some(true) {
        return Ok(());
    }
    Err(erreur(
        StatusCode::FORBIDDEN,
        "playlist_non_modifiable",
        format!(
            "cette playlist {nom} n'est pas modifiable par votre compte (playlist suivie, \
             éditoriale ou d'un autre compte) : aucun titre n'a été remplacé"
        ),
    ))
}

fn reponse_candidats(
    nom: &str,
    index: usize,
    ancien: &StreamTrack,
    modifiable: Option<bool>,
    c: &[Candidat],
) -> Value {
    json!({
        "index": index,
        "source": nom,
        "source_id": ancien.id,
        "title": ancien.title,
        "unavailable": remplacement::est_indisponible(ancien),
        "editable": modifiable,
        "threshold": SEUIL_SUR,
        "decision": remplacement::decider(c),
        "candidates": c.iter().map(|x| x.en_json(nom)).collect::<Vec<_>>(),
    })
}

pub(super) async fn candidats_du_titre(
    State(state): State<StreamingHttpState>,
    Path((nom, playlist_id, index)): Path<(String, String, usize)>,
) -> Response {
    let svc = match service(&state, &nom).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let liste = match pistes(&svc, &playlist_id).await {
        Ok(l) => l,
        Err(r) => return r,
    };
    let Some(ancien) = liste.get(index) else {
        return erreur(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("aucun titre au rang {index}"),
        );
    };
    let c = match candidats(&svc, &TitreCherche::from(ancien)).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let modifiable = svc
        .read()
        .await
        .playlist_modifiable(&playlist_id)
        .await
        .ok()
        .flatten();
    Json(reponse_candidats(&nom, index, ancien, modifiable, &c)).into_response()
}

#[derive(Deserialize)]
pub(super) struct CorpsRemplacement {
    /// Le titre que le client voit à ce rang : garde contre une playlist
    /// modifiée entre-temps.
    track_id: String,
    #[serde(default)]
    source_id: Option<String>,
}

/// Écrit chez le service ; 501 si le service ne sait pas le faire.
async fn ecrire(
    svc: &Svc,
    nom: &str,
    playlist_id: &str,
    index: usize,
    ancien: &str,
    nouveau: &str,
) -> Result<(), Response> {
    let r = svc
        .read()
        .await
        .replace_playlist_track(playlist_id, index, ancien, nouveau)
        .await;
    purge_contenu_utilisateur(nom);
    r.map_err(|e| match e {
        tune_core::TuneError::Unsupported(m) => erreur(
            StatusCode::NOT_IMPLEMENTED,
            "remplacement_non_pris_en_charge",
            m,
        ),
        autre => erreur(
            StatusCode::BAD_GATEWAY,
            "ecriture_echouee",
            autre.to_string(),
        ),
    })
}

pub(super) async fn remplacer_le_titre(
    State(state): State<StreamingHttpState>,
    Path((nom, playlist_id, index)): Path<(String, String, usize)>,
    Json(corps): Json<CorpsRemplacement>,
) -> Response {
    let svc = match service(&state, &nom).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(r) = exiger_modifiable(&svc, &nom, &playlist_id).await {
        return r;
    }
    let liste = match pistes(&svc, &playlist_id).await {
        Ok(l) => l,
        Err(r) => return r,
    };
    let Some(ancien) = liste.get(index) else {
        return erreur(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("aucun titre au rang {index}"),
        );
    };
    if ancien.id != corps.track_id {
        return erreur(
            StatusCode::CONFLICT,
            "ligne_modifiee",
            "la playlist a changé : rechargez-la avant de remplacer",
        );
    }
    let nouveau: StreamTrack = match corps.source_id.as_deref().map(str::trim) {
        Some(choisi) if !choisi.is_empty() => {
            if choisi == ancien.id {
                return erreur(
                    StatusCode::BAD_REQUEST,
                    "meme_titre",
                    "le remplaçant est le titre lui-même",
                );
            }
            match svc.read().await.get_track(choisi).await {
                Ok(p) if remplacement::est_indisponible(&p) => {
                    return erreur(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "remplacant_indisponible",
                        "le titre choisi est lui aussi indisponible",
                    );
                }
                Ok(p) => p,
                Err(_) => {
                    return erreur(
                        StatusCode::NOT_FOUND,
                        "remplacant_introuvable",
                        format!("{nom} ne connaît pas le titre {choisi}"),
                    );
                }
            }
        }
        _ => {
            let c = match candidats(&svc, &TitreCherche::from(ancien)).await {
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
                    let mut v = reponse_candidats(&nom, index, ancien, Some(true), &c);
                    v["error"] = json!(message);
                    v["code"] = json!(code);
                    return (statut, Json(v)).into_response();
                }
            }
        }
    };
    match ecrire(&svc, &nom, &playlist_id, index, &ancien.id, &nouveau.id).await {
        Ok(()) => {
            tracing::info!(service = %nom, %playlist_id, index, ancien = %ancien.id, nouveau = %nouveau.id, "playlist_service_titre_remplace_8066");
            let mut track = serde_json::to_value(&nouveau).unwrap_or(Value::Null);
            track["source"] = json!(nom);
            Json(json!({
                "replaced": true,
                "index": index,
                "old_source_id": ancien.id,
                "track": track,
            }))
            .into_response()
        }
        Err(r) => r,
    }
}

pub(super) async fn reparer_la_playlist(
    State(state): State<StreamingHttpState>,
    Path((nom, playlist_id)): Path<(String, String)>,
) -> Response {
    let svc = match service(&state, &nom).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(r) = exiger_modifiable(&svc, &nom, &playlist_id).await {
        return r;
    }
    let liste = match pistes(&svc, &playlist_id).await {
        Ok(l) => l,
        Err(r) => return r,
    };
    let (mut remplaces, mut a_choisir, mut sans_candidat, mut erreurs) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut examines = 0usize;
    for (index, ancien) in liste.iter().enumerate() {
        if !remplacement::est_indisponible(ancien) {
            continue;
        }
        examines += 1;
        let resume = |extra: Value| {
            let mut v = json!({
                "index": index,
                "source": nom,
                "source_id": ancien.id,
                "title": ancien.title,
                "artist_name": ancien.artist,
            });
            if let (Some(o), Value::Object(x)) = (v.as_object_mut(), extra) {
                o.extend(x);
            }
            v
        };
        let c = match candidats(&svc, &TitreCherche::from(ancien)).await {
            Ok(c) => c,
            Err(_) => {
                erreurs.push(resume(json!({"error": "recherche_echouee"})));
                continue;
            }
        };
        match remplacement::candidat_direct(&c) {
            // Le remplacement garde les rangs : les indices suivants restent
            // justes.
            Some(sur) => {
                match ecrire(&svc, &nom, &playlist_id, index, &ancien.id, &sur.piste.id).await {
                    Ok(()) => remplaces.push(resume(json!({
                        "new_source_id": sur.piste.id,
                        "score": sur.score,
                        "replacement": sur.en_json(&nom),
                    }))),
                    Err(_) => erreurs.push(resume(json!({"error": "ecriture_echouee"}))),
                }
            }
            None if c.is_empty() => sans_candidat.push(resume(json!({}))),
            None => a_choisir.push(resume(json!({
                "candidates": c.iter().map(|x| x.en_json(&nom)).collect::<Vec<_>>(),
            }))),
        }
    }
    Json(json!({
        "threshold": SEUIL_SUR,
        "examined": examines,
        "replaced": remplaces,
        "to_choose": a_choisir,
        "no_candidate": sans_candidat,
        "errors": erreurs,
    }))
    .into_response()
}

#[cfg(test)]
#[path = "remplacement_indisponibles_8066_tests.rs"]
mod tests;
