//! #8958 — « Albums douteux » : marquer une alerte comme NORMALE.
//!
//! Demande de Didier (fil 2685) : une piste de 3 s volontaire, un disque
//! d'effets sonores sans artiste… reviennent à chaque ouverture de l'écran.
//! Une alerte marquée normale n'apparaît plus ; la liste des alertes ignorées
//! permet de revenir sur le choix.
//!
//! Une ligne par (portée, cible, raison). La portée dépend de la NATURE de
//! l'alerte ([`portee_pour`]) :
//!
//! * `missing_artist` : l'ALBUM quand la piste en a un (titré) — l'artiste
//!   manque en général sur tout le disque, et un album d'ambiances n'en aura
//!   jamais ; sinon la piste ;
//! * `very_short`, `missing_album` et toute autre raison : la PISTE — une
//!   piste courte voulue ne dit rien de ses voisines, et une piste sans album
//!   n'a pas d'album où ranger le choix.
//!
//! 🔴 Mémorisé pour TOUTE la bibliothèque, pas par profil : l'écran
//! Métadonnées agit sur la bibliothèque commune (les réparations qu'il
//! propose valent pour tous les profils) et n'a pas de vue par profil.
//!
//! Le filtre est en SQL ([`clause_non_ignoree`]) : le total de
//! `/metadata/doubtful`, sa pagination et la pastille du tableau de bord
//! (`doubtful_count`) comptent la même chose.
//!
//! Table sans numéro de migration, sur le modèle de `politiques_parentales`
//! (les numéros SQLite 155 et PG 121/122 sont disputés par plusieurs PR du
//! lot rc13) : passe finale de `run_migrations` (SQLite), `ENSURE_TABLES` et
//! `PG_FULL_SCHEMA` (PostgreSQL). Copiée à la bascule : c'est un choix de
//! l'utilisateur.

use super::backend::{DbBackend, ToSqlValue};
use std::sync::Arc;

/// Le DDL commun aux deux moteurs.
pub const SQL_TABLE: &str = include_str!("../../migrations/alertes_douteuses_ignorees.sql");

/// Les raisons que `/metadata/doubtful` peut rendre.
pub const RAISONS: &[&str] = &["missing_artist", "very_short", "missing_album"];

/// Où le choix est mémorisé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Portee {
    Piste,
    Album,
}

impl Portee {
    pub fn as_str(self) -> &'static str {
        match self {
            Portee::Piste => "track",
            Portee::Album => "album",
        }
    }

    pub fn depuis(s: &str) -> Option<Portee> {
        match s {
            "track" => Some(Portee::Piste),
            "album" => Some(Portee::Album),
            _ => None,
        }
    }
}

/// La portée d'une alerte `raison` sur une piste d'album `album_id` titré
/// `album_titre` (voir l'en-tête du module).
pub fn portee_pour(raison: &str, album_id: Option<i64>, album_titre: Option<&str>) -> Portee {
    let album_reel =
        album_id.is_some_and(|id| id > 0) && album_titre.is_some_and(|t| !t.trim().is_empty());
    if raison == "missing_artist" && album_reel {
        Portee::Album
    } else {
        Portee::Piste
    }
}

/// Le filtre d'une raison, sur une piste d'alias `t` : vrai tant que
/// l'alerte `raison` n'est ignorée ni pour la piste ni pour son album.
/// `raison` est une constante du code (jamais une saisie) : elle est inlinée.
pub fn clause_non_ignoree(raison: &'static str) -> String {
    debug_assert!(raison.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    format!(
        "NOT EXISTS (SELECT 1 FROM alertes_douteuses_ignorees ig \
         WHERE ig.raison = '{raison}' \
         AND ((ig.portee = 'track' AND ig.cible_id = t.id) \
           OR (ig.portee = 'album' AND ig.cible_id = t.album_id)))"
    )
}

/// Une alerte ignorée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlerteIgnoree {
    pub portee: Portee,
    pub cible_id: i64,
    pub raison: String,
    pub ignoree_le: i64,
}

/// Marque l'alerte comme normale. Rejouée, elle ne change rien. Rend vrai si
/// une ligne a été ajoutée.
pub fn ignorer(
    db: &Arc<dyn DbBackend>,
    portee: Portee,
    cible_id: i64,
    raison: &str,
    maintenant: i64,
) -> Result<bool, String> {
    let portee = portee.as_str();
    db.execute(
        "INSERT INTO alertes_douteuses_ignorees (portee, cible_id, raison, ignoree_le) \
         VALUES (?, ?, ?, ?) ON CONFLICT (portee, cible_id, raison) DO NOTHING",
        &[&portee as &dyn ToSqlValue, &cible_id, &raison, &maintenant],
    )
    .map(|n| n > 0)
}

/// Annule le choix : l'alerte redevient visible si elle tient toujours. Rend
/// vrai si une ligne a été retirée.
pub fn retablir(
    db: &Arc<dyn DbBackend>,
    portee: Portee,
    cible_id: i64,
    raison: &str,
) -> Result<bool, String> {
    let portee = portee.as_str();
    db.execute(
        "DELETE FROM alertes_douteuses_ignorees WHERE portee = ? AND cible_id = ? AND raison = ?",
        &[&portee as &dyn ToSqlValue, &cible_id, &raison],
    )
    .map(|n| n > 0)
}

/// Toutes les alertes ignorées, les plus récentes d'abord.
pub fn lister(db: &Arc<dyn DbBackend>) -> Result<Vec<AlerteIgnoree>, String> {
    let lignes = db.query_many_strong(
        "SELECT portee, cible_id, raison, ignoree_le FROM alertes_douteuses_ignorees \
         ORDER BY ignoree_le DESC, portee, cible_id, raison",
        &[],
    )?;
    Ok(lignes
        .iter()
        .filter_map(|l| {
            Some(AlerteIgnoree {
                portee: Portee::depuis(l.first()?.as_str()?)?,
                cible_id: l.get(1)?.as_i64()?,
                raison: l.get(2)?.as_string()?,
                ignoree_le: l.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

/// Retire les choix dont la piste ou l'album n'existe plus : un identifiant
/// réutilisé n'hérite pas du choix fait sur un élément supprimé. Rend le
/// nombre de lignes retirées.
pub fn purger_orphelines(db: &Arc<dyn DbBackend>) -> Result<usize, String> {
    db.execute(
        "DELETE FROM alertes_douteuses_ignorees WHERE \
         (portee = 'track' AND NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.id = alertes_douteuses_ignorees.cible_id)) \
         OR (portee = 'album' AND NOT EXISTS (SELECT 1 FROM albums WHERE albums.id = alertes_douteuses_ignorees.cible_id))",
        &[],
    )
}
