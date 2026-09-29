//! #5314 — le genre posé sur un ALBUM vaut pour ses PISTES.
//!
//! Décision de Bertrand du 27/09/2026 (issue #5314) : changer le genre d'un
//! album le recopie sur ses pistes (`tracks.genre` et `tracks.genres`), par la
//! fiche « Modifier », `update_album`, l'édition en masse et les réparations
//! de genres (Last.fm / Discogs, par artiste, par famille). Oxygen reste fondé
//! sur les genres des PISTES (`genre_facet`, `facets.rs`) ; l'onglet Genres
//! lit l'ALBUM. Sans recopie, un genre posé à la Bibliothèque n'atteignait
//! jamais Oxygen.
//!
//! # L'exception
//!
//! Une COMPILATION dont les pistes portent des genres DIFFÉRENTS garde les
//! siens — sauf demande explicite (« appliquer aussi aux pistes », `forcer`).
//! Le genre de l'album change quand même ; seules les pistes sont épargnées.
//!
//! # Pourquoi une analyse ne défait pas la recopie
//!
//! Un scan qui relit un fichier reconstruit la ligne piste depuis ses BALISES,
//! qui ne portent pas le genre posé à la fiche. La recopie laisse donc un
//! marqueur dans `album_metadata` ([`CLE_GENRE_PISTES`], valeur = le genre
//! recopié) ; [`super::edition_album::Tenues`] le charge une fois par scan et
//! le repose sur chaque piste de l'album AVANT qu'elle soit écrite — la même
//! porte que le titre et la disposition tenus à la main. Une valeur VIDE dit
//! « examiné, pistes épargnées » (l'exception) : rien n'est reposé.
//!
//! # Colonnes écrites
//!
//! Comme le scan (`scan_import::build_genres_json`,
//! `metadata::genres_from_tag_values`) : `tracks.genres` est le tableau JSON
//! des genres découpés, `tracks.genre` le PREMIER — « Jazz; Fusion » donne
//! `genre = "Jazz"`, `genres = ["Jazz","Fusion"]`. `albums.genres` est réécrit
//! du même tableau : l'onglet Genres le lit AVANT `albums.genre`
//! (`genres_de_l_album`), un ancien tableau y aurait survécu à l'édition.
//!
//! Tout passe par `DbBackend` et les marques de chaque dialecte : SQLite et
//! PostgreSQL jouent le même code (témoins communs, rejoués par
//! `postgres_e2e::pg_genre_album_pistes_5314`).
use std::collections::HashSet;
use std::sync::Arc;

use super::backend::{DbBackend, DbTxHandle, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Clé, dans `album_metadata`, du genre recopié sur les pistes. Valeur vide :
/// l'album a été examiné et ses pistes épargnées (compilation aux genres
/// différents).
pub const CLE_GENRE_PISTES: &str = "genre_pistes";

/// Ce que la recopie a fait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recopie {
    /// Genre recopié sur ce nombre de pistes.
    Faite(usize),
    /// Compilation dont les pistes ont des genres différents : épargnées.
    CompilationEpargnee,
    /// L'album n'a pas de genre (ou n'existe pas) : rien à recopier.
    SansGenre,
}

fn marque(engine: Engine, n: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

fn sql_upsert(engine: Engine) -> String {
    match engine {
        Engine::Sqlite => super::album_metadata_repo::sql::upsert(&SqliteDialect),
        Engine::Postgres => super::album_metadata_repo::sql::upsert(&PostgresDialect),
    }
}

/// `(tracks.genre, tracks.genres)` pour un genre d'album : le premier genre
/// découpé et le tableau JSON de tous. `None` pour un genre vide.
pub fn colonnes_de_piste(genre_album: &str) -> Option<(String, String)> {
    let liste = crate::metadata::genres_from_tag_values(&[genre_album]);
    let premier = liste.first()?.clone();
    let json = serde_json::to_string(&liste).ok()?;
    Some((premier, json))
}

/// Les clés de genre d'une piste (`genre_key`), triées : le tableau JSON s'il
/// est lisible et non vide, sinon la colonne découpée.
fn cles_de_piste(genre: Option<&str>, genres: Option<&str>) -> Vec<String> {
    let mut noms: Vec<String> = genres
        .and_then(|j| serde_json::from_str::<Vec<String>>(j).ok())
        .unwrap_or_default();
    noms.retain(|g| !g.trim().is_empty());
    if noms.is_empty()
        && let Some(g) = genre
    {
        noms = crate::metadata::split_genre_tag(g);
    }
    let mut cles: Vec<String> = noms
        .iter()
        .map(|g| crate::metadata::genre_key(g))
        .filter(|k| !k.is_empty())
        .collect();
    cles.sort();
    cles.dedup();
    cles
}

/// Recopie, DANS la transaction `tx`, le genre ACTUEL de l'album sur ses
/// pistes. À appeler après l'écriture du genre (et du drapeau compilation)
/// de l'album. `forcer` lève l'exception des compilations.
pub fn recopier_dans(
    tx: &dyn DbTxHandle,
    engine: Engine,
    album_id: i64,
    forcer: bool,
) -> Result<Recopie, String> {
    let p1 = marque(engine, 1);
    let p2 = marque(engine, 2);
    let p3 = marque(engine, 3);
    let id: &dyn ToSqlValue = &album_id;
    let Some(ligne) = tx.query_one(
        &format!("SELECT genre, is_compilation FROM albums WHERE id = {p1}"),
        &[id],
    )?
    else {
        return Ok(Recopie::SansGenre);
    };
    let genre = ligne
        .first()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let compilation = ligne.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
    let Some((premier, json)) = colonnes_de_piste(&genre) else {
        // Plus de genre d'album : on n'efface pas celui des pistes (il vient
        // de leurs balises), et on ne le reposera plus au scan.
        tx.execute(
            &format!("DELETE FROM album_metadata WHERE album_id = {p1} AND key = {p2}"),
            &[id, &CLE_GENRE_PISTES as &dyn ToSqlValue],
        )?;
        return Ok(Recopie::SansGenre);
    };
    tx.execute(
        &format!("UPDATE albums SET genres = {p1} WHERE id = {p2}"),
        &[&json as &dyn ToSqlValue, id],
    )?;
    if compilation && !forcer {
        let mut distincts: HashSet<Vec<String>> = HashSet::new();
        for r in tx.query_many(
            &format!("SELECT genre, genres FROM tracks WHERE album_id = {p1}"),
            &[id],
        )? {
            let g = r.first().and_then(|v| v.as_string());
            let gs = r.get(1).and_then(|v| v.as_string());
            let cles = cles_de_piste(g.as_deref(), gs.as_deref());
            if !cles.is_empty() {
                distincts.insert(cles);
            }
        }
        if distincts.len() > 1 {
            let vide = String::new();
            tx.execute(
                &sql_upsert(engine),
                &[
                    id,
                    &CLE_GENRE_PISTES as &dyn ToSqlValue,
                    &vide as &dyn ToSqlValue,
                ],
            )?;
            return Ok(Recopie::CompilationEpargnee);
        }
    }
    let n = tx.execute(
        &format!("UPDATE tracks SET genre = {p1}, genres = {p2} WHERE album_id = {p3}"),
        &[&premier as &dyn ToSqlValue, &json as &dyn ToSqlValue, id],
    )?;
    tx.execute(
        &sql_upsert(engine),
        &[
            id,
            &CLE_GENRE_PISTES as &dyn ToSqlValue,
            &genre as &dyn ToSqlValue,
        ],
    )?;
    Ok(Recopie::Faite(n))
}

/// [`recopier_dans`] dans sa propre transaction.
pub fn recopier(db: &Arc<dyn DbBackend>, album_id: i64, forcer: bool) -> Result<Recopie, String> {
    let engine = db.engine();
    let mut sortie = Recopie::SansGenre;
    db.write_tx(&mut |tx| {
        sortie = recopier_dans(tx, engine, album_id, forcer)?;
        Ok(())
    })?;
    tracing::debug!(album_id, forcer, recopie = ?sortie, "genre_album_recopie_sur_les_pistes");
    Ok(sortie)
}

/// Pour les appelants qui ne doivent PAS échouer sur la recopie (l'écriture
/// de l'album a déjà réussi) : l'échec se lit au journal.
pub fn recopier_ou_journaliser(db: &Arc<dyn DbBackend>, album_id: i64, forcer: bool) {
    if let Err(e) = recopier(db, album_id, forcer) {
        tracing::warn!(album_id, erreur = %e, "genre_album_non_recopie_sur_les_pistes");
    }
}

/// Vrai quand le genre d'album passe de `avant` à `apres` — casse et espaces
/// de bord compris comme une même valeur. « Changer le genre » est ce que la
/// décision recopie : un formulaire renvoyé tel quel ne réécrit pas les
/// pistes.
pub fn genre_change(avant: Option<&str>, apres: Option<&str>) -> bool {
    let norme = |g: Option<&str>| g.map(str::trim).unwrap_or("").to_lowercase();
    norme(avant) != norme(apres)
}

/// Le rattrapage des albums dont le genre a été posé À LA MAIN avant #5314
/// (`edition_manuelle` contient `"genre"`) et jamais recopié (aucun
/// [`CLE_GENRE_PISTES`]). Idempotent : un album traité porte le marqueur, même
/// vide, et n'est plus revu. Rend le nombre d'albums examinés.
pub fn rattraper_les_genres_tenus(db: &Arc<dyn DbBackend>) -> Result<usize, String> {
    let p1 = marque(db.engine(), 1);
    let sql = format!(
        "SELECT albums.id FROM albums WHERE albums.genre IS NOT NULL AND albums.genre <> '' \
         AND {} \
         AND NOT EXISTS (SELECT 1 FROM album_metadata g WHERE g.album_id = albums.id AND g.key = {p1}) \
         ORDER BY albums.id",
        super::album_repo::sql_champ_tenu_a_la_main("genre")
    );
    let ids: Vec<i64> = db
        .query_many_strong(&sql, &[&CLE_GENRE_PISTES as &dyn ToSqlValue])?
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let mut faits = 0usize;
    let mut epargnes = 0usize;
    for id in &ids {
        match recopier(db, *id, false)? {
            Recopie::Faite(_) => faits += 1,
            Recopie::CompilationEpargnee => epargnes += 1,
            Recopie::SansGenre => {}
        }
    }
    if !ids.is_empty() {
        tracing::info!(
            examines = ids.len(),
            recopies = faits,
            compilations_epargnees = epargnes,
            "rattrapage_genres_tenus_5314"
        );
    }
    Ok(ids.len())
}

#[cfg(test)]
#[path = "genre_album_pistes_tests.rs"]
pub(crate) mod tests;
