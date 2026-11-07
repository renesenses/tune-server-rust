//! Contrôle parental, étape 2 (#7974) : le bail d'écoute d'un profil
//! supervisé.
//!
//! Un bail se demande à chaque départ de lecture (et à chaque reprise ou
//! enchaînement) : il est accordé si l'heure du serveur tombe dans une plage
//! permise de la politique (étape 1, `politique_parentale`) et si le budget
//! du jour et de la semaine n'est pas épuisé. Il dit alors combien de
//! secondes il reste avant sa fin.
//!
//! - Sans politique, aucun bail n'est demandé : rien ne change.
//! - Le temps écouté se compte par profil et par jour local du serveur
//!   (`ecoutes_parentales`), seulement quand la zone joue : une pause ne
//!   compte pas. La clé de jour fait la remise à zéro quotidienne ; la
//!   semaine est la semaine ISO (lundi à dimanche).
//! - Le refus voyage en sentinelle structurée, comme `free_zone_cap:` :
//!   `parental_lease_refused:<raison>`. La couche HTTP en fait le code
//!   d'erreur dédié et la phrase traduite.
//!
//! Table sans numéro de migration, comme `politiques_parentales` : passe
//! finale de `run_migrations` (SQLite), `ENSURE_TABLES` et `PG_FULL_SCHEMA`
//! (PostgreSQL). Elle n'est pas copiée à la bascule : un compteur du jour
//! perdu ne fait que rendre du temps.

use super::backend::{DbBackend, ToSqlValue};
use super::politique_parentale::{self, PolitiqueParentale};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use std::sync::Arc;

/// Le DDL commun aux deux moteurs.
pub const SQL_TABLE: &str = include_str!("../../migrations/ecoutes_parentales.sql");

/// Le préfixe de la sentinelle d'erreur du refus.
pub const SENTINELLE: &str = "parental_lease_refused:";

/// Le code stable du refus, celui que les clients lisent.
pub const CODE_REFUS: &str = "parental_lease_refused";

/// Pourquoi un bail est refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refus {
    /// L'heure du serveur n'est dans aucune plage permise.
    HorsPlage,
    /// Le budget du jour est consommé.
    BudgetQuotidienEpuise,
    /// Le budget de la semaine est consommé.
    BudgetHebdomadaireEpuise,
    /// La politique du profil ne se lit pas : on ne laisse pas jouer.
    PolitiqueIllisible,
}

impl Refus {
    /// La raison, en code stable.
    pub fn code(self) -> &'static str {
        match self {
            Refus::HorsPlage => "hors_plage",
            Refus::BudgetQuotidienEpuise => "budget_quotidien_epuise",
            Refus::BudgetHebdomadaireEpuise => "budget_hebdomadaire_epuise",
            Refus::PolitiqueIllisible => "politique_illisible",
        }
    }

    /// L'erreur que rend l'orchestrateur.
    pub fn sentinelle(self) -> String {
        format!("{SENTINELLE}{}", self.code())
    }

    /// Relit la raison d'une erreur de l'orchestrateur.
    pub fn depuis_sentinelle(erreur: &str) -> Option<Refus> {
        let code = erreur.split(SENTINELLE).nth(1)?;
        let mut mots = code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
        match mots.next()? {
            "hors_plage" => Some(Refus::HorsPlage),
            "budget_quotidien_epuise" => Some(Refus::BudgetQuotidienEpuise),
            "budget_hebdomadaire_epuise" => Some(Refus::BudgetHebdomadaireEpuise),
            "politique_illisible" => Some(Refus::PolitiqueIllisible),
            _ => None,
        }
    }
}

/// Un bail accordé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bail {
    /// Secondes avant la fin de la plage ou du budget ; `None` : ni plage
    /// ni budget ne limitent (politique qui ne restreint que les listes).
    pub reste_s: Option<i64>,
}

/// Le bail d'une politique à un instant, le temps déjà écouté étant connu.
/// Fonction pure : l'horloge et les compteurs viennent de l'appelant.
pub fn evaluer(
    politique: &PolitiqueParentale,
    maintenant: NaiveDateTime,
    ecoute_du_jour_s: i64,
    ecoute_de_la_semaine_s: i64,
) -> Result<Bail, Refus> {
    let mut reste: Option<i64> = None;
    let mut borner = |r: i64| reste = Some(reste.map_or(r, |x| x.min(r)));

    if !politique.plages_horaires.is_empty() {
        let jour = maintenant.weekday().number_from_monday() as u8;
        let heure = maintenant.time();
        let lire = |h: &str| NaiveTime::parse_from_str(h, "%H:%M").ok();
        // La plage qui couvre l'instant et finit le plus tard.
        let fin = politique
            .plages_horaires
            .iter()
            .filter(|p| p.jours.contains(&jour))
            .filter_map(|p| Some((lire(&p.debut)?, lire(&p.fin)?)))
            .filter(|(debut, fin)| *debut <= heure && heure < *fin)
            .map(|(_, fin)| fin)
            .max()
            .ok_or(Refus::HorsPlage)?;
        borner((fin - heure).num_seconds());
    }
    if let Some(minutes) = politique.budget_quotidien_minutes {
        let r = i64::from(minutes) * 60 - ecoute_du_jour_s;
        if r <= 0 {
            return Err(Refus::BudgetQuotidienEpuise);
        }
        borner(r);
    }
    if let Some(minutes) = politique.budget_hebdomadaire_minutes {
        let r = i64::from(minutes) * 60 - ecoute_de_la_semaine_s;
        if r <= 0 {
            return Err(Refus::BudgetHebdomadaireEpuise);
        }
        borner(r);
    }
    Ok(Bail { reste_s: reste })
}

/// La clé de jour d'une date locale.
pub fn cle_du_jour(jour: NaiveDate) -> String {
    jour.format("%Y-%m-%d").to_string()
}

/// La politique a de quoi limiter la lecture (plage ou budget). Une politique
/// qui ne filtre que les listes ne demande ni bail ni décompte.
pub fn limite_la_lecture(p: &PolitiqueParentale) -> bool {
    !p.plages_horaires.is_empty()
        || p.budget_quotidien_minutes.is_some()
        || p.budget_hebdomadaire_minutes.is_some()
}

/// Ajoute `secondes` au temps écouté par le profil le jour `jour`.
pub fn crediter(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
    jour: NaiveDate,
    secondes: i64,
) -> Result<(), String> {
    if secondes <= 0 {
        return Ok(());
    }
    db.execute(
        "INSERT INTO ecoutes_parentales (profile_id, jour, secondes) VALUES (?, ?, ?) \
         ON CONFLICT (profile_id, jour) \
         DO UPDATE SET secondes = ecoutes_parentales.secondes + excluded.secondes",
        &[
            &profile_id as &dyn ToSqlValue,
            &cle_du_jour(jour),
            &secondes,
        ],
    )?;
    Ok(())
}

/// Secondes écoutées par le profil de `du` à `au`, bornes comprises.
pub fn ecoute_entre(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
    du: NaiveDate,
    au: NaiveDate,
) -> Result<i64, String> {
    Ok(db
        .query_one_strong(
            "SELECT COALESCE(SUM(secondes), 0) FROM ecoutes_parentales \
             WHERE profile_id = ? AND jour >= ? AND jour <= ?",
            &[
                &profile_id as &dyn ToSqlValue,
                &cle_du_jour(du),
                &cle_du_jour(au),
            ],
        )?
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0))
}

/// Le temps écouté le jour `jour` et dans sa semaine ISO, jusqu'à ce jour.
pub fn ecoute_du_jour_et_de_la_semaine(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
    jour: NaiveDate,
) -> Result<(i64, i64), String> {
    let lundi = jour - Duration::days(i64::from(jour.weekday().num_days_from_monday()));
    Ok((
        ecoute_entre(db, profile_id, jour, jour)?,
        ecoute_entre(db, profile_id, lundi, jour)?,
    ))
}

/// La politique du profil si elle limite la lecture.
///
/// `Ok(None)` : profil non supervisé, politique qui ne limite pas la lecture,
/// ou table absente (aucune politique n'a pu être posée). Une politique
/// présente mais illisible refuse, comme pour les listes. Une autre erreur
/// de base laisse passer en le journalisant : sans base lisible, on ne sait
/// pas si le profil est supervisé.
pub fn politique_limitante(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
) -> Result<Option<PolitiqueParentale>, Refus> {
    match politique_parentale::superviseur(db, profile_id) {
        Ok(None) => return Ok(None),
        Ok(Some(_)) => {}
        Err(e) if politique_parentale::table_absente(&e) => return Ok(None),
        Err(e) => {
            tracing::warn!(profile_id, error = %e, "bail_d_ecoute_politique_non_lue");
            return Ok(None);
        }
    }
    match politique_parentale::lire(db, profile_id) {
        Ok(Some(p)) if limite_la_lecture(&p.politique) => Ok(Some(p.politique)),
        Ok(_) => Ok(None),
        Err(e) => {
            tracing::warn!(profile_id, error = %e, "bail_d_ecoute_politique_illisible");
            Err(Refus::PolitiqueIllisible)
        }
    }
}

/// Le bail du profil à l'instant `maintenant` (heure locale du serveur).
/// `Ok(None)` : aucun bail à demander, la lecture est libre.
pub fn demander(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
    maintenant: NaiveDateTime,
) -> Result<Option<Bail>, Refus> {
    let Some(politique) = politique_limitante(db, profile_id)? else {
        return Ok(None);
    };
    let (jour, semaine) = ecoute_du_jour_et_de_la_semaine(db, profile_id, maintenant.date())
        .unwrap_or_else(|e| {
            tracing::warn!(profile_id, error = %e, "bail_d_ecoute_compteur_non_lu");
            (0, 0)
        });
    evaluer(&politique, maintenant, jour, semaine).map(Some)
}

/// L'heure locale du serveur, celle des plages et de la remise à zéro.
pub fn maintenant_local() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

#[cfg(test)]
#[path = "bail_d_ecoute_tests_7974.rs"]
mod tests;
