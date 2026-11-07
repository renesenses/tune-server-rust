//! #7983 — la trace des tentatives de la passe « Enrichir les métadonnées »
//! (`POST /library/enrich-all`).
//!
//! Sans elle, la passe ne savait pas ce qu'elle avait déjà essayé : une piste
//! restait candidate tant qu'il lui manquait un seul champ, et MusicBrainz n'a
//! souvent ni label, ni année, ni compositeur. Au redémarrage, la sélection
//! repartait des tout premiers identifiants et le reste à faire remontait
//! (≈ 500 000 → 517 000 sans aucun fichier ajouté, fil 2673).
//!
//! Une ligne par piste tentée :
//! * `fait` : écriture réussie, même s'il manque encore des champs que
//!   MusicBrainz n'a pas ;
//! * `introuvable` : MusicBrainz ne connaît pas l'enregistrement ;
//! * `echec_temporaire` : réseau ou service indisponible (503…) ; retentée
//!   après `reessai_apres`, délai qui double à chaque échec, plafonné.
//!
//! La marque ne vaut que pour le fichier vu : `mtime_vu` garde la date de
//! modification du fichier au moment de la tentative. Un fichier modifié, puis
//! relu par un scan, redevient candidat.
//!
//! Table sans numéro de migration, sur le modèle de `politiques_parentales` :
//! créée par la passe finale de `run_migrations` (SQLite) et par
//! `ENSURE_TABLES` (PostgreSQL), déclarée dans `PG_FULL_SCHEMA`, non copiée à
//! la bascule (la passe la reconstitue).

use super::backend::{DbBackend, ToSqlValue};
use std::sync::Arc;

/// Le DDL commun aux deux moteurs.
pub const SQL_TABLE: &str = include_str!("../../migrations/enrich_tentatives.sql");

/// L'index du compte des échecs à retenter, lu à l'ouverture de chaque passe.
pub const SQL_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_enrich_tentatives_issue \
     ON enrich_tentatives(issue, reessai_apres)";

/// Premier délai avant de retenter une piste en échec temporaire : 1 h.
pub const DELAI_INITIAL_S: i64 = 3600;
/// Plafond du délai : 7 jours.
pub const DELAI_MAX_S: i64 = 7 * 24 * 3600;

/// L'issue d'une tentative d'enrichissement d'une piste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issue {
    Fait,
    Introuvable,
    EchecTemporaire,
}

impl Issue {
    pub fn as_str(self) -> &'static str {
        match self {
            Issue::Fait => "fait",
            Issue::Introuvable => "introuvable",
            Issue::EchecTemporaire => "echec_temporaire",
        }
    }
}

/// Le délai avant la prochaine tentative après `echecs` échecs temporaires
/// consécutifs (1 au premier) : 1 h, 2 h, 4 h… jusqu'à 7 jours.
pub fn delai_avant_reessai(echecs: i64) -> i64 {
    let rang = echecs.clamp(1, 32) - 1;
    DELAI_INITIAL_S
        .saturating_mul(1i64 << rang)
        .min(DELAI_MAX_S)
}

/// Le filtre de la sélection, sur une piste d'alias `t` : écarte les pistes
/// déjà tentées sur ce fichier, sauf les échecs temporaires dont le délai est
/// écoulé à `maintenant` (secondes depuis l'époque, littéral entier).
pub fn clause_pas_deja_tentee(maintenant: i64) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM enrich_tentatives e WHERE e.track_id = t.id \
         AND COALESCE(e.mtime_vu, '') = COALESCE(CAST(t.file_mtime AS TEXT), '') \
         AND (e.issue <> 'echec_temporaire' OR e.reessai_apres > {maintenant}))"
    )
}

/// Sur une piste d'alias `t` déjà passée par [`clause_pas_deja_tentee`] :
/// 1 si c'est une nouvelle tentative (échec temporaire dont le délai est
/// écoulé), 0 si la piste n'a jamais été tentée sur ce fichier.
///
/// 🔴 Une nouvelle tentative n'est pas une piste restante. Triées en tête par
/// leur petit identifiant et comptées dans le total, les pistes en 503 d'avant
/// un redémarrage refaisaient remonter le reste à faire et repartir la passe
/// des premiers `track_id` dès que leur délai (1 h) était écoulé.
pub const SQL_EST_UN_REESSAI: &str = "CASE WHEN EXISTS (SELECT 1 FROM enrich_tentatives e \
     WHERE e.track_id = t.id \
     AND COALESCE(e.mtime_vu, '') = COALESCE(CAST(t.file_mtime AS TEXT), '')) \
     THEN 1 ELSE 0 END";

/// Retire la trace des pistes qui n'existent plus : un identifiant réutilisé
/// n'hérite pas de la marque d'une piste supprimée. Rend le nombre de lignes
/// retirées.
pub fn purger_orphelines(db: &Arc<dyn DbBackend>) -> Result<usize, String> {
    db.execute(
        "DELETE FROM enrich_tentatives WHERE NOT EXISTS \
         (SELECT 1 FROM tracks WHERE tracks.id = enrich_tentatives.track_id)",
        &[],
    )
}

/// Note l'issue d'une tentative. Un échec temporaire augmente le compte
/// d'échecs et repousse la prochaine tentative ; toute autre issue le remet à
/// zéro. Rend l'instant de la prochaine tentative (0 hors échec temporaire).
pub fn noter(
    db: &Arc<dyn DbBackend>,
    track_id: i64,
    issue: Issue,
    maintenant: i64,
) -> Result<i64, String> {
    let (echecs, reessai_apres) = if issue == Issue::EchecTemporaire {
        let precedents = db
            .query_one_strong(
                "SELECT echecs, issue FROM enrich_tentatives WHERE track_id = ?",
                &[&track_id as &dyn ToSqlValue],
            )?
            .filter(|r| r.get(1).and_then(|v| v.as_string()).as_deref() == Some("echec_temporaire"))
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        let echecs = precedents + 1;
        (echecs, maintenant + delai_avant_reessai(echecs))
    } else {
        (0, 0)
    };
    let issue = issue.as_str();
    db.execute(
        "INSERT INTO enrich_tentatives \
         (track_id, issue, echecs, tente_le, reessai_apres, mtime_vu) \
         SELECT id, ?, ?, ?, ?, CAST(file_mtime AS TEXT) FROM tracks WHERE id = ? \
         ON CONFLICT (track_id) DO UPDATE SET issue = excluded.issue, \
         echecs = excluded.echecs, tente_le = excluded.tente_le, \
         reessai_apres = excluded.reessai_apres, mtime_vu = excluded.mtime_vu",
        &[
            &issue as &dyn ToSqlValue,
            &echecs,
            &maintenant,
            &reessai_apres,
            &track_id,
        ],
    )?;
    Ok(reessai_apres)
}

/// Le nombre de pistes en échec temporaire qui attendent leur délai à
/// `maintenant` : elles ne sont pas dans la sélection, et ne sont pas
/// abandonnées.
pub fn compter_a_retenter(db: &Arc<dyn DbBackend>, maintenant: i64) -> Result<i64, String> {
    Ok(db
        .query_one(
            "SELECT COUNT(*) FROM enrich_tentatives \
             WHERE issue = 'echec_temporaire' AND reessai_apres > ?",
            &[&maintenant as &dyn ToSqlValue],
        )?
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0))
}

#[cfg(test)]
#[path = "enrich_tentatives_tests_7983.rs"]
mod tests;
