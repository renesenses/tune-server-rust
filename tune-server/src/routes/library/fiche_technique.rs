//! #8160 — la FICHE TECHNIQUE d'une piste : `GET /library/tracks/{id}/fiche-technique`.
//!
//! Une seule route réunit, datés, ce que Tune sait déjà de la piste et qui
//! vivait jusqu'ici sur des routes séparées :
//!
//! * `format` : codec, fréquence, bits, canaux, débit, durée, taille, tels que
//!   le scan les a lus dans l'en-tête du fichier, et la date du fichier ;
//! * `spectrogramme` : l'image conservée (#7857, table `spectrogrammes`), son
//!   état (`a_jour`, `perime` si le fichier a changé depuis, `absent`), son
//!   URL, la date et la durée du calcul ;
//! * `dynamic_range` : le DR de la piste et sa source (`tag`, `analysis`,
//!   `sidecar`), l'algorithme et la date de la mesure quand Tune l'a calculé ;
//! * `faux_hires` : le verdict de la détection (#6285), avec la même règle de
//!   visibilité de « douteux » que la fiche album ;
//! * `integrite` : le dernier résultat de l'audit d'intégrité (#6286) ;
//! * `provenance` : la source de la piste, ses identifiants, les champs tenus
//!   à la main dans Tune, les crédits par source, la date du fichier lu ;
//! * `note` : la note personnelle du profil appelant (#7849) ;
//! * `forme_d_onde` : réservée à l'étude #8159, toujours `null` ici.
//!
//! Une section que rien ne renseigne vaut `null` : une piste de service, sans
//! fichier, n'a ni spectrogramme, ni DR mesuré, ni intégrité, et le client
//! n'affiche pas de section vide. Lecture seule, rien n'est calculé ici :
//! `POST /library/tracks/{id}/spectrogramme` recalcule l'image.

use std::path::Path as CheminFs;

use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::Engine;
use tune_core::db::models::Track;
use tune_core::db::notes_personnelles::{self as notes, TypeDObjet};
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// GET /library/tracks/{id}/fiche-technique
pub(super) async fn fiche(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let dossier = super::spectrogramme::dossier_du_cache();
    let profil = profile.id();
    tokio::task::spawn_blocking(move || construire(&state, profil, id, &dossier))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
        .map(Json)
}

/// Le fichier local d'une piste — le sien, ou l'image d'une feuille CUE — sur
/// sa graphie du disque (#1865), avec son état d'aujourd'hui. `None` : piste
/// de service, ou fichier introuvable.
pub(crate) fn fichier_de_la_piste(track: &Track) -> Option<(String, std::fs::Metadata)> {
    let chemin = track
        .file_path
        .as_deref()
        .filter(|c| !c.is_empty())
        .or(track.cue_media_path.as_deref().filter(|c| !c.is_empty()))?;
    let chemin = tune_core::library::local_path::resolve_existing_local_path(chemin)
        .unwrap_or_else(|| chemin.to_string());
    let meta = std::fs::metadata(&chemin).ok().filter(|m| m.is_file())?;
    Some((chemin, meta))
}

fn marque(engine: Engine, rang: usize) -> String {
    match engine {
        Engine::Sqlite => "?".to_string(),
        Engine::Postgres => format!("${rang}"),
    }
}

/// Secondes depuis l'époque Unix → date UTC `AAAA-MM-JJTHH:MM:SSZ`.
fn date_de(secondes: f64) -> Option<String> {
    if !secondes.is_finite() || secondes <= 0.0 {
        return None;
    }
    chrono::DateTime::from_timestamp(secondes.trunc() as i64, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Retire les clés à `null` ; rend `None` si rien ne reste.
fn sans_vides(v: Value) -> Option<Value> {
    let Value::Object(mut o) = v else {
        return Some(v);
    };
    o.retain(|_, v| !v.is_null());
    (!o.is_empty()).then_some(Value::Object(o))
}

pub(crate) fn construire(
    state: &AppState,
    profile_id: i64,
    id: i64,
    dossier: &CheminFs,
) -> Result<Value, AppError> {
    let track = TrackRepo::with_backend(state.backend.clone())
        .get(id)
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found("piste inconnue"))?;
    let fichier = fichier_de_la_piste(&track);
    let meta = fichier.as_ref().map(|(_, m)| m);
    let date_du_fichier = meta
        .and_then(tune_core::db::spectrogrammes::mtime_secondes)
        .or(track.file_mtime)
        .and_then(date_de);
    let metadonnees = TrackMetadataRepo::with_backend(state.backend.clone())
        .get_all(id)
        .unwrap_or_else(|e| {
            tracing::warn!(track_id = id, error = %e, "fiche_technique_metadonnees");
            Default::default()
        });

    let format = sans_vides(json!({
        "codec": track.format,
        "sample_rate": track.sample_rate.filter(|v| *v > 0),
        "bit_depth": track.bit_depth.filter(|v| *v > 0),
        "channels": (track.channels > 0).then_some(track.channels),
        "bitrate_kbps": track.bitrate_kbps.filter(|v| *v > 0),
        "duration_ms": (track.duration_ms > 0).then_some(track.duration_ms),
        "file_size": meta.map(|m| m.len() as i64).or(track.file_size),
        "fichier_modifie_le": date_du_fichier,
    }))
    // Une section qui n'aurait que la durée et les canaux ne dit rien du format.
    .filter(|f| f.get("codec").is_some() || f.get("sample_rate").is_some());

    let spectrogramme =
        meta.and_then(|m| super::spectrogramme::section(state.backend.as_ref(), id, m, dossier));

    let dynamic_range = metadonnees
        .get("dr_track")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .map(|valeur| {
            let source = metadonnees.get("dr_source").cloned();
            // La date d'une mesure de Tune : le témoin `rg_analyzed` (heure
            // unix) de la passe qui l'a calculée. Une valeur lue dans les
            // balises n'a pas d'autre date que celle du fichier.
            let mesure_le = (source.as_deref() == Some("analysis"))
                .then(|| metadonnees.get("rg_analyzed"))
                .flatten()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .and_then(date_de);
            sans_vides(json!({
                "valeur": valeur,
                "source": source,
                "algo": metadonnees.get(tune_core::audio::replaygain::DR_ALGO_KEY),
                "mesure_le": mesure_le,
            }))
            .unwrap_or_default()
        });

    let faux_hires = faux_hires(state, id)?;

    let integrite = if fichier.is_some() {
        super::audit_integrite::de_la_piste(
            &state.backend,
            id,
            track.cue_media_path.as_deref(),
            meta,
        )
        .map_err(AppError::internal)?
    } else {
        None
    };

    let provenance = provenance(state, &track, date_du_fichier.clone())?;

    let note = notes::lire(state.backend.as_ref(), profile_id, TypeDObjet::Track, id)
        .map_err(AppError::internal)?
        .map(|n| json!({ "texte_md": n.texte_md, "maj_le": n.maj_le }));

    Ok(json!({
        "track_id": id,
        "title": track.title,
        "artist_name": track.artist_name,
        "album_title": track.album_title,
        "source": track.source,
        "a_un_fichier": fichier.is_some(),
        "genere_le": tune_core::db::spectrogrammes::maintenant(),
        "format": format,
        "spectrogramme": spectrogramme,
        "dynamic_range": dynamic_range,
        "faux_hires": faux_hires,
        "integrite": integrite,
        "provenance": provenance,
        "note": note,
        // #8159 — la forme d'onde : place réservée, pas encore calculée ici.
        "forme_d_onde": Value::Null,
    }))
}

fn faux_hires(state: &AppState, id: i64) -> Result<Option<Value>, AppError> {
    let ligne = state
        .backend
        .query_one(
            &format!(
                "SELECT verdict, raison, coupure_hz, bits_effectifs, sample_rate, bits_declares, \
                 mesure_le FROM hires_verifications WHERE track_id = {}",
                marque(state.backend.engine(), 1)
            ),
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;
    let Some(r) = ligne else {
        return Ok(None);
    };
    let verdict = r.first().and_then(|v| v.as_string()).unwrap_or_default();
    let raison = r.get(1).and_then(|v| v.as_string());
    // #6285 — tant que « douteux » est masqué, une piste douteuse se dit non
    // analysée, sans raison : même règle que `GET /library/albums/{id}/hires`.
    let visible = tune_core::audio::faux_hires_publie::douteux_visible(&state.backend);
    let (verdict, raison) = if verdict == "douteux" && !visible {
        ("non_analyse".to_string(), None)
    } else {
        (verdict, raison)
    };
    Ok(Some(json!({
        "verdict": verdict,
        "raison": raison,
        "coupure_hz": r.get(2).and_then(|v| v.as_f64()),
        "bits_effectifs": r.get(3).and_then(|v| v.as_i64()),
        "sample_rate": r.get(4).and_then(|v| v.as_i64()),
        "bits_declares": r.get(5).and_then(|v| v.as_i64()),
        "mesure_le": r.get(6).and_then(|v| v.as_string()),
    })))
}

fn provenance(
    state: &AppState,
    track: &Track,
    date_du_fichier: Option<String>,
) -> Result<Option<Value>, AppError> {
    let id = track.id.unwrap_or_default();
    let tenus: Vec<&'static str> = tune_core::db::champs_tenus::de_la_piste(&state.backend, id)
        .map(|t| t.noms())
        .unwrap_or_default();
    let credits: Vec<Value> = state
        .backend
        .query_many(
            &format!(
                "SELECT COALESCE(source, ''), COUNT(*) FROM track_credits \
                 WHERE track_id = {} GROUP BY COALESCE(source, '') ORDER BY 1",
                marque(state.backend.engine(), 1)
            ),
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?
        .iter()
        .map(|r| {
            let source = r
                .first()
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            json!({
                "source": source,
                "nombre": r.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();
    let locale = track.source == "local";
    Ok(sans_vides(json!({
        "source": track.source,
        "source_id": track.source_id.as_deref().filter(|s| !s.is_empty()),
        "isrc": track.isrc.as_deref().filter(|s| !s.is_empty()),
        "musicbrainz_recording_id": track
            .musicbrainz_recording_id
            .as_deref()
            .filter(|s| !s.is_empty()),
        // Les balises du fichier font foi, sauf les champs corrigés à la
        // main dans Tune, qui sont tenus (`champs_tenus`).
        "balises_lues_du_fichier": locale.then_some(true),
        "fichier_modifie_le": date_du_fichier.filter(|_| locale),
        "champs_tenus": (!tenus.is_empty()).then_some(tenus),
        "credits": (!credits.is_empty()).then_some(credits),
    })))
}

#[cfg(test)]
#[path = "fiche_technique_tests_8160.rs"]
mod tests;
