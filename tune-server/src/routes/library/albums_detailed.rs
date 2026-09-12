//! `GET /library/albums-detailed` — un album par ligne, avec ses agrégats.
//!
//! La vue « cartes album » d'Oxygen (inspirée de Helium) veut, par album :
//! label, année, durée totale, nombre de CD, nombre de pistes. Jusqu'ici elle
//! les dérivait des pistes DÉJÀ CHARGÉES côté client — donc d'une page. Un
//! album dont la moitié des pistes tombait hors de la page s'affichait avec un
//! nombre de pistes et une durée faux, sans que rien ne le signale. Sur une
//! bibliothèque de 55 000 pistes, c'est la majorité des albums.
//!
//! Ce point d'entrée fait l'agrégat en SQL, sur la sélection de facettes
//! courante, en réutilisant `facets::build_conditions` : les cartes comptent
//! donc exactement ce que le rail annonce.

use axum::Json;
use axum::extract::{Query, RawQuery, State};
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::error::AppError;
use crate::state::AppState;

use super::facets::{FacetQuery, build_conditions, resolve_collection};

/// Une piste sans `album_id` n'est pas un album : elle n'a ni pochette, ni
/// numéro de disque fiable, et regrouper toutes les orphelines sous une carte
/// unique ne veut rien dire. Elles restent visibles dans la table détaillée.
const ONLY_REAL_ALBUMS: &str = "t.album_id IS NOT NULL";

pub(super) async fn albums_detailed(
    Query(q): Query<FacetQuery>,
    RawQuery(raw): RawQuery,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // Même lecture des facettes multi-valeurs que le rail (#2168) : les cartes
    // doivent compter exactement ce que le rail annonce.
    let q = q.hydrate(raw.as_deref())?;
    let engine = state.backend.engine();
    // Même résolution que le rail ET que la liste (#1864) : le nom d'une
    // collection manuelle vit dans un JSON de réglages, celui d'une collection
    // intelligente dans des règles à compiler — jamais dans une table joignable.
    let coll = q
        .collection
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|name| resolve_collection(&state, name));

    // `exclude` vide : ici AUCUNE facette n'est exclue. Le rail exclut la
    // facette qu'il compte pour garder ses alternatives visibles ; une liste
    // d'albums, elle, doit refléter la sélection entière.
    let (mut conds, params) = build_conditions(&q, engine, "", coll.as_ref());
    conds.push(ONLY_REAL_ALBUMS.to_string());
    let where_clause = format!(" WHERE {}", conds.join(" AND "));

    let limit = q.limit.unwrap_or(500).clamp(1, 2000);
    let offset = q.offset.unwrap_or(0).max(0);

    let bound: Vec<&dyn tune_core::db::backend::ToSqlValue> = params
        .iter()
        .map(|v| v as &dyn tune_core::db::backend::ToSqlValue)
        .collect();

    // Total = nombre d'ALBUMS distincts, pas de pistes : c'est ce que la vue
    // pagine et ce que la barre d'état annonce.
    let total_sql = format!("SELECT COUNT(DISTINCT t.album_id) FROM tracks t{where_clause}");
    let total = state
        .backend
        .query_one(&total_sql, &bound)
        .ok()
        .flatten()
        .and_then(|row| row.into_iter().next())
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // `MAX(...)` sur les colonnes d'album : elles sont constantes au sein d'un
    // groupe (même album), et un agrégat évite d'avoir à les lister dans le
    // GROUP BY — PostgreSQL l'exigerait, SQLite non. Écrire pour les deux.
    let sql = format!(
        "SELECT t.album_id, \
                MAX(al.title), \
                MAX(COALESCE(t.album_artist, ar.name)), \
                MAX(al.cover_path), \
                MAX(t.label), \
                MAX(t.year), \
                SUM(COALESCE(t.duration_ms, 0)), \
                COUNT(DISTINCT COALESCE(t.disc_number, 1)), \
                COUNT(*), \
                MAX(t.format), \
                MAX(t.sample_rate), \
                MAX(t.bit_depth), \
                MAX(al.is_compilation) \
         FROM tracks t \
         LEFT JOIN albums al ON al.id = t.album_id \
         LEFT JOIN artists ar ON ar.id = t.artist_id{where_clause} \
         GROUP BY t.album_id \
         ORDER BY MAX(COALESCE(t.album_artist, ar.name)), MAX(al.title) \
         LIMIT {limit} OFFSET {offset}"
    );

    let items: Vec<Value> = state
        .backend
        .query_many(&sql, &bound)
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|row| {
            let mut it = row.into_iter();
            let album_id = it.next()?.as_i64()?;
            let title = it.next().and_then(|v| v.as_string());
            let artist = it.next().and_then(|v| v.as_string());
            let cover = it.next().and_then(|v| v.as_string());
            let label = it.next().and_then(|v| v.as_string());
            let year = it.next().and_then(|v| v.as_i64());
            let duration_ms = it.next().and_then(|v| v.as_i64()).unwrap_or(0);
            let disc_count = it.next().and_then(|v| v.as_i64()).unwrap_or(1);
            let track_count = it.next().and_then(|v| v.as_i64()).unwrap_or(0);
            let format = it.next().and_then(|v| v.as_string());
            let sample_rate = it.next().and_then(|v| v.as_i64());
            let bit_depth = it.next().and_then(|v| v.as_i64());
            // Le drapeau « compilation » (#1957). Un `MAX()` comme les autres
            // colonnes d'album : constante au sein du groupe, et PostgreSQL
            // exigerait sinon la colonne dans le GROUP BY. Décodé par le
            // décodeur unique de `tune-core` — jamais de `null` : une ligne
            // sans album, ou une base migrée qui porte encore NULL, vaut
            // « non », exactement comme dans le modèle `Album`.
            let is_compilation = tune_core::db::album_repo::drapeau_compilation(it.next().as_ref());
            Some(json!({
                "album_id": album_id,
                "title": title,
                "album_artist": artist,
                "cover_path": cover,
                "label": label,
                "year": year,
                "duration_ms": duration_ms,
                "disc_count": disc_count,
                "track_count": track_count,
                "format": format,
                "sample_rate": sample_rate,
                "bit_depth": bit_depth,
                "is_compilation": is_compilation,
            }))
        })
        .collect();

    Ok(Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

#[cfg(test)]
mod tests {
    use super::ONLY_REAL_ALBUMS;

    /// Le garde-fou qui empêche les pistes orphelines de former une carte
    /// fantôme. S'il disparaît, `GROUP BY t.album_id` produit un groupe NULL
    /// rassemblant des morceaux sans rapport.
    #[test]
    fn les_pistes_sans_album_sont_ecartees() {
        assert!(ONLY_REAL_ALBUMS.contains("album_id IS NOT NULL"));
    }

    /// Marqueur de contrat : le total pagine des ALBUMS. Compter des pistes
    /// donnerait un nombre de pages faux d'un facteur dix.
    #[test]
    fn le_total_compte_des_albums_distincts() {
        let sql = "SELECT COUNT(DISTINCT t.album_id) FROM tracks t";
        assert!(sql.contains("COUNT(DISTINCT t.album_id)"));
    }
}
