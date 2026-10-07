//! Où vit la règle de choix des versions (#2264, décision 3 du 07/10/2026).
//!
//! La règle se range PAR PROFIL, dans les réglages du profil
//! (`settings.profile_{id}_settings`, l'objet JSON que lit et écrit
//! `GET/POST /profiles/{id}/settings`), sous la clé [`CLE_REGLE`]. Un profil
//! qui n'en a pas suit le défaut GLOBAL (`settings.versions_default_rule`, la
//! clé d'avant), et sans défaut global c'est [`RegleDeChoix::DEFAUT`].
//!
//! Une seule lecture pour tous : la route de réglage, le regroupement
//! (« Autres versions ») et la lecture ([`crate::orchestrator`]) passent par
//! [`regle_effective`]. Deux lectures qui divergeraient feraient jouer autre
//! chose que ce que l'écran annonce.

use std::sync::Arc;

use serde_json::Value;

use super::groupes_versions::RegleDeChoix;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La clé de la règle : dans `settings` pour le défaut global, et dans
/// l'objet des réglages d'un profil pour la règle de ce profil.
pub const CLE_REGLE: &str = "versions_default_rule";

/// D'où vient la règle appliquée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origine {
    /// Réglée sur le profil.
    Profil,
    /// Le défaut global, réglé.
    Global,
    /// Rien n'est réglé : [`RegleDeChoix::DEFAUT`].
    Defaut,
}

impl Origine {
    /// Le nom publié dans le contrat (`origin`).
    pub fn nom(self) -> &'static str {
        match self {
            Origine::Profil => "profile",
            Origine::Global => "setting",
            Origine::Defaut => "default",
        }
    }
}

/// La clé `settings` des réglages d'un profil — celle de
/// `GET/POST /profiles/{id}/settings`.
pub fn cle_reglages_du_profil(profile_id: i64) -> String {
    format!("profile_{profile_id}_settings")
}

fn reglages_du_profil(db: &Arc<dyn DbBackend>, profile_id: i64) -> serde_json::Map<String, Value> {
    SettingsRepo::with_backend(db.clone())
        .get(&cle_reglages_du_profil(profile_id))
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

/// La règle réglée sur CE profil, sans repli. Une valeur illisible (écrite à
/// la main) vaut « rien de réglé ».
pub fn regle_du_profil(db: &Arc<dyn DbBackend>, profile_id: i64) -> Option<RegleDeChoix> {
    reglages_du_profil(db, profile_id)
        .get(CLE_REGLE)
        .and_then(Value::as_str)
        .and_then(RegleDeChoix::depuis)
}

/// Le défaut global réglé, sans repli.
pub fn regle_globale(db: &Arc<dyn DbBackend>) -> Option<RegleDeChoix> {
    SettingsRepo::with_backend(db.clone())
        .get(CLE_REGLE)
        .ok()
        .flatten()
        .and_then(|t| RegleDeChoix::depuis(&t))
}

/// La règle qui s'applique pour `profile_id` : celle du profil, sinon le
/// défaut global, sinon [`RegleDeChoix::DEFAUT`].
pub fn regle_effective(
    db: &Arc<dyn DbBackend>,
    profile_id: Option<i64>,
) -> (RegleDeChoix, Origine) {
    if let Some(r) = profile_id.and_then(|id| regle_du_profil(db, id)) {
        return (r, Origine::Profil);
    }
    match regle_globale(db) {
        Some(r) => (r, Origine::Global),
        None => (RegleDeChoix::DEFAUT, Origine::Defaut),
    }
}

/// Règle (ou retire, avec `None`) la règle d'un profil.
///
/// Lecture puis écriture de l'objet ENTIER : `POST /profiles/{id}/settings`
/// remplace l'objet, et les autres préférences du profil doivent survivre à
/// ce réglage-ci.
pub fn poser_regle_du_profil(
    db: &Arc<dyn DbBackend>,
    profile_id: i64,
    regle: Option<&RegleDeChoix>,
) -> Result<(), String> {
    let mut reglages = reglages_du_profil(db, profile_id);
    match regle {
        Some(r) => {
            reglages.insert(CLE_REGLE.to_string(), Value::String(r.texte()));
        }
        None => {
            reglages.remove(CLE_REGLE);
        }
    }
    let texte = serde_json::to_string(&Value::Object(reglages)).map_err(|e| e.to_string())?;
    SettingsRepo::with_backend(db.clone()).set(&cle_reglages_du_profil(profile_id), &texte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    #[test]
    fn la_regle_du_profil_prime_puis_le_global_puis_le_defaut() {
        let db = base();
        assert_eq!(
            regle_effective(&db, Some(2)),
            (RegleDeChoix::PrefererLocal, Origine::Defaut)
        );
        SettingsRepo::with_backend(db.clone())
            .set(CLE_REGLE, "quality")
            .unwrap();
        assert_eq!(
            regle_effective(&db, Some(2)),
            (RegleDeChoix::MeilleureQualite, Origine::Global)
        );
        poser_regle_du_profil(&db, 2, Some(&RegleDeChoix::PrefererService("qobuz".into())))
            .unwrap();
        assert_eq!(
            regle_effective(&db, Some(2)),
            (
                RegleDeChoix::PrefererService("qobuz".into()),
                Origine::Profil
            )
        );
        // Contre-épreuve : un AUTRE profil n'est pas touché.
        assert_eq!(
            regle_effective(&db, Some(3)),
            (RegleDeChoix::MeilleureQualite, Origine::Global)
        );
        poser_regle_du_profil(&db, 2, None).unwrap();
        assert_eq!(regle_effective(&db, Some(2)).1, Origine::Global);
    }

    #[test]
    fn regler_la_regle_garde_les_autres_preferences_du_profil() {
        let db = base();
        SettingsRepo::with_backend(db.clone())
            .set(
                &cle_reglages_du_profil(4),
                r#"{"home_layout":["a","b"],"listen_later_tag":7}"#,
            )
            .unwrap();
        poser_regle_du_profil(&db, 4, Some(&RegleDeChoix::MeilleureQualite)).unwrap();
        let v: Value = serde_json::from_str(
            &SettingsRepo::with_backend(db.clone())
                .get(&cle_reglages_du_profil(4))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["home_layout"], serde_json::json!(["a", "b"]));
        assert_eq!(v["listen_later_tag"], 7);
        assert_eq!(v[CLE_REGLE], "quality");
    }

    #[test]
    fn une_valeur_illisible_vaut_rien_de_regle() {
        let db = base();
        SettingsRepo::with_backend(db.clone())
            .set(
                &cle_reglages_du_profil(5),
                r#"{"versions_default_rule":"n'importe"}"#,
            )
            .unwrap();
        assert_eq!(regle_effective(&db, Some(5)).1, Origine::Defaut);
    }
}
