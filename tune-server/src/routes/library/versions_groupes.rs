//! Les versions d'une piste, REGROUPÉES par enregistrement, avec la version
//! jouée par défaut (#2264).
//!
//! # Les routes
//!
//! | méthode | chemin | rôle |
//! |---|---|---|
//! | `GET` | `/library/tracks/{id}/versions/groups` | les exemplaires de la piste et de ses versions, groupés, avec la version par défaut de chaque groupe |
//! | `GET` | `/library/versions/rule` | la règle de choix réglée |
//! | `PUT` | `/library/versions/rule` | la régler (`{"rule": "quality"}`) ou revenir au défaut (`{"rule": null}`) |
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
//! persisté, et la lecture d'une playlist, d'un favori ou de la file reste
//! attachée à la piste précise qui y a été mise. Ce sont les décisions 2 et 4
//! de l'audit du 29/08, que la PR pose en questions plutôt que de les
//! supposer.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::groupes_versions::{Exemplaire, Qualite, RegleDeChoix, choisir, grouper};
use tune_core::library::track_matcher::normaliser_isrc;
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::routes::filtre_sources::FiltreSources;
use crate::routes::versions::marqueur;
use crate::state::AppState;

/// La clé du réglage dans la table `settings`. Aucune migration : la table
/// clé/valeur existe sur les deux moteurs.
pub(crate) const CLE_REGLE: &str = "versions_default_rule";

/// Plafond des pistes rapprochées par identifiant (ISRC ou MBID).
const PLAFOND_PAR_IDENTIFIANT: i64 = 200;

/// La règle réglée, et d'où elle vient : `setting` ou `default`.
///
/// Une valeur illisible en base (écrite à la main) vaut le défaut : la route
/// `PUT` refuse de l'écrire, et une lecture ne doit pas échouer pour autant.
pub(crate) fn regle_reglee(state: &AppState) -> (RegleDeChoix, &'static str) {
    match SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_REGLE)
        .ok()
        .flatten()
        .and_then(|t| RegleDeChoix::depuis(&t))
    {
        Some(r) => (r, "setting"),
        None => (RegleDeChoix::DEFAUT, "default"),
    }
}

/// Les colonnes lues pour une piste de la bibliothèque, dans l'ordre de
/// [`exemplaire_de_ligne`]. Alias imposés : `t`, `al`, `ar`, `ar2`.
const COLONNES_PISTE: &str = "t.id, t.title, COALESCE(ar2.name, ar.name, ''), \
     COALESCE(al.title, ''), t.isrc, t.musicbrainz_recording_id, t.duration_ms, \
     COALESCE(t.source, 'local'), t.source_id, t.format, t.sample_rate, t.bit_depth, \
     t.album_id, al.cover_path";

const JOINTURES_PISTE: &str = "FROM tracks t \
     LEFT JOIN albums al ON t.album_id = al.id \
     LEFT JOIN artists ar ON al.artist_id = ar.id \
     LEFT JOIN artists ar2 ON t.artist_id = ar2.id";

type Ligne = Vec<tune_core::db::backend::SqlValue>;

fn exemplaire_de_ligne(cols: &Ligne) -> (Exemplaire, Value) {
    let s = |i: usize| cols.get(i).and_then(|v| v.as_string());
    let n = |i: usize| cols.get(i).and_then(|v| v.as_i64());
    let qualite = Qualite {
        format: s(9),
        sample_rate: n(10),
        bit_depth: n(11),
    };
    let e = Exemplaire {
        source: s(7).unwrap_or_else(|| "local".into()),
        track_id: n(0),
        source_id: s(8),
        titre: s(1).unwrap_or_default(),
        artiste: s(2).unwrap_or_default(),
        album: s(3).unwrap_or_default(),
        isrc: s(4),
        mbid_enregistrement: s(5),
        duree_ms: n(6),
        qualite: Some(qualite),
        disponible: None,
    };
    let fiche = json!({ "album_id": n(12), "cover_path": s(13) });
    (e, fiche)
}

/// La piste de départ. `None` : elle n'existe pas (404).
fn lire_reference(state: &AppState, id: i64) -> Option<(Exemplaire, Value)> {
    let e = state.backend.engine();
    let sql = format!(
        "SELECT {COLONNES_PISTE} {JOINTURES_PISTE} WHERE t.id = {}",
        marqueur(e, 1)
    );
    state
        .backend
        .query_one(&sql, &[&id as &dyn ToSqlValue])
        .ok()
        .flatten()
        .map(|cols| exemplaire_de_ligne(&cols))
}

/// Les pistes de la bibliothèque qui partagent l'ISRC ou le MBID
/// d'enregistrement de la référence, quel que soit leur titre.
///
/// L'ISRC est comparé sous la forme que rend `normaliser_isrc` pour les deux
/// séparateurs qu'on rencontre (`-` et l'espace) ; la comparaison exacte est
/// refaite en Rust par `relation`, ce SQL ne fait que trouver les candidats.
/// Aucun index ne porte ces colonnes : la requête parcourt `tracks`. Mesure
/// dans la PR, sur SQLite et PostgreSQL.
fn pistes_par_identifiant(state: &AppState, reference: &Exemplaire) -> Vec<(Exemplaire, Value)> {
    let isrc = reference
        .isrc
        .as_deref()
        .map(normaliser_isrc)
        .unwrap_or_default();
    let mbid = reference
        .mbid_enregistrement
        .as_deref()
        .map(|m| m.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if isrc.is_empty() && mbid.is_empty() {
        return Vec::new();
    }
    let e = state.backend.engine();
    let sql = format!(
        "SELECT {COLONNES_PISTE} {JOINTURES_PISTE} \
         CROSS JOIN (SELECT CAST({} AS TEXT) AS isrc, CAST({} AS TEXT) AS mbid) ref_id \
         WHERE t.id <> {} AND ( \
           (ref_id.isrc <> '' AND UPPER(REPLACE(REPLACE(t.isrc, '-', ''), ' ', '')) = ref_id.isrc) \
           OR (ref_id.mbid <> '' AND LOWER(TRIM(t.musicbrainz_recording_id)) = ref_id.mbid)) \
         ORDER BY t.id LIMIT {PLAFOND_PAR_IDENTIFIANT}",
        marqueur(e, 1),
        marqueur(e, 2),
        marqueur(e, 3),
    );
    let id = reference.track_id.unwrap_or(-1);
    let params: [&dyn ToSqlValue; 3] = [&isrc, &mbid, &id];
    state
        .backend
        .query_many(&sql, &params)
        .ou_defaut_journalise()
        .iter()
        .map(exemplaire_de_ligne)
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
/// de ce module.
pub(super) async fn track_version_groups(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(p): Query<ParamsGroupes>,
) -> impl IntoResponse {
    let (regle, origine) = match p.rule.as_deref() {
        Some(texte) => match RegleDeChoix::depuis(texte) {
            Some(r) => (r, "query"),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "rule: attendu local, quality ou service:<nom>" })),
                )
                    .into_response();
            }
        },
        None => regle_reglee(&state),
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

/// `GET /library/versions/rule`.
pub(super) async fn get_version_rule(State(state): State<AppState>) -> Json<Value> {
    let (regle, origine) = regle_reglee(&state);
    Json(json!({ "rule": regle.texte(), "origin": origine }))
}

#[derive(Deserialize)]
pub(super) struct CorpsRegle {
    rule: Option<String>,
}

/// `PUT /library/versions/rule` — `{"rule": "quality"}` règle,
/// `{"rule": null}` revient au défaut. Une règle illisible est refusée (400)
/// et rien n'est écrit.
pub(super) async fn put_version_rule(
    State(state): State<AppState>,
    Json(corps): Json<CorpsRegle>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let ecrit = match corps.rule.as_deref() {
        None => settings.delete(CLE_REGLE),
        Some(texte) => match RegleDeChoix::depuis(texte) {
            Some(r) => settings.set(CLE_REGLE, &r.texte()),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "rule: attendu local, quality ou service:<nom>" })),
                )
                    .into_response();
            }
        },
    };
    if let Err(e) = ecrit {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response();
    }
    let (regle, origine) = regle_reglee(&state);
    Json(json!({ "rule": regle.texte(), "origin": origine })).into_response()
}

#[cfg(test)]
#[path = "versions_groupes_tests.rs"]
mod tests;
