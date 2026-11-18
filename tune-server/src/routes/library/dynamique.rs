//! #9040 — la DYNAMIQUE d'une piste et d'un album (go de Bertrand du 18/11,
//! forum fil 2902) : DR, plage de sonie (LRA), crêtes et sonie intégrée.
//!
//! Informatif, jamais un verdict : aucun seuil, aucun « douteux ». Lecture
//! seule, rien n'est mesuré ici :
//!
//! * DR (TT) : `dr_track` / `dr_album`, du tag du fichier ou de la passe ;
//! * sonie intégrée, sample peak et true peak : la passe ReplayGain
//!   (`rg_track_*`, `rg_album_*`), seulement quand c'est une mesure de Tune
//!   (`rg_*_source = analysis`) — un gain lu dans les balises n'a pas de
//!   référence sûre (ReplayGain 1 ou 2) ;
//! * LRA : `plages_de_sonie` ([`tune_core::audio::plage_de_sonie`]). Celle
//!   d'un album se calcule sur les fenêtres de TOUTES ses pistes, et n'est
//!   publiée que si toutes ses pistes à fichier sont mesurées : une LRA faite
//!   de la moitié d'un album n'est pas celle de l'album.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Value, json};
use tune_core::audio::plage_de_sonie::{self, Histogramme};
use tune_core::audio::replaygain as rg;
use tune_core::db::backend::ToSqlValue;

use crate::error::AppError;
use crate::state::AppState;

fn dixieme(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Gain ReplayGain (« -3.20 dB ») → sonie intégrée en LUFS.
fn lufs_du_gain(g: &str) -> Option<f64> {
    g.trim()
        .trim_end_matches("dB")
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|g| g.is_finite())
        .map(|g| dixieme(rg::REFERENCE_LUFS - g))
}

/// Crête linéaire → dB (dBFS ou dBTP).
fn db_de_la_crete(p: &str) -> Option<f64> {
    p.trim()
        .parse::<f64>()
        .ok()
        .filter(|p| p.is_finite() && *p > 0.0)
        .map(|p| dixieme(20.0 * p.log10()))
}

fn sans_vides(v: Value) -> Option<Value> {
    let Value::Object(mut o) = v else {
        return Some(v);
    };
    o.retain(|_, v| !v.is_null());
    (!o.is_empty()).then_some(Value::Object(o))
}

/// Secondes Unix (texte) → date UTC.
fn date_unix(v: &str) -> Option<String> {
    let s = v
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s > 0.0)?;
    chrono::DateTime::from_timestamp(s.trunc() as i64, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Fil 2793 — la sonie EBU R128 d'une piste mesurée par la passe ReplayGain
/// de Tune : sonie intégrée, crête vraie, et (#9040) sample peak. `None` hors
/// d'une mesure de Tune.
pub(super) fn sonie_de_la_piste(m: &HashMap<String, String>) -> Option<Value> {
    if m.get(rg::TRACK_SOURCE_KEY).map(String::as_str) != Some(rg::SOURCE_ANALYSIS) {
        return None;
    }
    let lufs = m.get("rg_track_gain").and_then(|g| lufs_du_gain(g));
    let dbtp = m.get("rg_track_true_peak").and_then(|p| db_de_la_crete(p));
    if lufs.is_none() && dbtp.is_none() {
        return None;
    }
    sans_vides(json!({
        "integree_lufs": lufs,
        "crete_vraie_dbtp": dbtp,
        "crete_echantillon_dbfs": m.get("rg_track_peak").and_then(|p| db_de_la_crete(p)),
        "algo": m.get(rg::RG_ALGO_KEY),
        "mesure_le": m.get("rg_analyzed").and_then(|v| date_unix(v)),
    }))
}

/// La section `dynamique` de la fiche technique d'une piste : sonie
/// intégrée, crêtes, LRA. Le DR n'y est PAS repris : la section
/// `dynamic_range` de la fiche le porte déjà, avec sa source (#8160). `None`
/// quand rien n'est mesuré.
pub(super) fn de_la_piste(
    state: &AppState,
    id: i64,
    m: &HashMap<String, String>,
) -> Result<Option<Value>, AppError> {
    let lra = state
        .backend
        .query_one(
            "SELECT lra_lu, algo, mesure_le FROM plages_de_sonie WHERE track_id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;
    let sonie = sonie_de_la_piste(m);
    let s = |k: &str| sonie.as_ref().and_then(|v| v.get(k)).cloned();
    Ok(sans_vides(json!({
        "integree_lufs": s("integree_lufs"),
        "crete_echantillon_dbfs": s("crete_echantillon_dbfs"),
        "crete_vraie_dbtp": s("crete_vraie_dbtp"),
        "sonie_algo": s("algo"),
        "sonie_mesuree_le": s("mesure_le"),
        "lra_lu": lra.as_ref().and_then(|r| r.first().and_then(|v| v.as_f64())),
        "lra_algo": lra.as_ref().and_then(|r| r.get(1).and_then(|v| v.as_string())),
        "lra_mesuree_le": lra.as_ref().and_then(|r| r.get(2).and_then(|v| v.as_string())),
    })))
}

/// GET /library/albums/{id}/dynamique
pub(super) async fn de_l_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    tokio::task::spawn_blocking(move || album(&state, id))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
        .map(Json)
}

fn album(state: &AppState, id: i64) -> Result<Value, AppError> {
    let b = &state.backend;
    let p = [&id as &dyn ToSqlValue];
    let pistes = b
        .query_one(
            "SELECT COUNT(*) FROM tracks WHERE album_id = ? \
             AND file_path IS NOT NULL AND file_path != ''",
            &p,
        )
        .map_err(AppError::internal)?
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);

    // Le DR d'album : la règle unique de la grille, du tri et des facettes.
    let dr = b
        .query_one(
            &format!(
                "SELECT {}, {} FROM track_metadata tm JOIN tracks tdr ON tdr.id = tm.track_id \
                 WHERE tdr.album_id = ? AND {}",
                tune_core::db::facet_filter::DR_ALBUM_VALUE,
                tune_core::db::facet_filter::DR_ALBUM_FROM_TAG,
                tune_core::db::facet_filter::dr_tag_where(b.engine()),
            ),
            &p,
        )
        .map_err(AppError::internal)?;
    let dr_valeur = dr
        .as_ref()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .filter(|v| *v > 0);
    let dr_du_tag = dr
        .as_ref()
        .and_then(|r| r.get(1).and_then(|v| v.as_i64()))
        .map(|v| v == 1);

    // Sonie et crêtes d'album : les clés d'album de la passe ReplayGain,
    // recopiées sur chaque piste, seulement là où l'album est de Tune.
    let lignes = b
        .query_many(
            &format!(
                "SELECT m.key, MAX(m.value) FROM track_metadata m JOIN tracks t ON t.id = m.track_id \
                 WHERE t.album_id = ? \
                   AND m.key IN ('rg_album_gain', 'rg_album_peak', 'rg_album_true_peak') \
                   AND EXISTS (SELECT 1 FROM track_metadata s WHERE s.track_id = m.track_id \
                         AND s.key = '{}' AND s.value = '{}') \
                 GROUP BY m.key",
                rg::ALBUM_SOURCE_KEY,
                rg::SOURCE_ANALYSIS
            ),
            &p,
        )
        .map_err(AppError::internal)?;
    let cle = |k: &str| {
        lignes
            .iter()
            .find(|r| r.first().and_then(|v| v.as_string()).as_deref() == Some(k))
            .and_then(|r| r.get(1).and_then(|v| v.as_string()))
    };

    // LRA d'album : les histogrammes de toutes ses pistes, fusionnés.
    let histos = b
        .query_many(
            "SELECT p.histogramme FROM plages_de_sonie p JOIN tracks t ON t.id = p.track_id \
             WHERE t.album_id = ? AND t.file_path IS NOT NULL AND t.file_path != ''",
            &p,
        )
        .map_err(AppError::internal)?;
    let mut album = Histogramme::default();
    let mut lisibles = 0i64;
    for r in &histos {
        if let Some(h) = r
            .first()
            .and_then(|v| v.as_string())
            .and_then(|s| Histogramme::decoder(&s))
        {
            album.fusionner(&h);
            lisibles += 1;
        }
    }
    let lra = (pistes > 0 && lisibles == pistes)
        .then(|| album.lra())
        .flatten();

    Ok(json!({
        "album_id": id,
        "pistes": pistes,
        "dr": dr_valeur,
        "dr_du_tag_d_album": dr_valeur.and(dr_du_tag),
        "integree_lufs": cle("rg_album_gain").and_then(|g| lufs_du_gain(&g)),
        "crete_echantillon_dbfs": cle("rg_album_peak").and_then(|p| db_de_la_crete(&p)),
        "crete_vraie_dbtp": cle("rg_album_true_peak").and_then(|p| db_de_la_crete(&p)),
        "lra_lu": lra,
        "lra_pistes_mesurees": lisibles,
        "lra_algo": (lisibles > 0).then_some(plage_de_sonie::ALGO),
    }))
}

#[cfg(test)]
#[path = "dynamique_tests_9040.rs"]
mod tests;
