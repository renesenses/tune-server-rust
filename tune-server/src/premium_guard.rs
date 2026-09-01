use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tracing::info;

use tune_core::license::{Feature, LicenseManager};

/// Où acheter un droit, et où lier son compte. Une seule définition : le refus
/// d'un module réutilise l'adresse que `require_premium` sert déjà.
const UPGRADE_URL: &str = "https://mozaiklabs.fr/pricing";

/// Check that a premium feature is enabled.  Returns `Ok(())` when the
/// feature is available, or an `Err(Response)` with HTTP 402 and a
/// structured JSON body when it is not.
pub async fn require_premium(license: &LicenseManager, feature: Feature) -> Result<(), Response> {
    if license.check_feature(feature).await {
        Ok(())
    } else {
        info!(feature = feature.display_name(), "premium_feature_blocked");
        Err((
            StatusCode::PAYMENT_REQUIRED,
            axum::Json(json!({
                "error": "premium_required",
                "feature": feature.display_name(),
                "message": format!("{} requires Tune Premium", feature.display_name()),
                "upgrade_url": UPGRADE_URL
            })),
        )
            .into_response())
    }
}

/// Pourquoi un **module payant** (SKU séparé, ex. « diretta ») est indisponible.
///
/// Les modules ne passent pas par [`Feature`] : ils ne sont pas des options du
/// palier premium mais des achats distincts, et leur droit voyage
/// **uniquement avec le compte lié** — jamais avec la clé de licence. D'où les
/// deux raisons, qui appellent deux gestes opposés de la part de
/// l'utilisateur : lier son compte, ou acheter le module.
///
/// Cette distinction est tout l'objet de #2392. Un bêta-testeur du module
/// Diretta a réinstallé Fedora, changé de système de fichiers et recompilé
/// trente minutes durant, parce que le serveur ne disait **rien** : son droit
/// était valide depuis sept jours, il lui manquait une connexion de compte.
/// Le refus n'existait nulle part — ni en journal au-dessus de `debug`, ni
/// dans une réponse d'API. Le nommer est le correctif.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleRefusal {
    /// Aucun compte lié : le droit ne peut pas parvenir au serveur, même
    /// acheté et même avec une clé premium valide saisie.
    AccountNotLinked,
    /// Compte lié, mais ce module-ci n'est pas possédé.
    NotOwned,
}

impl ModuleRefusal {
    /// La raison du refus, ou `None` si le module est bien possédé.
    ///
    /// `account_linked` = un jeton de compte (`mozaik_access_token`) est
    /// stocké. Il départage les deux seuls écrans aujourd'hui identiques.
    pub fn evaluate(module_owned: bool, account_linked: bool) -> Option<Self> {
        match (module_owned, account_linked) {
            (true, _) => None,
            // L'ordre compte : sans compte lié, on ne SAIT pas si le module est
            // possédé — la liste est vide parce que personne n'a pu la lire, pas
            // parce que l'achat manque. Annoncer « non possédé » à quelqu'un qui
            // a payé, c'est le renvoyer acheter deux fois.
            (false, false) => Some(Self::AccountNotLinked),
            (false, true) => Some(Self::NotOwned),
        }
    }

    /// Le **code** stable, seul terme du contrat avec le client.
    ///
    /// Piège relevé sur #2419 : `require_premium` compose son `message` en
    /// anglais (`"… requires Tune Premium"`) et l'interface l'affiche tel quel
    /// dans un écran traduit. On ne le reproduit pas ici — le client traduit
    /// ce code, et ne lit `message` que pour un journal.
    pub fn code(self) -> &'static str {
        match self {
            Self::AccountNotLinked => "module_account_not_linked",
            Self::NotOwned => "module_not_owned",
        }
    }

    /// Le geste attendu de l'utilisateur — l'« actionnable » du refus.
    pub fn action(self) -> &'static str {
        match self {
            Self::AccountNotLinked => "link_account",
            Self::NotOwned => "purchase_module",
        }
    }

    /// Repli anglais, pour le journal et pour un client qui ne connaîtrait pas
    /// encore le code. **Jamais** destiné à être affiché tel quel.
    fn message(self, module: &str) -> String {
        match self {
            Self::AccountNotLinked => format!(
                "the {module} module is a paid add-on: link your Mozaiklabs account so the server can receive the entitlement"
            ),
            Self::NotOwned => {
                format!("the {module} module is a paid add-on and this account does not own it")
            }
        }
    }

    /// Le refus, dans la **même forme** que celui de [`require_premium`] :
    /// `error` + `message` + `upgrade_url`, plus le `code` et l'`action` qui
    /// portent le sens. Une seule famille de refus premium, pas deux.
    pub fn to_json(self, module: &str) -> Value {
        json!({
            "error": "module_required",
            "code": self.code(),
            "module": module,
            "action": self.action(),
            "message": self.message(module),
            "upgrade_url": UPGRADE_URL,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2392, le cas vécu : le droit est acheté et valide côté compte, mais
    /// aucun compte n'est lié au serveur — donc `licensed_modules` reste vide.
    /// Le serveur doit NOMMER cette raison. C'est elle qui manquait : le
    /// testeur a réinstallé son système entier faute de la voir.
    #[test]
    fn un_module_paye_sans_compte_lie_nomme_sa_raison() {
        let refus = ModuleRefusal::evaluate(false, false);
        assert_eq!(
            refus,
            Some(ModuleRefusal::AccountNotLinked),
            "un module non possede et aucun compte lie doit produire une raison nommee, pas le silence"
        );
        assert_eq!(refus.unwrap().code(), "module_account_not_linked");
        assert_eq!(refus.unwrap().action(), "link_account");
    }

    /// L'autre moitié : compte bien lié, mais ce module-ci n'est pas acheté.
    /// Même écran aujourd'hui, geste opposé — ce sont deux codes distincts.
    #[test]
    fn un_module_non_possede_avec_compte_lie_dit_autre_chose() {
        let refus = ModuleRefusal::evaluate(false, true);
        assert_eq!(refus, Some(ModuleRefusal::NotOwned));
        assert_eq!(refus.unwrap().code(), "module_not_owned");
        assert_eq!(refus.unwrap().action(), "purchase_module");
        assert_ne!(
            ModuleRefusal::AccountNotLinked.code(),
            ModuleRefusal::NotOwned.code(),
            "les deux causes doivent rester distinguables par le client"
        );
    }

    /// Et un module réellement possédé ne refuse rien, compte lié ou non.
    #[test]
    fn un_module_possede_ne_refuse_rien() {
        assert_eq!(ModuleRefusal::evaluate(true, true), None);
        assert_eq!(ModuleRefusal::evaluate(true, false), None);
    }

    /// Le piège de #2419 : le serveur y composait sa phrase en anglais et
    /// l'interface traduite l'affichait telle quelle. Le contrat porte donc un
    /// CODE, et il suit la forme de `require_premium` (error / upgrade_url)
    /// pour que le client n'ait pas deux familles de refus à connaître.
    #[test]
    fn le_refus_porte_un_code_stable_et_pas_seulement_une_phrase_anglaise() {
        let body = ModuleRefusal::AccountNotLinked.to_json("diretta");
        assert_eq!(body["error"], "module_required");
        assert_eq!(body["code"], "module_account_not_linked");
        assert_eq!(body["module"], "diretta");
        assert_eq!(body["action"], "link_account");
        assert_eq!(body["upgrade_url"], UPGRADE_URL);
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.contains("diretta")),
            "le repli journal doit nommer le module: {body}"
        );
    }
}
