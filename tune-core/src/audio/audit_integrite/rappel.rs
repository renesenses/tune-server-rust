//! Le rappel périodique de l'audit d'intégrité (#6286) : réglable en jours,
//! ÉTEINT par défaut.
//!
//! Le serveur ne relance rien de lui-même : un audit relit toute la
//! bibliothèque (ou ses fichiers changés) et c'est à l'utilisateur d'en
//! choisir le moment. Le rappel dit seulement, dans `GET /library/integrity`,
//! qu'un audit est dû : le dernier audit TERMINÉ date de plus de `jours`
//! jours, ou aucun ne l'a jamais été.
//!
//! Le réglage vit dans `settings` (aucune migration) :
//!
//! | Clé                                  | Valeur                     | Défaut      |
//! |--------------------------------------|----------------------------|-------------|
//! | `library_integrity_reminder_days`    | entier de 0 à 365, en jours | 0 : éteint |
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// L'intervalle du rappel, en jours. 0 : éteint.
pub const CLE_RAPPEL_JOURS: &str = "library_integrity_reminder_days";

/// Éteint par défaut.
pub const RAPPEL_JOURS_DEFAUT: u32 = 0;

/// Au plus un an : au-delà, un rappel n'en est plus un.
pub const RAPPEL_JOURS_MAX: u32 = 365;

/// Un entier de 0 à [`RAPPEL_JOURS_MAX`], accepté nu ou entre guillemets.
/// Toute autre forme est refusée en nommant les bornes.
pub fn valider(texte: &str) -> Result<u32, String> {
    let texte = texte.trim().trim_matches('"').trim();
    texte
        .parse::<u32>()
        .ok()
        .filter(|j| *j <= RAPPEL_JOURS_MAX)
        .ok_or_else(|| {
            format!(
                "{CLE_RAPPEL_JOURS} : un nombre entier de jours de 0 (éteint) à \
                 {RAPPEL_JOURS_MAX} est attendu, reçu {texte}"
            )
        })
}

/// L'intervalle réglé. Absent ou illisible : éteint.
pub fn rappel_jours(backend: &Arc<dyn DbBackend>) -> u32 {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_RAPPEL_JOURS)
        .ok()
        .flatten()
        .and_then(|v| valider(&v).ok())
        .unwrap_or(RAPPEL_JOURS_DEFAUT)
}

/// Ce que `GET /library/integrity` publie du rappel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rappel {
    /// L'intervalle réglé, en jours. 0 : éteint.
    pub jours: u32,
    /// Un audit est dû : rappel allumé et dernier audit terminé trop ancien,
    /// ou jamais terminé.
    pub du: bool,
    /// La date du prochain rappel (RFC 3339), quand un audit a déjà été
    /// terminé et que le rappel est allumé.
    pub prochain: Option<String>,
}

/// L'échéance du rappel, à `maintenant`, d'après la fin du dernier audit
/// TERMINÉ. Un rappel éteint n'est jamais dû.
pub fn echeance(
    jours: u32,
    dernier_audit_termine: Option<&str>,
    maintenant: DateTime<Utc>,
) -> Rappel {
    if jours == 0 {
        return Rappel {
            jours,
            du: false,
            prochain: None,
        };
    }
    let dernier = dernier_audit_termine
        .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
        .map(|d| d.with_timezone(&Utc));
    let prochain = dernier.map(|d| d + chrono::Duration::days(i64::from(jours)));
    Rappel {
        jours,
        du: prochain.is_none_or(|p| p <= maintenant),
        prochain: prochain.map(|p| p.to_rfc3339()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eteint_par_defaut_et_jamais_du() {
        let db: Arc<dyn DbBackend> =
            Arc::new(crate::db::sqlite::SqliteDb::open_in_memory().unwrap());
        db.execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT, updated_at TEXT)",
        )
        .unwrap();
        assert_eq!(
            rappel_jours(&db),
            0,
            "le rappel doit être éteint par défaut"
        );
        let r = echeance(rappel_jours(&db), None, Utc::now());
        assert!(!r.du, "un rappel éteint ne doit jamais être dû : {r:?}");
        SettingsRepo::with_backend(db.clone())
            .set(CLE_RAPPEL_JOURS, "30")
            .unwrap();
        assert_eq!(rappel_jours(&db), 30);
        SettingsRepo::with_backend(db.clone())
            .set(CLE_RAPPEL_JOURS, "jamais")
            .unwrap();
        assert_eq!(rappel_jours(&db), 0, "illisible : éteint");
    }

    #[test]
    fn du_apres_l_intervalle_ou_sans_audit_termine() {
        let maintenant = DateTime::parse_from_rfc3339("2026-10-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let r = echeance(30, Some("2026-10-01T12:00:00+00:00"), maintenant);
        assert!(!r.du, "26 jours sur 30 : pas encore dû ({r:?})");
        assert_eq!(r.prochain.as_deref(), Some("2026-10-31T12:00:00+00:00"));
        assert!(echeance(30, Some("2026-09-27T12:00:00+00:00"), maintenant).du);
        assert!(
            echeance(7, None, maintenant).du,
            "jamais terminé : dû dès que le rappel est allumé"
        );
    }

    #[test]
    fn bornes_du_reglage() {
        assert_eq!(valider("0"), Ok(0));
        assert_eq!(valider(" \"90\" "), Ok(90));
        assert_eq!(valider("365"), Ok(365));
        for mauvais in ["366", "-1", "7.5", "souvent", ""] {
            assert!(valider(mauvais).is_err(), "{mauvais}");
        }
    }
}
