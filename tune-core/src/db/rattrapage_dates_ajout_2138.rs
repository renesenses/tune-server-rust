//! Fil 2138 — rattrapage UNIQUE des dates d'ajout figées au premier scan.
//!
//! Jusqu'à #5748, le premier scan d'une base NEUVE datait chaque fichier par
//! « maintenant » (`file_first_seen.first_seen_at`). Sur une installation
//! neuve, tout entre au même scan : toutes les pistes portent la date de ce
//! scan, à quelques minutes près, et le tri « date d'ajout » retombe sur
//! l'ordre de parcours des dossiers. #5748 corrige les bases NEUVES (la
//! première vue devient la date de modification du fichier) ; il ne touche
//! pas aux bases déjà faites. C'est l'objet de cette passe.
//!
//! # Ce que la passe corrige, et rien d'autre
//!
//! La colonne corrigée est `file_first_seen.first_seen_at`, celle que lisent
//! le tri « date d'ajout » de la bibliothèque (`AlbumRepo::ADDED_AT_JOIN`) et
//! « Ajoutés récemment » ([`super::home_queries::DATE_D_AJOUT`]) : la date
//! d'ajout y vaut `COALESCE(ffs.first_seen_at, t.file_mtime)`. Aucun cache ne
//! la retient côté serveur : elle est relue à chaque requête.
//!
//! 1. **Détection prudente** ([`mesurer_le_bloc`], [`est_figee`]) : les dates des
//!    pistes locales sont triées ; le BLOC du premier scan est la fenêtre de
//!    [`DUREE_MAX_DU_BLOC_S`] qui porte le PLUS de pistes (fil 2203 : un
//!    premier scan fait en plusieurs fois, ou quelques pistes datées avant
//!    lui, coupaient l'ancien bloc, ancré sur la date la plus ancienne et
//!    fermé au premier trou de dix minutes). La base est dite
//!    figée si ce bloc porte au moins [`PROPORTION_FIGEE_MIN`] des pistes
//!    locales (pistes sans ligne `file_first_seen` comprises, au
//!    dénominateur), et qu'elle en a au moins [`PISTES_MIN`].
//! 2. **Base figée** : pour les pistes DU BLOC seulement, la date devient la
//!    date de modification du fichier — `tracks.file_mtime` si la base en a
//!    une, sinon un `stat` (l'audio n'est jamais relu) — bornée à la date
//!    figée : le fichier était dans la bibliothèque à ce scan, son ajout ne
//!    peut pas être plus tardif. Une piste dont le fichier est ABSENT garde sa
//!    date. L'`UPDATE` exige l'ancienne valeur exacte : une date déjà
//!    différente de la date figée n'est jamais touchée.
//! 3. **Sinon** : rien n'est écrit, hormis le marqueur.
//!
//! Le marqueur [`CLE_RATTRAPAGE_DATES_AJOUT_2138`] (`settings`) est posé
//! dans la même transaction que les corrections : la passe ne se rejoue pas.
//! Une exception, une seule fois : une base que la passe de la rc3 a dite
//! « non figée » ([`MARQUEUR_NON_FIGEE_RC3`]) est réexaminée par la détection
//! du fil 2203, puis marquée [`MARQUEUR_NON_FIGEE`].
//! Le poser aussi quand la base n'est pas figée est sûr : depuis #5748, un
//! premier scan ne fige plus rien, une base saine ne peut pas le devenir.

use std::collections::HashMap;
use std::sync::Arc;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Clé de `settings` qui marque la passe comme faite.
pub const CLE_RATTRAPAGE_DATES_AJOUT_2138: &str = "rattrapage_dates_ajout_premier_scan_2138";

/// Part minimale des pistes locales dans le bloc du premier scan.
///
/// 80 % : une installation neuve scannée avant #5748 en porte ~100 %, et
/// l'écart laisse la place aux pistes ajoutées depuis (une bibliothèque qui a
/// grossi d'un quart après son premier scan reste détectée). En dessous, la
/// bibliothèque a grandi d'au moins un quart après le premier scan : le tri
/// « date d'ajout » y dit déjà quelque chose, et l'emporter sur l'avis de
/// l'utilisateur serait un pari.
pub const PROPORTION_FIGEE_MIN: f64 = 0.80;

/// Largeur de la fenêtre du bloc : le premier scan d'une très grosse
/// bibliothèque sur un Raspberry Pi peut durer des heures, être interrompu
/// puis repris, jamais s'étaler sur des jours. Plus de borne sur l'écart
/// entre deux lots (fil 2203) : un scan arrêté puis relancé trois heures plus
/// tard reste le premier scan.
pub const DUREE_MAX_DU_BLOC_S: f64 = 72.0 * 3600.0;

/// Marqueur posé par la passe de la rc3 sur une base dite non figée : sa
/// détection était trop étroite (fil 2203), la base est réexaminée une fois.
pub const MARQUEUR_NON_FIGEE_RC3: &str = "non_figee";

/// Marqueur d'une base non figée selon la détection du fil 2203 : définitif.
pub const MARQUEUR_NON_FIGEE: &str = "non_figee_2203";

/// En dessous, une proportion ne dit rien : la passe ne conclut pas.
pub const PISTES_MIN: usize = 20;

/// Bilan d'une passe sur une base figée.
#[derive(Debug, Clone, PartialEq)]
pub struct Bilan {
    /// Pistes locales distinctes (par chemin).
    pub pistes: usize,
    /// Pistes du bloc du premier scan.
    pub dans_le_bloc: usize,
    /// Pistes du bloc redatées par la date de modification du fichier.
    pub corrigees: usize,
    /// Pistes du bloc laissées : date de modification inconnue, ou pas plus
    /// ancienne que la date figée.
    pub laissees: usize,
    /// Pistes du bloc dont le fichier est absent : date gardée.
    pub absentes: usize,
    /// Bornes du bloc (epoch, secondes).
    pub debut: f64,
    pub fin: f64,
}

/// Issue de [`rattraper_les_dates_d_ajout`].
#[derive(Debug, Clone, PartialEq)]
pub enum Issue {
    /// Le marqueur est déjà posé : rien n'a été lu ni écrit.
    DejaFaite,
    /// La base n'est pas figée : seul le marqueur est posé.
    NonFigee { pistes: usize, dans_le_bloc: usize },
    /// La base était figée : corrections et marqueur, en une transaction.
    Corrigee(Bilan),
}

/// Le bloc du premier scan : `(debut, fin, nombre)`, mesuré sur `dates`, la
/// date d'ajout de chaque piste locale qui en a une. `None` sans date valable.
///
/// C'est la fenêtre d'au plus [`DUREE_MAX_DU_BLOC_S`] qui porte le plus de
/// dates (la plus ancienne à égalité) ; `debut` et `fin` sont ses dates
/// extrêmes. Ni une piste datée bien avant le scan, ni un scan repris après
/// une pause ne la coupent (fil 2203).
pub fn mesurer_le_bloc(dates: &[f64]) -> Option<(f64, f64, usize)> {
    let mut triees: Vec<f64> = dates
        .iter()
        .copied()
        .filter(|d| d.is_finite() && *d > 0.0)
        .collect();
    triees.sort_by(|a, b| a.total_cmp(b));
    let mut meilleur = (*triees.first()?, *triees.first()?, 0usize);
    let mut gauche = 0usize;
    for (droite, &d) in triees.iter().enumerate() {
        while d - triees[gauche] > DUREE_MAX_DU_BLOC_S {
            gauche += 1;
        }
        let nombre = droite + 1 - gauche;
        if nombre > meilleur.2 {
            meilleur = (triees[gauche], d, nombre);
        }
    }
    Some(meilleur)
}

/// La base est-elle figée au premier scan ? `dans_le_bloc` pistes sur
/// `pistes` pistes locales (celles sans date comprises).
pub fn est_figee(dans_le_bloc: usize, pistes: usize) -> bool {
    pistes >= PISTES_MIN && dans_le_bloc as f64 >= PROPORTION_FIGEE_MIN * pistes as f64
}

/// La nouvelle date d'une piste du bloc : sa date de modification (`file_mtime`
/// de la base, sinon celle du disque), si elle est valable et PLUS ANCIENNE
/// que la date figée d'au moins une seconde. `None` : on garde la date.
pub fn nouvelle_date(
    mtime_base: Option<f64>,
    mtime_disque: Option<f64>,
    figee: f64,
) -> Option<f64> {
    let valable = |d: &f64| d.is_finite() && *d > 0.0;
    mtime_base
        .filter(valable)
        .or(mtime_disque.filter(valable))
        .filter(|d| *d < figee - 1.0)
}

fn mtime_du_disque(meta: &std::fs::Metadata) -> Option<f64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
}

fn placeholders(engine: Engine) -> (String, String, String) {
    match engine {
        Engine::Sqlite => (
            SqliteDialect.placeholder(1),
            SqliteDialect.placeholder(2),
            SqliteDialect.placeholder(3),
        ),
        Engine::Postgres => (
            PostgresDialect.placeholder(1),
            PostgresDialect.placeholder(2),
            PostgresDialect.placeholder(3),
        ),
    }
}

/// Les pistes LOCALES, une par chemin : `(chemin, date figée ou NULL,
/// file_mtime ou NULL)`. Même transtypage du `file_mtime` que
/// `DATE_D_AJOUT` : TEXT ou DOUBLE selon le millésime d'une base PostgreSQL.
const SQL_PISTES_LOCALES: &str = "SELECT t.file_path, ffs.first_seen_at, \
     CAST(NULLIF(CAST(t.file_mtime AS TEXT), '') AS DOUBLE PRECISION) \
     FROM tracks t LEFT JOIN file_first_seen ffs ON ffs.file_path = t.file_path \
     WHERE t.file_path IS NOT NULL AND t.file_path <> '' AND t.file_path NOT LIKE 'http%'";

struct Piste {
    figee: Option<f64>,
    mtime: Option<f64>,
}

/// La passe est-elle faite ? Oui dès qu'un marqueur est posé, sauf le
/// « non figée » de la rc3 ([`MARQUEUR_NON_FIGEE_RC3`]), réexaminé une fois.
fn lire_le_marqueur(db: &Arc<dyn DbBackend>) -> Result<bool, String> {
    let (p1, _, _) = placeholders(db.engine());
    let sql = format!("SELECT value FROM settings WHERE key = {p1}");
    let params: [&dyn ToSqlValue; 1] = [&CLE_RATTRAPAGE_DATES_AJOUT_2138];
    Ok(match db.query_one_strong(&sql, &params)? {
        None => false,
        Some(ligne) => {
            ligne.first().and_then(SqlValue::as_string).as_deref() != Some(MARQUEUR_NON_FIGEE_RC3)
        }
    })
}

/// La passe : détection, corrections et marqueur. Voir la note de module.
pub fn rattraper_les_dates_d_ajout(db: &Arc<dyn DbBackend>) -> Result<Issue, String> {
    if lire_le_marqueur(db)? {
        return Ok(Issue::DejaFaite);
    }
    let engine = db.engine();
    let mut pistes: HashMap<String, Piste> = HashMap::new();
    for ligne in db.query_many_strong(SQL_PISTES_LOCALES, &[])? {
        let Some(chemin) = ligne.first().and_then(SqlValue::as_string) else {
            continue;
        };
        let figee = ligne.get(1).and_then(SqlValue::as_f64);
        let mtime = ligne.get(2).and_then(SqlValue::as_f64);
        pistes.entry(chemin).or_insert(Piste { figee, mtime });
    }
    let dates: Vec<f64> = pistes.values().filter_map(|p| p.figee).collect();
    let bloc = mesurer_le_bloc(&dates);

    let mut corrections: Vec<(String, f64, f64)> = Vec::new();
    let issue = match bloc {
        Some((debut, fin, dans_le_bloc)) if est_figee(dans_le_bloc, pistes.len()) => {
            let mut bilan = Bilan {
                pistes: pistes.len(),
                dans_le_bloc,
                corrigees: 0,
                laissees: 0,
                absentes: 0,
                debut,
                fin,
            };
            for (chemin, piste) in &pistes {
                let Some(figee) = piste.figee.filter(|d| *d >= debut && *d <= fin) else {
                    continue;
                };
                let Ok(meta) = std::fs::metadata(chemin) else {
                    bilan.absentes += 1;
                    continue;
                };
                match nouvelle_date(piste.mtime, mtime_du_disque(&meta), figee) {
                    Some(date) => corrections.push((chemin.clone(), date, figee)),
                    None => bilan.laissees += 1,
                }
            }
            Issue::Corrigee(bilan)
        }
        bloc => Issue::NonFigee {
            pistes: pistes.len(),
            dans_le_bloc: bloc.map_or(0, |(_, _, n)| n),
        },
    };

    let (p1, p2, p3) = placeholders(engine);
    let maj = format!(
        "UPDATE file_first_seen SET first_seen_at = {p1} WHERE file_path = {p2} AND first_seen_at = {p3}"
    );
    let marqueur = format!(
        "INSERT INTO settings (key, value, updated_at) VALUES ({p1}, {p2}, {p3}) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at"
    );
    let maintenant = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut ecrites = 0usize;
    db.write_tx(&mut |tx| {
        ecrites = 0;
        for (chemin, date, figee) in &corrections {
            let params: [&dyn ToSqlValue; 3] = [date, chemin, figee];
            ecrites += tx.execute(&maj, &params)?;
        }
        // La valeur du marqueur dit ce qui a été ÉCRIT, pas ce qui était prévu.
        let valeur = match &issue {
            Issue::Corrigee(b) => format!(
                "corrigees={ecrites};laissees={};absentes={}",
                b.laissees + corrections.len().saturating_sub(ecrites),
                b.absentes
            ),
            _ => MARQUEUR_NON_FIGEE.to_string(),
        };
        let params: [&dyn ToSqlValue; 3] = [&CLE_RATTRAPAGE_DATES_AJOUT_2138, &valeur, &maintenant];
        tx.execute(&marqueur, &params)?;
        Ok(())
    })?;
    // Une ligne changée entre la lecture et l'écriture (un scan concurrent)
    // n'est pas écrite : elle compte parmi les laissées.
    Ok(match issue {
        Issue::Corrigee(mut b) => {
            b.corrigees = ecrites;
            b.laissees += corrections.len().saturating_sub(ecrites);
            Issue::Corrigee(b)
        }
        autre => autre,
    })
}

/// La passe au démarrage : journalise son issue, ne bloque jamais.
pub fn rattrapage_journalise(db: &Arc<dyn DbBackend>) {
    match rattraper_les_dates_d_ajout(db) {
        Ok(Issue::DejaFaite) => {}
        Ok(Issue::NonFigee {
            pistes,
            dans_le_bloc,
        }) => tracing::info!(
            pistes,
            dans_le_bloc,
            "dates_d_ajout_2138_base_non_figee — rien à rattraper"
        ),
        Ok(Issue::Corrigee(b)) => tracing::info!(
            pistes = b.pistes,
            dans_le_bloc = b.dans_le_bloc,
            corrigees = b.corrigees,
            laissees = b.laissees,
            absentes = b.absentes,
            debut = b.debut,
            fin = b.fin,
            "dates_d_ajout_2138_premier_scan_redate_par_les_fichiers"
        ),
        Err(e) => tracing::warn!(error = %e, "dates_d_ajout_2138_rattrapage_echoue"),
    }
}

#[cfg(test)]
#[path = "rattrapage_dates_ajout_2138_tests.rs"]
mod tests;
