use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::library::quality::score_qualite;
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::error::AppError;
use crate::state::AppState;

use super::Pagination;

#[derive(Deserialize)]
pub(super) struct ResolveDuplicate {
    keep_id: i64,
    delete_id: i64,
}

/// BIB-B3 : la porte unique des doublons parle avec des CRITÈRES NOMMÉS.
///
/// `GET /library/duplicates` empilait quatre listes de formes différentes
/// (`by_hash`, `by_metadata`, `by_fingerprint`, `by_content`) et le client
/// les aplatissait lui-même en paires, sans savoir ce que chacune PROUVAIT.
/// La même route rend désormais aussi `paires` — une seule forme pour les
/// quatre origines, chaque paire portant son critère et une recommandation
/// « garder » calculée par la règle de qualité partagée (`score_qualite`,
/// celle de « Disponible en meilleure qualité » et du repli d'album) — et
/// `criteres`, la liste de ce que chaque critère prouve et de ce qu'il
/// autorise. `?critere=` restreint la réponse à une famille ; un critère
/// inconnu est refusé (400), avec la liste. Les quatre listes d'origine
/// restent telles quelles : le client d'aujourd'hui les lit encore.
pub(super) struct Critere {
    pub code: &'static str,
    pub libelle: &'static str,
    pub preuve: &'static str,
    /// Supprimer l'une des deux copies ne perd rien d'audible.
    pub suppression_sure: bool,
}

pub(super) const CRITERES: [Critere; 4] = [
    Critere {
        code: "fichier_identique",
        libelle: "Fichiers identiques",
        preuve: "Les deux fichiers ont le même hash et sont identiques octet pour octet : deux copies du même fichier.",
        suppression_sure: true,
    },
    Critere {
        code: "contenu_identique",
        libelle: "Même enregistrement",
        preuve: "L'empreinte du son décodé est la même : le même enregistrement sous deux encodages ou deux masters proches. La qualité peut différer.",
        suppression_sure: false,
    },
    Critere {
        code: "empreinte_identique",
        libelle: "Même empreinte de fichier",
        preuve: "Le détecteur de doublons a relevé la même empreinte de fichier lors d'une analyse demandée.",
        suppression_sure: false,
    },
    Critere {
        code: "etiquettes_identiques",
        libelle: "Mêmes étiquettes",
        preuve: "Même titre, même artiste et même durée, mais des fichiers différents : à écouter avant de trancher.",
        suppression_sure: false,
    },
];

#[derive(Deserialize)]
pub(super) struct ParamsDoublons {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub critere: Option<String>,
}

/// `None` = tout ; `tous` = tout ; un code connu = cette famille seule ;
/// autre chose = refus, avec la liste des codes.
fn critere_demande(demande: Option<&str>) -> Result<Option<&'static str>, String> {
    match demande
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "tous")
    {
        None => Ok(None),
        Some(d) => CRITERES
            .iter()
            .map(|c| c.code)
            .find(|code| *code == d)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "critere_inconnu:{d} (attendu : tous, {})",
                    CRITERES
                        .iter()
                        .map(|c| c.code)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
    }
}

fn criteres_json() -> Value {
    Value::Array(
        CRITERES
            .iter()
            .map(|c| {
                json!({
                    "code": c.code,
                    "libelle": c.libelle,
                    "preuve": c.preuve,
                    "suppression_sure": c.suppression_sure,
                })
            })
            .collect(),
    )
}

/// La fiche d'une copie, dans la forme unique de `paires`. Les lignes
/// `by_hash` / `by_metadata` portent la seconde copie sous des clés `dup_*`.
fn fiche(source: &Value, dup: bool) -> Value {
    let cle = |nom: &str, nom_dup: &str| {
        if dup {
            source.get(nom_dup).cloned().unwrap_or(Value::Null)
        } else {
            source.get(nom).cloned().unwrap_or(Value::Null)
        }
    };
    let artiste = if dup {
        cle("artist_name", "dup_artist_name")
    } else {
        source
            .get("artist_name")
            .or_else(|| source.get("artist"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    json!({
        "id": cle("id", "dup_id"),
        "title": source.get("title").cloned().unwrap_or(Value::Null),
        "artist_name": artiste,
        "file_path": cle("file_path", "dup_path"),
        "duration_ms": source.get("duration_ms").cloned().unwrap_or(Value::Null),
        "format": cle("format", "dup_format"),
        "sample_rate": cle("sample_rate", "dup_sample_rate"),
        "bit_depth": cle("bit_depth", "dup_bit_depth"),
    })
}

/// « Garder » la meilleure copie par la règle partagée ; à égalité, la
/// première ; sans format connu des deux côtés, on ne recommande rien.
fn recommandation(a: &Value, b: &Value) -> Value {
    let score = |v: &Value| {
        v.get("format").and_then(Value::as_str).map(|format| {
            score_qualite(
                Some(format),
                v.get("sample_rate").and_then(Value::as_i64),
                v.get("bit_depth").and_then(Value::as_i64),
            )
        })
    };
    match (score(a), score(b)) {
        (Some(qa), Some(qb)) if qa > qb => {
            json!({"garder": a["id"], "raison": "meilleure_qualite"})
        }
        (Some(qa), Some(qb)) if qb > qa => {
            json!({"garder": b["id"], "raison": "meilleure_qualite"})
        }
        (Some(_), Some(_)) => json!({"garder": a["id"], "raison": "qualite_egale"}),
        _ => json!({"garder": Value::Null, "raison": "qualite_inconnue"}),
    }
}

fn paire(critere: &'static str, a: Value, b: Value) -> Value {
    let recommandation = recommandation(&a, &b);
    let suppression_sure = CRITERES
        .iter()
        .find(|c| c.code == critere)
        .is_some_and(|c| c.suppression_sure);
    json!({
        "critere": critere,
        "suppression_sure": suppression_sure,
        "a": a,
        "b": b,
        "recommandation": recommandation,
    })
}

/// Un groupe (`tracks`) devient des paires consécutives, comme le client
/// le faisait déjà pour `by_fingerprint`.
fn paires_depuis_groupe(critere: &'static str, groupe: &Value) -> Vec<Value> {
    let pistes = groupe
        .get("tracks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    pistes
        .windows(2)
        .map(|w| paire(critere, fiche(&w[0], false), fiche(&w[1], false)))
        .collect()
}

pub(super) async fn list_duplicates(
    State(state): State<AppState>,
    Query(p): Query<ParamsDoublons>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(100);
    let offset = p.offset.unwrap_or(0);
    let filtre = critere_demande(p.critere.as_deref()).map_err(AppError::bad_request)?;

    let make_ph = |i: usize| match state.backend.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    };

    // Duplicates by audio_hash
    let hash_sql = format!(
        "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.audio_hash, t1.duration_ms,
                t2.id, t2.file_path, ar2.name,
                t1.format, t1.sample_rate, t1.bit_depth, t2.format, t2.sample_rate, t2.bit_depth
         FROM tracks t1
         JOIN tracks t2 ON t1.audio_hash = t2.audio_hash AND t1.id < t2.id
         LEFT JOIN artists ar1 ON t1.artist_id = ar1.id
         LEFT JOIN artists ar2 ON t2.artist_id = ar2.id
         WHERE t1.audio_hash IS NOT NULL AND t1.audio_hash != ''
         LIMIT {lim} OFFSET {off}",
        lim = make_ph(1),
        off = make_ph(2),
    );
    let limit_val = limit as i64;
    let offset_val = offset as i64;
    let hash_params: &[&dyn ToSqlValue] = &[&limit_val, &offset_val];
    let hash_rows = state
        .backend
        .query_many(&hash_sql, hash_params)
        .ou_defaut_journalise();
    let hash_dups: Vec<Value> = hash_rows
        .iter()
        .filter_map(|row| {
            let file_path = row.get(3).and_then(|v| v.as_string())?;
            let duplicate_path = row.get(7).and_then(|v| v.as_string())?;
            if !tune_core::scanner::hasher::files_are_byte_identical(
                std::path::Path::new(&file_path),
                std::path::Path::new(&duplicate_path),
            )
            .unwrap_or(false)
            {
                return None;
            }
            Some(json!({
                "id": row.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": row.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": row.get(2).and_then(|v| v.as_string()),
                "file_path": file_path,
                "audio_hash": row.get(4).and_then(|v| v.as_string()),
                "duration_ms": row.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
                "dup_id": row.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                "dup_path": duplicate_path,
                "dup_artist_name": row.get(8).and_then(|v| v.as_string()),
                "match_type": "audio_hash",
                "format": row.get(9).and_then(|v| v.as_string()),
                "sample_rate": row.get(10).and_then(|v| v.as_i64()),
                "bit_depth": row.get(11).and_then(|v| v.as_i64()),
                "dup_format": row.get(12).and_then(|v| v.as_string()),
                "dup_sample_rate": row.get(13).and_then(|v| v.as_i64()),
                "dup_bit_depth": row.get(14).and_then(|v| v.as_i64()),
            }))
        })
        .collect();

    // Duplicates by (title + artist_name + duration_ms) where no hash match
    let meta_sql = format!(
        "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.duration_ms,
                t2.id, t2.file_path, ar2.name,
                t1.format, t1.sample_rate, t1.bit_depth, t2.format, t2.sample_rate, t2.bit_depth
         FROM tracks t1
         JOIN tracks t2 ON LOWER(t1.title) = LOWER(t2.title)
                       AND t1.duration_ms = t2.duration_ms
                       AND t1.id < t2.id
         LEFT JOIN artists ar1 ON t1.artist_id = ar1.id
         LEFT JOIN artists ar2 ON t2.artist_id = ar2.id
         WHERE LOWER(ar1.name) = LOWER(ar2.name)
           AND (t1.audio_hash IS NULL OR t2.audio_hash IS NULL OR t1.audio_hash != t2.audio_hash)
         LIMIT {lim} OFFSET {off}",
        lim = make_ph(1),
        off = make_ph(2),
    );
    let meta_params: &[&dyn ToSqlValue] = &[&limit_val, &offset_val];
    let meta_rows = state
        .backend
        .query_many(&meta_sql, meta_params)
        .ou_defaut_journalise();
    let meta_dups: Vec<Value> = meta_rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                "title": row.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": row.get(2).and_then(|v| v.as_string()),
                "file_path": row.get(3).and_then(|v| v.as_string()),
                "duration_ms": row.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "dup_id": row.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
                "dup_path": row.get(6).and_then(|v| v.as_string()),
                "dup_artist_name": row.get(7).and_then(|v| v.as_string()),
                "match_type": "metadata",
                "format": row.get(8).and_then(|v| v.as_string()),
                "sample_rate": row.get(9).and_then(|v| v.as_i64()),
                "bit_depth": row.get(10).and_then(|v| v.as_i64()),
                "dup_format": row.get(11).and_then(|v| v.as_string()),
                "dup_sample_rate": row.get(12).and_then(|v| v.as_i64()),
                "dup_bit_depth": row.get(13).and_then(|v| v.as_i64()),
            })
        })
        .collect();

    let fp_groups =
        tune_core::library::duplicate_detector::scan_fingerprint_duplicates(&state.backend);
    let fp_dups: Vec<Value> = fp_groups
        .iter()
        .map(|g| {
            json!({
                "fingerprint": g.hash,
                "tracks": g.tracks.iter().map(|t| json!({
                    "id": t.id, "title": t.title, "artist": t.artist_name, "file_path": t.file_path,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let content_dups = doublons_par_contenu(&state, limit, offset);
    // `?critere=` : une seule famille, dans `paires` COMME dans la liste
    // d'origine correspondante — les autres listes sont vides, pas absentes.
    let garde = |code: &str, liste: Vec<Value>| match filtre {
        Some(c) if c != code => Vec::new(),
        _ => liste,
    };
    let hash_dups = garde("fichier_identique", hash_dups);
    let meta_dups = garde("etiquettes_identiques", meta_dups);
    let fp_dups = garde("empreinte_identique", fp_dups);
    let content_dups = garde("contenu_identique", content_dups);
    let mut paires: Vec<Value> = Vec::new();
    for ligne in &hash_dups {
        paires.push(paire(
            "fichier_identique",
            fiche(ligne, false),
            fiche(ligne, true),
        ));
    }
    for groupe in &content_dups {
        paires.extend(paires_depuis_groupe("contenu_identique", groupe));
    }
    for groupe in &fp_dups {
        paires.extend(paires_depuis_groupe("empreinte_identique", groupe));
    }
    for ligne in &meta_dups {
        paires.push(paire(
            "etiquettes_identiques",
            fiche(ligne, false),
            fiche(ligne, true),
        ));
    }
    Ok(Json(json!({
        "duplicates": {
            "by_hash": hash_dups,
            "by_metadata": meta_dups,
            "by_fingerprint": fp_dups,
            "by_content": content_dups,
        },
        "paires": paires,
        "criteres": criteres_json(),
        "total": hash_dups.len() + meta_dups.len() + fp_dups.len() + content_dups.len(),
    })))
}

/// BIB-B2 (phase C) — les groupes de pistes dont le CONTENU décodé est le
/// même (`audio/empreinte.rs`) : le rip FLAC et sa copie AAC, l'AIFF et son
/// ALAC, deux résolutions du même master — ce que ni `audio_hash` (octets du
/// conteneur) ni les métadonnées ne voient. Empreintes lues telles quelles,
/// regroupées par `grouper_par_contenu` (durées à une seconde près, préfiltre
/// grossier, puis comparaison alignée avec tolérance). Base antérieure à la
/// colonne : liste vide, sans bruit. `limit`/`offset` portent sur les groupes.
fn doublons_par_contenu(state: &AppState, limit: i64, offset: i64) -> Vec<Value> {
    use tune_core::audio::empreinte::{Empreinte, grouper_par_contenu};
    let rows = match state.backend.query_many(
        "SELECT t.id, t.title, ar.name, t.file_path, t.duration_ms, t.format, t.sample_rate, \
                t.bit_depth, t.audio_fingerprint \
         FROM tracks t LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE t.audio_fingerprint IS NOT NULL",
        &[],
    ) {
        Ok(r) => r,
        Err(e) => {
            if !(e.contains("no such column") || e.contains("does not exist")) {
                tracing::warn!(error = %e, "duplicates_contenu_query_failed");
            }
            return Vec::new();
        }
    };
    let mut fiches: std::collections::HashMap<i64, Value> = std::collections::HashMap::new();
    let mut empreintes: Vec<(i64, Empreinte)> = Vec::new();
    for row in &rows {
        let Some(id) = row.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        let Some(empreinte) = row
            .get(8)
            .and_then(|v| v.as_string())
            .and_then(|t| Empreinte::deserialiser(&t))
        else {
            continue;
        };
        fiches.insert(
            id,
            json!({
                "id": id,
                "title": row.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": row.get(2).and_then(|v| v.as_string()),
                "file_path": row.get(3).and_then(|v| v.as_string()),
                "duration_ms": row.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "format": row.get(5).and_then(|v| v.as_string()),
                "sample_rate": row.get(6).and_then(|v| v.as_i64()),
                "bit_depth": row.get(7).and_then(|v| v.as_i64()),
            }),
        );
        empreintes.push((id, empreinte));
    }
    grouper_par_contenu(&empreintes)
        .into_iter()
        .skip(offset.max(0) as usize)
        .take(limit.max(0) as usize)
        .map(|ids| {
            json!({
                "match_type": "audio_content",
                "tracks": ids.iter().filter_map(|id| fiches.get(id).cloned()).collect::<Vec<_>>(),
            })
        })
        .collect()
}

pub(super) async fn resolve_duplicate(
    State(state): State<AppState>,
    Json(body): Json<ResolveDuplicate>,
) -> Result<impl IntoResponse, AppError> {
    let make_ph = |i: usize| match state.backend.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    };

    // Verify both tracks exist
    let keep_exists = state
        .backend
        .query_one(
            &format!("SELECT COUNT(*) FROM tracks WHERE id = {}", make_ph(1)),
            &[&body.keep_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        > 0;

    let delete_exists = state
        .backend
        .query_one(
            &format!("SELECT COUNT(*) FROM tracks WHERE id = {}", make_ph(1)),
            &[&body.delete_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        > 0;

    if !keep_exists || !delete_exists {
        return Err(AppError::not_found("track not found"));
    }

    // Reassign playlist references from deleted track to kept track
    state
        .backend
        .execute(
            &format!(
                "UPDATE playlist_tracks SET track_id = {} WHERE track_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Reassign play queue references (local rows only — streaming rows have
    // track_id NULL and are unaffected).
    state
        .backend
        .execute(
            &format!(
                "UPDATE queue_items SET track_id = {} WHERE track_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Reassign listen history references
    state
        .backend
        .execute(
            &format!(
                "UPDATE listen_history SET track_id = {} WHERE track_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Reassign bookmarks
    state
        .backend
        .execute(
            &format!(
                "UPDATE bookmarks SET track_id = {} WHERE track_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Reassign favorites
    state
        .backend
        .execute(
            &format!(
                "UPDATE favorites SET item_id = {} WHERE item_type = 'track' AND item_id = {}",
                make_ph(1),
                make_ph(2)
            ),
            &[&body.keep_id as &dyn ToSqlValue, &body.delete_id],
        )
        .ok();

    // Delete the duplicate track
    state
        .backend
        .execute(
            &format!("DELETE FROM tracks WHERE id = {}", make_ph(1)),
            &[&body.delete_id as &dyn ToSqlValue],
        )
        .ok();

    Ok(Json(json!({
        "kept": body.keep_id,
        "deleted": body.delete_id,
    })))
}

pub(super) async fn smart_duplicates(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(100);
    let offset = p.offset.unwrap_or(0);

    let make_ph = |i: usize| match state.backend.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    };

    let sql = format!(
        "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.duration_ms, t1.format, t1.sample_rate, t1.bit_depth, \
                t2.id, t2.file_path, t2.duration_ms, t2.format, t2.sample_rate, t2.bit_depth, ar2.name \
         FROM tracks t1 \
         JOIN tracks t2 ON LOWER(t1.title) = LOWER(t2.title) AND t1.id < t2.id \
         LEFT JOIN artists ar1 ON t1.artist_id = ar1.id \
         LEFT JOIN artists ar2 ON t2.artist_id = ar2.id \
         WHERE LOWER(ar1.name) = LOWER(ar2.name) \
           AND ABS(COALESCE(t1.duration_ms,0) - COALESCE(t2.duration_ms,0)) < 3000 \
         LIMIT {lim} OFFSET {off}",
        lim = make_ph(1),
        off = make_ph(2),
    );

    let limit_val = limit as i64;
    let offset_val = offset as i64;
    let params: &[&dyn ToSqlValue] = &[&limit_val, &offset_val];
    let rows = state
        .backend
        .query_many(&sql, params)
        .ou_defaut_journalise();

    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "track_a": {
                    "id": row.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                    "title": row.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    "artist": row.get(2).and_then(|v| v.as_string()),
                    "file_path": row.get(3).and_then(|v| v.as_string()),
                    "duration_ms": row.get(4).and_then(|v| v.as_i64()),
                    "format": row.get(5).and_then(|v| v.as_string()),
                    "sample_rate": row.get(6).and_then(|v| v.as_i64()),
                    "bit_depth": row.get(7).and_then(|v| v.as_i64()),
                },
                "track_b": {
                    "id": row.get(8).and_then(|v| v.as_i64()).unwrap_or(0),
                    "file_path": row.get(9).and_then(|v| v.as_string()),
                    "duration_ms": row.get(10).and_then(|v| v.as_i64()),
                    "format": row.get(11).and_then(|v| v.as_string()),
                    "sample_rate": row.get(12).and_then(|v| v.as_i64()),
                    "bit_depth": row.get(13).and_then(|v| v.as_i64()),
                    "artist": row.get(14).and_then(|v| v.as_string()),
                },
            })
        })
        .collect();

    Ok(Json(json!({
        "duplicates": items,
        "count": items.len(),
    })))
}

/// BIB-B2 (phase D) : où en est l'empreinte, et la rattraper À LA DEMANDE.
///
/// L'empreinte est posée par la passe ReplayGain, puis rattrapée en arrière-plan
/// par petits lots, seulement quand rien ne joue. Pour la calibrer sur une vraie
/// bibliothèque, il faut lire sa couverture et pouvoir forcer le rattrapage sans
/// attendre le créneau : c'est ce que rendent ces deux portes. Le rattrapage
/// forcé reste borné (`max` lots, 40 au plus) et respecte les mêmes gardes que
/// le fond (analyse désactivée, zone en lecture ⇒ le lot s'arrête).
pub(super) async fn couverture_empreintes(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let version = tune_core::audio::empreinte::VERSION;
    let motif = format!("{version}:%");
    let marque = format!("{version}:-");
    let sql = "SELECT COUNT(*), \
               SUM(CASE WHEN audio_fingerprint LIKE ? AND audio_fingerprint != ? THEN 1 ELSE 0 END), \
               SUM(CASE WHEN audio_fingerprint = ? THEN 1 ELSE 0 END), \
               SUM(CASE WHEN LOWER(COALESCE(format, '')) IN ('dsd', 'dsf', 'dff', 'dsdiff') THEN 1 ELSE 0 END) \
               FROM tracks WHERE file_path IS NOT NULL AND file_path != ''";
    let ligne = match state.backend.query_one(
        sql,
        &[
            &motif as &dyn ToSqlValue,
            &marque as &dyn ToSqlValue,
            &marque as &dyn ToSqlValue,
        ],
    ) {
        Ok(ligne) => ligne,
        Err(e) if e.contains("no such column") || e.contains("does not exist") => {
            return Ok(Json(json!({ "disponible": false, "version": version })));
        }
        Err(e) => {
            return Err(AppError::internal(format!(
                "couverture des empreintes : {e}"
            )));
        }
    };
    let n = |i: usize| {
        ligne
            .as_ref()
            .and_then(|l| l.get(i))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    let candidates =
        tune_core::audio::replaygain::compter_les_candidats_a_empreinter(&state.backend)
            .unwrap_or(0);
    let groupes = doublons_par_contenu(&state, i64::MAX, 0).len();
    Ok(Json(json!({
        "disponible": true,
        "version": version,
        "pistes": n(0),
        "avec_empreinte": n(1),
        "marquees_silence_ou_indecodable": n(2),
        "dsd_exclues": n(3),
        "candidates": candidates,
        "groupes_par_contenu": groupes,
        "analyse_active": tune_core::audio::replaygain::analysis_enabled(&state.backend),
    })))
}

#[derive(Deserialize)]
pub(super) struct ParamsEmpreinte {
    /// Nombre de lots à traiter maintenant (25 pistes par lot) ; 4 par défaut, 40 au plus.
    max: Option<usize>,
}

pub(super) async fn empreinter_maintenant(
    State(state): State<AppState>,
    Query(p): Query<ParamsEmpreinte>,
) -> Result<Json<Value>, AppError> {
    let max = p.max.unwrap_or(4).min(40);
    let mut lots = 0usize;
    let mut traitees = 0usize;
    while lots < max {
        let n = tune_core::audio::replaygain::empreinter_un_lot(&state.backend).await;
        lots += 1;
        traitees += n;
        if n == 0 {
            break;
        }
    }
    let restantes =
        tune_core::audio::replaygain::compter_les_candidats_a_empreinter(&state.backend)
            .unwrap_or(0);
    Ok(Json(json!({
        "lots": lots,
        "traitees": traitees,
        "restantes": restantes,
        "analyse_active": tune_core::audio::replaygain::analysis_enabled(&state.backend),
    })))
}

#[derive(Deserialize)]
pub(super) struct ScanParams {
    /// Plafond de pistes examinées. 0 (défaut) = toute la bibliothèque.
    limit: Option<usize>,
}

/// POST /library/duplicates/scan
///
/// Recalcule les empreintes audio manquantes et rend le compte des doublons.
///
/// `list_duplicates` ne rapproche que des pistes dont `audio_hash` est déjà
/// renseigné. Le scanner le calcule à l'indexation
/// (`scanner/walker.rs`), mais rien ne rattrapait les pistes entrées avant
/// que le hachage existe, ni celles dont il avait échoué : elles restaient
/// invisibles aux doublons pour toujours, sans que rien ne le signale.
///
/// Le moteur `duplicate_detector::scan_duplicates` fait exactement ce
/// rattrapage — il calcule l'empreinte absente, la PERSISTE, puis regroupe.
/// Il était complet dans `tune-core` et **n'avait aucun appelant** : une
/// porte HTTP manquait, et l'interface appelait `/metadata/duplicates/scan`,
/// qui n'a jamais existé (#1893).
///
/// Les deux champs rendus sont ceux que lit l'écran Métadonnées ; `groups`
/// n'est pas renvoyé, la liste détaillée étant le travail de
/// `GET /library/duplicates` qui la pagine.
pub(super) async fn scan_duplicates(
    State(state): State<AppState>,
    Query(p): Query<ScanParams>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(0);

    // Le scan lit et réécrit chaque fichier sans empreinte : sur une grande
    // bibliothèque il tient la durée d'un calcul de hachage par piste. Le
    // sortir du fil de la requête évite de bloquer l'exécuteur asynchrone.
    let backend = state.backend.clone();
    let resultat = tokio::task::spawn_blocking(move || {
        tune_core::library::duplicate_detector::scan_duplicates(&backend, limit)
    })
    .await
    .map_err(|e| AppError::internal(format!("scan de doublons interrompu : {e}")))?;

    Ok(Json(json!({
        "total_scanned": resultat.total_scanned,
        "duplicates_found": resultat.duplicates_found,
        "groups": resultat.groups.len(),
        "errors": resultat.errors,
    })))
}

#[cfg(test)]
pub(super) mod tests_contenu;
#[cfg(test)]
mod tests_empreintes;
#[cfg(test)]
mod tests_paires;
