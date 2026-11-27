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
//!
//! # Changer de compte relié
//!
//! La déliaison efface l'état de licence mais garde le propriétaire. Sans
//! autre règle, le compte relié ensuite ne pouvait plus jamais écrire les
//! modules payants : son profil n'était pas le propriétaire. La propriété
//! suit donc le nouveau compte dans deux cas seulement, tranchés par
//! [`le_proprietaire_suit`] : la session authentifiée du propriétaire, ou une
//! liaison faite par qui peut délier (administrateur, cette machine) alors
//! qu'aucun compte n'est relié.

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
/// d'une connexion : voir [`suivre_le_compte_relie`].
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

/// Qui relie le compte du serveur, vu par la règle de transfert
/// ([`le_proprietaire_suit`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Relieur {
    /// Le profil d'une session AUTHENTIFIÉE (authentification active). Sans
    /// authentification, une session ne prouve rien — le profil administrateur
    /// d'origine (`admin`, `default` avant la rc14) s'y ouvre sans mot de passe — et ce champ reste vide.
    pub session_authentifiee: Option<i64>,
    /// L'appelant a-t-il aussi le droit de DÉLIER le compte du serveur
    /// (administrateur, ou cette machine sans authentification) ? Jamais vrai
    /// pour une ronde lancée ou finie depuis le réseau local.
    pub peut_delier: bool,
    /// Un compte était-il relié au serveur au moment de la liaison (jeton ou
    /// identité en base, « reconnexion requise » comprise) ?
    pub compte_relie: bool,
}

/// Le propriétaire doit-il passer au profil `nouveau`, celui du compte que
/// l'on vient de relier ? Le cœur de la règle, sans base de données.
///
/// Deux cas, et deux seulement :
///
/// 1. **la session authentifiée est celle du propriétaire** : il change
///    lui-même de compte, la propriété le suit ;
/// 2. **aucun compte n'est relié, et l'appelant est de ceux qui délient**
///    (administrateur, ou cette machine) : le compte précédent a été délié,
///    ce qui a déjà effacé l'état de licence (`clear_account_premium`). Il
///    n'y a donc plus rien à prendre à personne, et la place se libère pour
///    le compte suivant.
///
/// Tout le reste garde le propriétaire en place : un autre profil qui relie
/// son compte pendant que celui du propriétaire est relié, ou n'importe quelle
/// liaison venue du réseau local (qui ne peut pas délier).
pub fn le_proprietaire_suit(proprietaire: Option<i64>, nouveau: i64, relieur: Relieur) -> bool {
    if nouveau <= 0 {
        return false;
    }
    let Some(p) = proprietaire else {
        // Personne de désigné : `revendiquer` s'en charge.
        return false;
    };
    if p == nouveau {
        return false;
    }
    relieur.session_authentifiee == Some(p) || (relieur.peut_delier && !relieur.compte_relie)
}

/// Applique [`le_proprietaire_suit`] : désigne `nouveau` propriétaire si la
/// règle le permet. Rend vrai si la propriété a changé de main. À appeler
/// AVANT [`revendiquer`], qui dira ensuite si `nouveau` pilote la licence.
pub fn suivre_le_compte_relie(settings: &SettingsRepo, nouveau: i64, relieur: Relieur) -> bool {
    let ancien = proprietaire(settings);
    if !le_proprietaire_suit(ancien, nouveau, relieur) {
        return false;
    }
    if settings
        .set(CLE_PROPRIETAIRE, &nouveau.to_string())
        .is_err()
    {
        return false;
    }
    tracing::info!(
        ancien = ancien.unwrap_or(0),
        nouveau,
        "licence_proprietaire_suit_le_compte_relie"
    );
    true
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

/// Le profil auquel rattacher une première liaison SSO (#7082).
///
/// Sans correspondance de courriel, la liaison créait un NOUVEAU profil et la
/// session basculait dessus : sur une installation à un seul profil (le
/// `default`, sans courriel), le testeur perdait photo et thème d'un coup — ils
/// restaient sur le profil 1, que plus rien ne sélectionnait.
///
/// Règle : s'il n'existe qu'UN profil et qu'il n'a pas de courriel, le compte
/// cloud s'y rattache (le courriel y est enregistré). Avec plusieurs profils,
/// rien n'est deviné : `None`, et l'appelant crée un profil comme avant. Ne
/// touche ni au propriétaire de licence ni au nom, ni à la couleur d'avatar.
pub fn rattacher_au_profil_unique(backend: &Arc<dyn DbBackend>, courriel: &str) -> Option<i64> {
    if courriel.trim().is_empty() {
        return None;
    }
    let nombre = backend
        .query_one("SELECT COUNT(*) FROM profiles", &[])
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))?;
    if nombre != 1 {
        return None;
    }
    let id = backend
        .query_one(
            "SELECT id FROM profiles WHERE email IS NULL OR TRIM(email) = ''",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))?;
    backend
        .execute(
            "UPDATE profiles SET email = ? WHERE id = ?",
            &[&courriel as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        )
        .ok()?;
    tracing::info!(profil = id, "sso_rattache_au_profil_unique");
    Some(id)
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
    // ─────────────────── Changer de compte relié ───────────────────

    fn relieur(session: Option<i64>, peut_delier: bool, compte_relie: bool) -> Relieur {
        Relieur {
            session_authentifiee: session,
            peut_delier,
            compte_relie,
        }
    }

    /// 🔴 Le défaut : le compte précédent a été délié, cette machine relie un
    /// autre compte. Sans transfert, ce compte n'écrit jamais les modules.
    #[test]
    fn apres_deliaison_cette_machine_fait_suivre_la_propriete() {
        assert!(le_proprietaire_suit(Some(1), 2, relieur(None, true, false)));
    }

    /// Le propriétaire authentifié change lui-même de compte.
    #[test]
    fn le_proprietaire_authentifie_fait_suivre_la_propriete() {
        assert!(le_proprietaire_suit(
            Some(1),
            2,
            relieur(Some(1), true, true)
        ));
    }

    /// La garde d'origine : un compte est relié et l'appelant n'est pas le
    /// propriétaire authentifié — la propriété ne bouge pas.
    #[test]
    fn un_autre_profil_ne_s_empare_pas_du_serveur() {
        assert!(!le_proprietaire_suit(
            Some(1),
            2,
            relieur(Some(3), true, true)
        ));
        assert!(!le_proprietaire_suit(Some(1), 2, relieur(None, true, true)));
    }

    /// Le réseau local ne délie pas : il ne fait jamais suivre, même sans
    /// compte relié.
    #[test]
    fn le_reseau_local_ne_fait_jamais_suivre() {
        assert!(!le_proprietaire_suit(
            Some(1),
            2,
            relieur(None, false, false)
        ));
    }

    /// Ni profil nul, ni propriétaire absent (c'est `revendiquer`), ni
    /// réécriture du même.
    #[test]
    fn les_cas_sans_objet_ne_font_rien() {
        let libre = relieur(None, true, false);
        assert!(!le_proprietaire_suit(Some(1), 0, libre));
        assert!(!le_proprietaire_suit(Some(1), -1, libre));
        assert!(!le_proprietaire_suit(None, 2, libre));
        assert!(!le_proprietaire_suit(Some(2), 2, libre));
    }

    #[test]
    fn suivre_le_compte_relie_ecrit_le_nouveau_proprietaire() {
        let db = base();
        let s = SettingsRepo::with_backend(db.clone());
        let a = profil(&db, "a@exemple.test");
        let b = profil(&db, "b@exemple.test");
        assert!(revendiquer(&s, a));
        assert!(!suivre_le_compte_relie(&s, b, relieur(None, true, true)));
        assert_eq!(proprietaire(&s), Some(a));
        assert!(suivre_le_compte_relie(&s, b, relieur(None, true, false)));
        assert_eq!(proprietaire(&s), Some(b));
        assert!(revendiquer(&s, b), "le nouveau proprietaire pilote");
        assert!(!revendiquer(&s, a), "l'ancien ne pilote plus");
    }

    /// #7082 : un seul profil, sans courriel -> la liaison s'y rattache.
    #[test]
    fn profil_unique_sans_courriel_est_rattache() {
        let db = base();
        let defaut: i64 = db
            .query_one("SELECT id FROM profiles ORDER BY id LIMIT 1", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .expect("profil par defaut");
        assert_eq!(rattacher_au_profil_unique(&db, "a@b.fr"), Some(defaut));
        assert_eq!(profil_du_courriel(&db, "a@b.fr"), Some(defaut));
        let n = db
            .query_one("SELECT COUNT(*) FROM profiles", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()));
        assert_eq!(n, Some(1), "aucun nouveau profil");
    }

    /// Deux profils : on ne devine pas, comportement d'avant.
    #[test]
    fn deux_profils_ne_rattachent_rien() {
        let db = base();
        profil(&db, "autre@b.fr");
        assert_eq!(rattacher_au_profil_unique(&db, "a@b.fr"), None);
        assert_eq!(profil_du_courriel(&db, "a@b.fr"), None);
    }

    /// Le profil unique porte deja un courriel : un autre compte n'y entre pas.
    #[test]
    fn profil_unique_deja_lie_n_est_pas_rattache() {
        let db = base();
        db.execute("UPDATE profiles SET email = 'x@b.fr'", &[])
            .unwrap();
        assert_eq!(rattacher_au_profil_unique(&db, "a@b.fr"), None);
    }
}
