//! Le profil dont le compte cloud **pilote la licence** du serveur.
//!
//! # Le défaut que ce module ferme
//!
//! Chaque connexion SSO écrivait l'état de licence du serveur —
//! [`set_account_premium`], [`set_modules`], [`set_qobuz_proxy_first`] — quel
//! que soit le compte qui se connectait. Le jeton, lui, est un réglage unique :
//! la connexion suivante écrase la précédente.
//!
//! Sur une installation partagée, la séquence est donc :
//!
//! 1. le propriétaire lie son compte premium — `mozaik_premium = true` ;
//! 2. quelqu'un d'autre lie un compte **gratuit** — `set_account_premium(false)`
//!    remet le drapeau à faux et efface l'échéance ;
//! 3. le battement de fond (`background.rs`, toutes les heures) relit le jeton
//!    **global**, c'est-à-dire celui du second compte, et **confirme** le
//!    palier gratuit à chaque passage.
//!
//! Le premium ne redescend donc pas le temps d'un rafraîchissement : il reste
//! par terre, et se relever demande au propriétaire de refaire toute la ronde
//! OAuth — jusqu'à la prochaine connexion de quelqu'un d'autre.
//!
//! Une clé de licence, elle, survit : `effective_tier` combine les deux
//! chemins. C'est donc le premium **par compte**, celui qui n'a pas de clé,
//! que ce défaut détruit.
//!
//! # La règle
//!
//! **Un seul profil pilote la licence** : celui que ce module appelle le
//! propriétaire. Les autres comptes se lient pour leur identité — nom, photo,
//! préférences — et n'écrivent rien dans l'état de licence.
//!
//! # Pourquoi un réglage dédié, et pas `is_admin`
//!
//! Deux repères plausibles ont été écartés par la mesure, le 12/09/2026 :
//!
//! - `RequireAdmin` rend `Ok` **immédiatement** quand `auth_enabled` est faux,
//!   ce qui est le cas par défaut et sur toute installation de salon. Le garde
//!   ne garderait rien.
//! - la colonne `is_admin` valait **vrai sur les deux profils** d'une
//!   installation réelle (`default` et le compte lié) : elle ne discrimine pas.
//!   Elle vient d'ailleurs du compte mozaiklabs (`CloudUser::is_admin`), où
//!   elle désigne l'équipe du site, pas le propriétaire d'un serveur.
//!
//! D'où [`CLE_PROPRIETAIRE`], posé une fois, à la première liaison.

use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;

/// Réglage portant l'identifiant du profil propriétaire.
pub const CLE_PROPRIETAIRE: &str = "owner_profile_id";

/// Réglage où la liaison SSO range le compte cloud courant.
const CLE_COMPTE_LIE: &str = "mozaik_user";

/// Ce profil pilote-t-il la licence ?
///
/// Le cœur de la règle, isolé de toute base de données pour qu'il soit
/// vérifiable directement.
///
/// **Personne de désigné ⇒ oui.** C'est délibéré, et c'est ce qui rend le
/// changement invisible sur une installation à un seul compte : le premier à
/// se lier devient propriétaire, exactement comme avant. Refuser par défaut
/// aurait laissé toute installation neuve sans aucun pilote — donc sans
/// premium, jamais.
pub fn pilote_la_licence(proprietaire: Option<i64>, profil: i64) -> bool {
    match proprietaire {
        None => true,
        Some(p) => p == profil,
    }
}

/// Le propriétaire enregistré, s'il y en a un.
///
/// Une valeur illisible ou absurde (zéro, négative, texte) est traitée comme
/// une absence : mieux vaut laisser le prochain compte revendiquer la place
/// que de bloquer la licence sur une ligne corrompue.
pub fn proprietaire(settings: &SettingsRepo) -> Option<i64> {
    settings
        .get(CLE_PROPRIETAIRE)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&id| id > 0)
}

/// Désigne `profil` propriétaire s'il n'y en a pas encore, et dit s'il pilote
/// la licence à l'issue de l'appel.
///
/// Idempotent : rappelé avec le propriétaire déjà en place, il ne réécrit rien.
/// Un profil qui n'est pas le propriétaire ne le devient jamais par ce chemin —
/// changer de propriétaire est une décision explicite, pas un effet de bord
/// d'une connexion.
pub fn revendiquer(settings: &SettingsRepo, profil: i64) -> bool {
    if profil <= 0 {
        return false;
    }
    match proprietaire(settings) {
        Some(p) => p == profil,
        None => {
            settings.set(CLE_PROPRIETAIRE, &profil.to_string()).ok();
            tracing::info!(profil, "licence_proprietaire_designe");
            true
        }
    }
}

/// L'identifiant du profil portant ce courriel.
///
/// La liaison SSO range le jeton sous le profil que le **courriel** résout, et
/// jamais sous le profil sélectionné à l'écran : sans cette distinction,
/// quelqu'un ayant choisi le profil d'un autre dans la liste et se connectant
/// avec son propre compte écraserait la session de cet autre par la sienne.
pub fn profil_du_courriel(backend: &Arc<dyn DbBackend>, courriel: &str) -> Option<i64> {
    if courriel.trim().is_empty() {
        return None;
    }
    backend
        .query_one(
            "SELECT id FROM profiles WHERE email = ?",
            &[&courriel as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
}

/// Adopte le compte **déjà lié** comme propriétaire, sur une installation qui
/// en portait un avant que ce réglage n'existe.
///
/// Sans cette reprise, le premier à se connecter après la mise à jour
/// revendiquerait la place — y compris un nouvel arrivant, pendant que le
/// propriétaire historique perdrait la main sur sa propre licence. Sans effet
/// si un propriétaire est déjà désigné ou si aucun compte n'est lié.
///
/// Rend le propriétaire en vigueur après l'appel.
pub fn adopter_le_compte_lie(settings: &SettingsRepo, backend: &Arc<dyn DbBackend>) -> Option<i64> {
    if let Some(deja) = proprietaire(settings) {
        return Some(deja);
    }
    let courriel = settings
        .get(CLE_COMPTE_LIE)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(str::to_owned))?;

    let profil = profil_du_courriel(backend, &courriel)?;
    settings.set(CLE_PROPRIETAIRE, &profil.to_string()).ok();
    tracing::info!(profil, "licence_proprietaire_adopte");
    Some(profil)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Un profil lié à un compte cloud, comme la ronde SSO en crée.
    fn profil(db: &Arc<dyn DbBackend>, courriel: &str) -> i64 {
        let zero: i64 = 0;
        db.execute(
            "INSERT INTO profiles (username, display_name, email, avatar_path, is_admin) \
             VALUES (?, ?, ?, ?, ?)",
            &[
                &courriel as &dyn ToSqlValue,
                &courriel as &dyn ToSqlValue,
                &courriel as &dyn ToSqlValue,
                &"#6366f1" as &dyn ToSqlValue,
                &zero as &dyn ToSqlValue,
            ],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    /// Installation neuve : personne n'est désigné, le premier compte lié
    /// pilote. Sans cette porte ouverte, une installation neuve n'aurait aucun
    /// pilote — donc aucun premium, jamais.
    #[test]
    fn sans_proprietaire_tout_profil_pilote() {
        assert!(pilote_la_licence(None, 1));
        assert!(pilote_la_licence(None, 7));
    }

    #[test]
    fn le_proprietaire_pilote() {
        assert!(pilote_la_licence(Some(2), 2));
    }

    /// 🔴 Le défaut lui-même : le compte du fils ne doit pas faire tomber le
    /// premium de la maison.
    #[test]
    fn un_autre_profil_ne_pilote_pas() {
        assert!(!pilote_la_licence(Some(2), 3));
        assert!(!pilote_la_licence(Some(2), 1));
    }

    /// Un profil non résolu (courriel inconnu, insertion échouée) vaut zéro
    /// chez les appelants : il ne doit jamais se retrouver à piloter une
    /// licence parce qu'il se trouve égal à un propriétaire absent.
    #[test]
    fn un_profil_nul_ne_pilote_pas_un_proprietaire_designe() {
        assert!(!pilote_la_licence(Some(2), 0));
    }

    // ─────────────────── Les parties qui touchent la base ───────────────────

    #[test]
    fn le_courriel_resout_son_profil() {
        let db = base();
        let id = profil(&db, "proprio@exemple.test");
        assert_eq!(profil_du_courriel(&db, "proprio@exemple.test"), Some(id));
    }

    /// Un courriel qu'aucun profil ne porte ne doit pas rendre un identifiant
    /// au hasard : les appelants en font « personne », et c'est ce qui empêche
    /// un compte inconnu de piloter la licence.
    #[test]
    fn un_courriel_inconnu_ne_resout_rien() {
        let db = base();
        profil(&db, "proprio@exemple.test");
        assert_eq!(profil_du_courriel(&db, "inconnu@exemple.test"), None);
        assert_eq!(profil_du_courriel(&db, ""), None);
        assert_eq!(profil_du_courriel(&db, "   "), None);
    }

    #[test]
    fn le_premier_qui_revendique_devient_proprietaire() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());
        assert_eq!(proprietaire(&s), None);
        assert!(revendiquer(&s, 2));
        assert_eq!(proprietaire(&s), Some(2));
    }

    #[test]
    fn revendiquer_est_idempotent() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());
        assert!(revendiquer(&s, 2));
        assert!(revendiquer(&s, 2));
        assert_eq!(proprietaire(&s), Some(2));
    }

    /// 🔴 LE DÉFAUT, de bout en bout. Le compte du fils se lie normalement,
    /// mais ne prend pas la main sur la licence — et ne vole pas la propriété
    /// au passage.
    #[test]
    fn un_second_compte_ne_prend_pas_la_licence() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());

        let pere = profil(&db, "pere@exemple.test");
        assert!(revendiquer(&s, pere), "le premier lié doit piloter");

        let fils = profil(&db, "fils@exemple.test");
        assert!(
            !revendiquer(&s, fils),
            "le second compte pilote la licence : c'est exactement le defaut corrige"
        );
        assert_eq!(
            proprietaire(&s),
            Some(pere),
            "le second compte a vole la propriete"
        );
    }

    /// La reprise des installations existantes : un compte était déjà lié
    /// avant que ce réglage n'existe. Sans elle, le premier à se connecter
    /// après la mise à jour — y compris un nouvel arrivant — raflerait la
    /// place pendant que le propriétaire historique perdrait sa licence.
    #[test]
    fn le_compte_deja_lie_est_adopte() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());
        let pere = profil(&db, "pere@exemple.test");
        s.set("mozaik_user", r#"{"email":"pere@exemple.test"}"#)
            .unwrap();

        assert_eq!(proprietaire(&s), None, "rien ne doit etre pose d'avance");
        assert_eq!(adopter_le_compte_lie(&s, &db), Some(pere));
        assert_eq!(proprietaire(&s), Some(pere));

        // …et le fils qui se connecte ensuite ne pilote rien.
        let fils = profil(&db, "fils@exemple.test");
        assert!(!revendiquer(&s, fils));
    }

    #[test]
    fn l_adoption_ne_deloge_pas_un_proprietaire_en_place() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());
        let pere = profil(&db, "pere@exemple.test");
        let fils = profil(&db, "fils@exemple.test");
        revendiquer(&s, pere);
        // Le jeton global porte désormais le fils — c'est l'état que produit
        // une seconde connexion. L'adoption ne doit pas s'en saisir.
        s.set("mozaik_user", r#"{"email":"fils@exemple.test"}"#)
            .unwrap();
        assert_eq!(adopter_le_compte_lie(&s, &db), Some(pere));
        assert_ne!(proprietaire(&s), Some(fils));
    }

    /// Sans compte lié, il n'y a rien à adopter — et surtout rien à inventer.
    #[test]
    fn sans_compte_lie_l_adoption_ne_pose_rien() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());
        assert_eq!(adopter_le_compte_lie(&s, &db), None);
        assert_eq!(proprietaire(&s), None);
    }

    /// Un profil non résolu vaut zéro chez l'appelant : `sso_callback` termine
    /// par `.unwrap_or(0)` quand l'insertion échoue (#3726 l'a rendue possible
    /// sur PostgreSQL natif). Zéro ne doit ni piloter, ni se faire désigner
    /// propriétaire — sans quoi une installation neuve dont la première
    /// insertion échoue se retrouverait avec un propriétaire fantôme.
    #[test]
    fn un_profil_nul_ne_revendique_rien() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());
        assert!(!revendiquer(&s, 0));
        assert!(!revendiquer(&s, -1));
        assert_eq!(proprietaire(&s), None, "un profil nul a ete designe");
    }

    /// Une ligne corrompue ne doit pas bloquer la licence pour toujours.
    #[test]
    fn un_proprietaire_illisible_vaut_absence() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());
        for brut in ["", "   ", "zero", "0", "-3"] {
            s.set(CLE_PROPRIETAIRE, brut).unwrap();
            assert_eq!(proprietaire(&s), None, "valeur brute : {brut:?}");
        }
    }
}
