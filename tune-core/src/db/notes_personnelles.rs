//! #7849 — les notes personnelles en Markdown d'un profil sur un artiste, un
//! album ou une piste (table `notes_personnelles`, migrations 136 / PG 101).
//!
//! Une note appartient à UN profil : chaque requête de ce module porte
//! `profile_id` dans sa clause, lecture comme écriture. Un profil ne voit,
//! ne modifie ni ne supprime jamais la note d'un autre.
//!
//! Le texte est gardé tel quel : aucun assainissement ici, le client web le
//! fait au rendu.
//!
//! Notes d'album et `album_ratings.note` : l'ancienne colonne reste lue tant
//! que le client web n'a pas basculé. Une écriture d'ici la tient donc à jour
//! quand la ligne de note d'album existe (voir [`ecrire`] et [`supprimer`]),
//! et `RatingRepo::rate_album` recopie dans l'autre sens.

use serde::Serialize;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::Engine;

/// Le genre d'objet annoté.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TypeDObjet {
    Artist,
    Album,
    Track,
}

impl TypeDObjet {
    pub fn as_str(self) -> &'static str {
        match self {
            TypeDObjet::Artist => "artist",
            TypeDObjet::Album => "album",
            TypeDObjet::Track => "track",
        }
    }

    /// `artist`, `album` ou `track` ; tout autre mot est refusé.
    pub fn lire(mot: &str) -> Option<Self> {
        match mot {
            "artist" => Some(TypeDObjet::Artist),
            "album" => Some(TypeDObjet::Album),
            "track" => Some(TypeDObjet::Track),
            _ => None,
        }
    }
}

/// Une note telle que la table la garde.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotePersonnelle {
    pub item_type: TypeDObjet,
    pub item_id: i64,
    pub texte_md: String,
    pub maj_le: String,
}

fn marque(engine: Engine, rang: usize) -> String {
    match engine {
        Engine::Sqlite => "?".to_string(),
        Engine::Postgres => format!("${rang}"),
    }
}

fn maintenant() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn ligne_en_note(ligne: &[SqlValue]) -> Option<NotePersonnelle> {
    Some(NotePersonnelle {
        item_type: TypeDObjet::lire(&ligne.first()?.as_string()?)?,
        item_id: ligne.get(1)?.as_i64()?,
        texte_md: ligne.get(2)?.as_string().unwrap_or_default(),
        maj_le: ligne.get(3)?.as_string().unwrap_or_default(),
    })
}

/// La note du profil sur un objet, ou `None`.
pub fn lire(
    db: &dyn DbBackend,
    profile_id: i64,
    item_type: TypeDObjet,
    item_id: i64,
) -> Result<Option<NotePersonnelle>, String> {
    let e = db.engine();
    let sql = format!(
        "SELECT item_type, item_id, texte_md, maj_le FROM notes_personnelles \
         WHERE profile_id = {} AND item_type = {} AND item_id = {}",
        marque(e, 1),
        marque(e, 2),
        marque(e, 3)
    );
    let genre = item_type.as_str();
    let ligne = db.query_one(&sql, &[&profile_id as &dyn ToSqlValue, &genre, &item_id])?;
    Ok(ligne.as_deref().and_then(ligne_en_note))
}

/// Les notes du profil, les plus récentes d'abord, filtrées par type et par
/// texte (`q` : sous-chaîne, sans tenir compte de la casse ASCII ; `%` et `_`
/// y sont des caractères comme les autres).
pub fn lister(
    db: &dyn DbBackend,
    profile_id: i64,
    item_type: Option<TypeDObjet>,
    q: Option<&str>,
    limite: i64,
) -> Result<Vec<NotePersonnelle>, String> {
    let e = db.engine();
    let genre = item_type.map(TypeDObjet::as_str);
    let motif = q.map(str::trim).filter(|q| !q.is_empty()).map(|q| {
        let echappe = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("%{}%", echappe.to_lowercase())
    });
    let mut parametres: Vec<&dyn ToSqlValue> = vec![&profile_id];
    let mut clauses = vec![format!("profile_id = {}", marque(e, 1))];
    if let Some(genre) = genre.as_ref() {
        parametres.push(genre);
        clauses.push(format!("item_type = {}", marque(e, parametres.len())));
    }
    if let Some(motif) = motif.as_ref() {
        parametres.push(motif);
        clauses.push(format!(
            "LOWER(texte_md) LIKE {} ESCAPE '\\'",
            marque(e, parametres.len())
        ));
    }
    parametres.push(&limite);
    let sql = format!(
        "SELECT item_type, item_id, texte_md, maj_le FROM notes_personnelles \
         WHERE {} ORDER BY maj_le DESC, item_type, item_id LIMIT {}",
        clauses.join(" AND "),
        marque(e, parametres.len())
    );
    Ok(db
        .query_many(&sql, &parametres)?
        .iter()
        .filter_map(|l| ligne_en_note(l))
        .collect())
}

/// Crée ou remplace la note du profil sur un objet. Un texte vide (après
/// retrait des blancs) supprime la note : une note vide n'a pas de sens.
/// Rend la note écrite, ou `None` si elle a été supprimée.
pub fn ecrire(
    db: &dyn DbBackend,
    profile_id: i64,
    item_type: TypeDObjet,
    item_id: i64,
    texte_md: &str,
) -> Result<Option<NotePersonnelle>, String> {
    if texte_md.trim().is_empty() {
        supprimer(db, profile_id, item_type, item_id)?;
        return Ok(None);
    }
    let e = db.engine();
    let maj_le = maintenant();
    let genre = item_type.as_str();
    let sql = format!(
        "INSERT INTO notes_personnelles (profile_id, item_type, item_id, texte_md, maj_le) \
         VALUES ({}, {}, {}, {}, {}) \
         ON CONFLICT (profile_id, item_type, item_id) \
         DO UPDATE SET texte_md = excluded.texte_md, maj_le = excluded.maj_le",
        marque(e, 1),
        marque(e, 2),
        marque(e, 3),
        marque(e, 4),
        marque(e, 5)
    );
    db.execute(
        &sql,
        &[
            &profile_id as &dyn ToSqlValue,
            &genre,
            &item_id,
            &texte_md,
            &maj_le,
        ],
    )?;
    if item_type == TypeDObjet::Album {
        recopier_dans_album_ratings(db, profile_id, item_id, Some(texte_md))?;
    }
    Ok(Some(NotePersonnelle {
        item_type,
        item_id,
        texte_md: texte_md.to_string(),
        maj_le,
    }))
}

/// Supprime la note du profil sur un objet. Rend `true` si une note existait.
pub fn supprimer(
    db: &dyn DbBackend,
    profile_id: i64,
    item_type: TypeDObjet,
    item_id: i64,
) -> Result<bool, String> {
    let e = db.engine();
    let genre = item_type.as_str();
    let sql = format!(
        "DELETE FROM notes_personnelles \
         WHERE profile_id = {} AND item_type = {} AND item_id = {}",
        marque(e, 1),
        marque(e, 2),
        marque(e, 3)
    );
    let n = db.execute(&sql, &[&profile_id as &dyn ToSqlValue, &genre, &item_id])?;
    if item_type == TypeDObjet::Album {
        recopier_dans_album_ratings(db, profile_id, item_id, None)?;
    }
    Ok(n > 0)
}

/// Tient `album_ratings.note` à jour tant que le client web la lit. Seule une
/// ligne existante est touchée : une note d'album n'a pas besoin d'une note
/// sur cinq, et la table des notes sur cinq n'accepte pas de ligne sans elle.
fn recopier_dans_album_ratings(
    db: &dyn DbBackend,
    profile_id: i64,
    album_id: i64,
    texte_md: Option<&str>,
) -> Result<(), String> {
    let e = db.engine();
    let sql = format!(
        "UPDATE album_ratings SET note = {} WHERE album_id = {} AND profile_id = {}",
        marque(e, 1),
        marque(e, 2),
        marque(e, 3)
    );
    db.execute(
        &sql,
        &[&texte_md as &dyn ToSqlValue, &album_id, &profile_id],
    )?;
    Ok(())
}

/// Le sens inverse, pour `RatingRepo::rate_album` : une note d'album écrite
/// par l'ancienne route arrive aussi dans `notes_personnelles`. Une note
/// absente ou vide n'efface rien ici — l'ancienne route ne distingue pas
/// « note effacée » de « note non envoyée ».
pub fn recevoir_note_d_album(
    db: &dyn DbBackend,
    profile_id: i64,
    album_id: i64,
    note: Option<&str>,
) -> Result<(), String> {
    let Some(texte) = note.filter(|t| !t.trim().is_empty()) else {
        return Ok(());
    };
    ecrire(db, profile_id, TypeDObjet::Album, album_id, texte).map(|_| ())
}
