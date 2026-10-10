//! Les versions d'une piste, REGROUPÉES par enregistrement, avec la version
//! jouée par défaut (#2264).
//!
//! # Les routes
//!
//! | méthode | chemin | rôle |
//! |---|---|---|
//! | `GET` | `/library/tracks/{id}/versions/groups` | les exemplaires de la piste et de ses versions, groupés, avec la version par défaut de chaque groupe |
//! | `GET` | `/library/versions/rule` | la règle de choix réglée : celle du profil nommé par `X-Profile-Id`, sinon le défaut global |
//! | `PUT` | `/library/versions/rule` | la régler (`{"rule": "quality"}`) ou revenir au défaut (`{"rule": null}`) : sur le profil nommé par `X-Profile-Id`, sinon le défaut global |
//!
//! # Ce que la route ajoute à `GET /library/tracks/{id}/versions`
//!
//! Les candidats sont les MÊMES : ceux de `rassembler_versions` (bibliothèque
//! et services), plus les pistes de la bibliothèque qui partagent l'ISRC ou le
//! MBID d'enregistrement de la piste — celles-là peuvent porter un autre
//! titre, et le rapprochement par le titre ne les verrait pas. La route
//! historique est inchangée ; celle-ci dit en plus lesquels sont le même
//! enregistrement ([`tune_core::library::groupes_versions::grouper`]) et lequel
//! jouer ([`tune_core::library::groupes_versions::choisir`]).
//!
//! # Ce que la route ne fait PAS
//!
//! Elle n'écrit rien, hors du réglage de la règle : aucun groupe n'est
//! persisté. La LECTURE applique la même règle, au même endroit pour tous les
//! lancements (`tune-core/src/orchestrator/version_de_lecture.rs`, décisions
//! du 07/10/2026).
//!
//! # Portée de la règle (décision 3)
//!
//! Par profil, rangée dans les réglages du profil, avec un repli sur le
//! défaut global (`tune_core::library::regle_de_version`). Une requête SANS
//! `X-Profile-Id` garde le contrat d'avant : elle lit et règle le défaut
//! global. `?scope=global` force le défaut global même avec l'en-tête.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::groupes_versions::{Exemplaire, Qualite, RegleDeChoix, choisir, grouper};
use tune_core::library::regle_de_version::{
    self as regle_de_version, Origine, regle_effective, regle_globale,
};
use tune_core::library::track_matcher::normaliser_isrc;
use tune_core::library::versions_en_base as base;

use crate::routes::active_profile::ActiveProfile;
use crate::routes::filtre_sources::FiltreSources;
use crate::state::AppState;

/// La clé du réglage : dans `settings` pour le défaut global, dans les
/// réglages du profil pour la règle d'un profil. Aucune migration.
pub(crate) const CLE_REGLE: &str = regle_de_version::CLE_REGLE;

/// Le défaut GLOBAL réglé, et d'où il vient : `setting` ou `default`.
///
/// Une valeur illisible en base (écrite à la main) vaut le défaut : la route
/// `PUT` refuse de l'écrire, et une lecture ne doit pas échouer pour autant.
pub(crate) fn regle_reglee(state: &AppState) -> (RegleDeChoix, &'static str) {
    match regle_globale(&state.backend) {
        Some(r) => (r, Origine::Global.nom()),
        None => (RegleDeChoix::DEFAUT, Origine::Defaut.nom()),
    }
}

/// Le profil que NOMME la requête par `X-Profile-Id`.
///
/// `Ok(None)` : pas d'en-tête, la portée est le défaut global (le contrat
/// d'avant). `Err` : l'en-tête nomme un profil inexistant ou que l'appelant
/// n'a pas le droit de régler — l'extracteur commun se serait alors rabattu
/// sur le profil actif, et la règle aurait été écrite sur un AUTRE profil que
/// celui demandé.
fn profil_nomme(headers: &HeaderMap, profil: &ActiveProfile) -> Result<Option<i64>, Response> {
    let Some(brut) = headers.get("X-Profile-Id") else {
        return Ok(None);
    };
    let demande = brut
        .to_str()
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    match demande {
        Some(id) if id > 0 && id == profil.id() => Ok(Some(id)),
        _ => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "X-Profile-Id: profil introuvable ou non autorisé" })),
        )
            .into_response()),
    }
}

/// Une piste de la bibliothèque, et sa fiche d'affichage.
fn exemplaire_de_ligne(cols: &base::Ligne) -> (Exemplaire, Value) {
    let fiche = json!({
        "album_id": cols.get(base::COL_ALBUM_ID).and_then(|v| v.as_i64()),
        "cover_path": cols.get(base::COL_COVER).and_then(|v| v.as_string()),
    });
    (base::exemplaire_de_ligne(cols), fiche)
}

/// La piste de départ. `None` : elle n'existe pas (404).
fn lire_reference(state: &AppState, id: i64) -> Option<(Exemplaire, Value)> {
    base::lire_piste(&state.backend, id).map(|(_, cols)| exemplaire_de_ligne(&cols))
}

/// Les pistes de la bibliothèque qui partagent l'ISRC ou le MBID
/// d'enregistrement de la référence, quel que soit leur titre. Requête
/// commune avec la règle de lecture, servie par les index de la migration 122
/// (PG 086) : voir `tune_core::library::versions_en_base`.
fn pistes_par_identifiant(state: &AppState, reference: &Exemplaire) -> Vec<(Exemplaire, Value)> {
    base::pistes_par_identifiant(
        &state.backend,
        reference,
        reference.track_id,
        base::PLAFOND_PAR_IDENTIFIANT,
    )
    .iter()
    .map(|(_, cols)| exemplaire_de_ligne(cols))
    .collect()
}

/// Un candidat LOCAL rendu par `versions_locales`.
fn exemplaire_local(v: &Value) -> (Exemplaire, Value) {
    let s = |k: &str| v[k].as_str().map(str::to_string);
    let e = Exemplaire {
        source: s("source").unwrap_or_else(|| "local".into()),
        track_id: v["track_id"].as_i64(),
        source_id: None,
        titre: s("title").unwrap_or_default(),
        artiste: s("artist_name").unwrap_or_default(),
        album: s("album_title").unwrap_or_default(),
        isrc: s("isrc"),
        mbid_enregistrement: s("musicbrainz_recording_id"),
        duree_ms: v["duration_ms"].as_i64(),
        qualite: Some(Qualite {
            format: s("format"),
            sample_rate: v["sample_rate"].as_i64(),
            bit_depth: v["bit_depth"].as_i64(),
        }),
        disponible: None,
    };
    (
        e,
        json!({ "album_id": v["album_id"], "cover_path": v["cover_path"], "kind": "version" }),
    )
}

/// Un candidat de SERVICE rendu par `versions_streaming`.
fn exemplaire_de_service(v: &Value) -> (Exemplaire, Value) {
    let s = |k: &str| v[k].as_str().map(str::to_string);
    let q = &v["quality"];
    let qualite = q.is_object().then(|| Qualite {
        format: q["codec"].as_str().map(|c| c.to_ascii_lowercase()),
        sample_rate: q["sample_rate"].as_i64(),
        bit_depth: q["bit_depth"].as_i64(),
    });
    let e = Exemplaire {
        source: s("service").unwrap_or_default(),
        track_id: None,
        source_id: s("source_id"),
        titre: s("title").unwrap_or_default(),
        artiste: s("artist_name").unwrap_or_default(),
        album: s("album_title").unwrap_or_default(),
        isrc: s("isrc"),
        mbid_enregistrement: None,
        duree_ms: v["duration_ms"].as_i64(),
        qualite,
        disponible: v["available"].as_bool(),
    };
    (
        e,
        json!({ "album_id": v["album_id"], "cover_path": v["cover_path"], "kind": v["kind"] }),
    )
}

fn json_membre(e: &Exemplaire, fiche: &Value) -> Value {
    json!({
        "source": e.source,
        "track_id": e.track_id,
        "source_id": e.source_id,
        "title": e.titre,
        "artist_name": e.artiste,
        "album_title": e.album,
        "album_id": fiche["album_id"],
        "cover_path": fiche["cover_path"],
        "kind": fiche["kind"],
        "duration_ms": e.duree_ms,
        "isrc": e.isrc,
        "musicbrainz_recording_id": e.mbid_enregistrement,
        "quality": e.qualite.as_ref().map(|q| json!({
            "format": q.format,
            "sample_rate": q.sample_rate,
            "bit_depth": q.bit_depth,
        })),
        "available": e.disponible,
    })
}

/// Rassemble, groupe et choisit. `None` : la piste n'existe pas.
pub(super) async fn rassembler_groupes(
    state: &AppState,
    id: i64,
    regle: &RegleDeChoix,
    limite: i64,
    avec_streaming: bool,
    filtre: &FiltreSources,
) -> Option<Value> {
    let (reference, mut fiche_ref) = lire_reference(state, id)?;
    fiche_ref["kind"] = json!("reference");
    let candidats =
        super::tracks::rassembler_versions(state, id, limite, avec_streaming, filtre).await?;

    let mut exemplaires: Vec<Exemplaire> = vec![reference.clone()];
    let mut fiches: Vec<Value> = vec![fiche_ref];
    let mut vus_local = std::collections::HashSet::from([id]);
    let mut vus_service = std::collections::HashSet::new();
    if let Some(sid) = &reference.source_id {
        vus_service.insert((reference.source.to_ascii_lowercase(), sid.clone()));
    }
    let mut ajouter = |(e, f): (Exemplaire, Value)| {
        let nouveau = match (e.track_id, &e.source_id) {
            (Some(t), _) => vus_local.insert(t),
            (None, Some(sid)) => vus_service.insert((e.source.to_ascii_lowercase(), sid.clone())),
            (None, None) => true,
        };
        if nouveau {
            exemplaires.push(e);
            fiches.push(f);
        }
    };

    if filtre.local_demande() {
        for (e, mut f) in pistes_par_identifiant(state, &reference) {
            f["kind"] = json!("version");
            ajouter((e, f));
        }
    }
    for v in candidats["versions"].as_array().into_iter().flatten() {
        ajouter(exemplaire_local(v));
    }
    for v in candidats["streaming"].as_array().into_iter().flatten() {
        ajouter(exemplaire_de_service(v));
    }

    let groupes: Vec<Value> = grouper(&exemplaires)
        .iter()
        .map(|g| {
            let indices: Vec<usize> = g.membres.iter().map(|m| m.indice).collect();
            let defaut = choisir(&exemplaires, &indices, regle);
            let membres: Vec<Value> = g
                .membres
                .iter()
                .map(|m| {
                    let mut v = json_membre(&exemplaires[m.indice], &fiches[m.indice]);
                    v["link"] = json!(m.lien.map(|l| l.nom()));
                    v["is_reference"] = json!(m.indice == 0);
                    v["is_default"] = json!(Some(m.indice) == defaut);
                    v
                })
                .collect();
            let premier = |f: fn(&Exemplaire) -> Option<String>| {
                indices.iter().find_map(|&i| f(&exemplaires[i]))
            };
            json!({
                "identity": g.identite().map(|l| l.nom()),
                "isrc": premier(|e| e.isrc.as_deref().map(normaliser_isrc).filter(|s| !s.is_empty())),
                "musicbrainz_recording_id": premier(|e| e.mbid_enregistrement.clone()),
                "contains_reference": indices.contains(&0),
                "default": defaut.and_then(|d| indices.iter().position(|&i| i == d)),
                "members": membres,
            })
        })
        .collect();

    Some(json!({
        "track_id": id,
        "title": reference.titre,
        "artist_name": reference.artiste,
        "rule": regle.texte(),
        "groups": groupes,
    }))
}

#[derive(Deserialize)]
pub(super) struct ParamsGroupes {
    /// `local`, `quality` ou `service:<nom>`. Absent : la règle réglée.
    rule: Option<String>,
    /// Même contrat que `GET /library/tracks/{id}/versions`.
    limit: Option<i64>,
    streaming: Option<bool>,
    sources: Option<String>,
}

/// `GET /library/tracks/{id}/versions/groups`. Contrat dans la PR et en tête
/// de ce module. Sans `rule`, c'est la règle du profil de la requête (celle
/// que la lecture appliquera), sinon le défaut global.
pub(super) async fn track_version_groups(
    State(state): State<AppState>,
    profil: ActiveProfile,
    Path(id): Path<i64>,
    Query(p): Query<ParamsGroupes>,
) -> impl IntoResponse {
    let (regle, origine) = match p.rule.as_deref() {
        Some(texte) => match RegleDeChoix::depuis(texte) {
            Some(r) => (r, "query"),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "rule: attendu none, local, quality ou service:<nom>" })),
                )
                    .into_response();
            }
        },
        None => {
            let (r, o) = regle_effective(&state.backend, Some(profil.id()));
            (r, o.nom())
        }
    };
    let limite = p.limit.unwrap_or(50).clamp(1, 200);
    let avec_streaming = p.streaming.unwrap_or(true);
    let filtre = FiltreSources::depuis(p.sources.as_deref());
    match rassembler_groupes(&state, id, &regle, limite, avec_streaming, &filtre).await {
        Some(mut v) => {
            v["rule_origin"] = json!(origine);
            Json(v).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize, Default)]
pub(super) struct ParamsRegle {
    /// `global` : le défaut global, même avec `X-Profile-Id`.
    scope: Option<String>,
}

impl ParamsRegle {
    fn global(&self) -> bool {
        self.scope.as_deref() == Some("global")
    }
}

/// La réponse commune des deux routes de réglage.
fn corps_regle(state: &AppState, profil: Option<i64>) -> Value {
    match profil {
        Some(id) => {
            let (regle, origine) = regle_effective(&state.backend, Some(id));
            json!({ "rule": regle.texte(), "origin": origine.nom(), "scope": "profile", "profile_id": id })
        }
        None => {
            let (regle, origine) = regle_reglee(state);
            json!({ "rule": regle.texte(), "origin": origine, "scope": "global", "profile_id": null })
        }
    }
}

/// `GET /library/versions/rule` — avec `X-Profile-Id` : la règle qui
/// s'applique à ce profil (`origin` = `profile`, `setting` ou `default`) ;
/// sans : le défaut global, comme avant.
pub(super) async fn get_version_rule(
    State(state): State<AppState>,
    profil: ActiveProfile,
    headers: HeaderMap,
    Query(q): Query<ParamsRegle>,
) -> Response {
    let nomme = if q.global() {
        None
    } else {
        match profil_nomme(&headers, &profil) {
            Ok(n) => n,
            Err(r) => return r,
        }
    };
    Json(corps_regle(&state, nomme)).into_response()
}

#[derive(Deserialize)]
pub(super) struct CorpsRegle {
    rule: Option<String>,
}

/// `PUT /library/versions/rule` — `{"rule": "quality"}` règle,
/// `{"rule": null}` revient au défaut (pour un profil : au défaut global).
/// Une règle illisible est refusée (400) et rien n'est écrit. La portée est
/// celle de `GET`.
pub(super) async fn put_version_rule(
    State(state): State<AppState>,
    profil: ActiveProfile,
    headers: HeaderMap,
    Query(q): Query<ParamsRegle>,
    Json(corps): Json<CorpsRegle>,
) -> Response {
    let nomme = if q.global() {
        None
    } else {
        match profil_nomme(&headers, &profil) {
            Ok(n) => n,
            Err(r) => return r,
        }
    };
    let regle = match corps.rule.as_deref() {
        None => None,
        Some(texte) => match RegleDeChoix::depuis(texte) {
            Some(r) => Some(r),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "rule: attendu none, local, quality ou service:<nom>" })),
                )
                    .into_response();
            }
        },
    };
    let ecrit = match (nomme, &regle) {
        (Some(id), r) => regle_de_version::poser_regle_du_profil(&state.backend, id, r.as_ref()),
        (None, None) => SettingsRepo::with_backend(state.backend.clone()).delete(CLE_REGLE),
        (None, Some(r)) => {
            SettingsRepo::with_backend(state.backend.clone()).set(CLE_REGLE, &r.texte())
        }
    };
    if let Err(e) = ecrit {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response();
    }
    Json(corps_regle(&state, nomme)).into_response()
}

#[cfg(test)]
#[path = "versions_groupes_tests.rs"]
mod tests;
