//! La version de l'INTERFACE, à côté de celle du serveur (#3380).
//!
//! ## Le fait mesuré le 05/09/2026
//!
//! Quand un testeur décrit un bogue d'écran, on connaît la version de son
//! **serveur**. On ne connaît pas celle de son **interface** — et les deux
//! divergent déjà : `web/` est déployé SÉPARÉMENT du binaire. Cas consigné, un
//! correctif fusionné sur `release/v0.9` a disparu du .18 du jour au lendemain,
//! écrasé à 06:09 par un déploiement basé sur `main`.
//!
//! Zéro occurrence de `ui_version` ou `web_version` dans `tune-server/src` et
//! `tune-core/src` : le serveur ignorait la version de l'interface qu'il sert.
//! Elle existe pourtant côté client — `package.json` la porte et `vite.config.ts`
//! la compile dans le paquet — mais personne ne la remonte, et sur le .18 elle
//! n'est lisible que noyée dans un `assets/index-<hash>.js` minifié.
//!
//! Conséquence concrète : un rapport d'écran envoyé avec une version serveur
//! récente peut venir d'une interface en retard de plusieurs versions. On
//! cherche alors dans du code qui n'est pas celui qui tourne.
//!
//! ## Ce que ce module fait, et ce qu'il ne fait pas
//!
//! Il LIT `<web_dir>/version.json`, le fichier que le build de
//! `tune-web-client` dépose à la racine de son `dist/`. Il ne l'écrit pas, et
//! il ne devine rien.
//!
//! 🔴 **Aucun repli sur `crate::version()`.** Un serveur qui, faute de fichier,
//! afficherait sa propre version comme version d'interface dirait exactement le
//! mensonge que ce ticket existe pour empêcher : deux numéros identiques, et
//! l'écart invisible. Quand le fichier manque, la réponse est `None` — « pas
//! établi », qui est une réponse acceptable.
//!
//! ## Pourquoi la règle vit ici et pas dans la route
//!
//! Elle est éprouvable sans monter un serveur ni servir un fichier : un
//! répertoire jetable, un `version.json`, et la porte de sortie du ticket — une
//! instance dont le `web/` est volontairement décalé doit rendre DEUX numéros
//! différents — se mesure pour de vrai. Un témoin qui ne vérifierait que le cas
//! aligné ne garderait rien.

use std::path::Path;

/// Le fichier que le build web dépose à la racine de son `dist/`.
pub const FICHIER_VERSION: &str = "version.json";

/// Extrait la version d'un `web/version.json`.
///
/// PURE : ni disque, ni réseau. La forme attendue est celle que `package.json`
/// porte déjà — `{"version":"0.9.141"}` — et tout le reste du document est
/// ignoré, pour qu'un champ ajouté plus tard côté client ne casse rien ici.
///
/// Rend `None` sur un document illisible, sans champ `version`, ou dont la
/// version est vide : mieux vaut ne rien dire que dire une chaîne vide qu'un
/// écran afficherait comme un numéro.
pub fn version_depuis_json(contenu: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(contenu).ok()?;
    let brut = doc.get("version")?.as_str()?.trim();
    if brut.is_empty() {
        None
    } else {
        Some(brut.to_string())
    }
}

/// Lit `<web_dir>/version.json`.
///
/// `None` quand le fichier est absent ou illisible — jamais la version du
/// serveur, voir l'en-tête de ce module.
pub fn version_interface(web_dir: &Path) -> Option<String> {
    let contenu = std::fs::read_to_string(web_dir.join(FICHIER_VERSION)).ok()?;
    version_depuis_json(&contenu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_forme_du_build_web_est_lue() {
        assert_eq!(
            version_depuis_json(r#"{"version":"0.9.141"}"#),
            Some("0.9.141".to_string())
        );
    }

    /// Le build peut y ajouter ce qu'il veut : seul `version` est lu.
    #[test]
    fn les_champs_en_trop_sont_ignores() {
        assert_eq!(
            version_depuis_json(r#"{"name":"tune-web-client","version":" 0.9.140 ","built":1}"#),
            Some("0.9.140".to_string())
        );
    }

    #[test]
    fn un_document_sans_version_ne_dit_rien() {
        assert_eq!(version_depuis_json(r#"{"name":"tune"}"#), None);
        assert_eq!(version_depuis_json(r#"{"version":""}"#), None);
        assert_eq!(version_depuis_json(r#"{"version":42}"#), None);
        assert_eq!(version_depuis_json("pas du json"), None);
        assert_eq!(version_depuis_json(""), None);
    }

    /// 🔴 LA PORTE DE SORTIE DU TICKET. Une instance dont le `web/` est
    /// volontairement décalé doit rendre DEUX numéros différents. Un témoin qui
    /// ne vérifierait que le cas aligné ne garderait rien : il resterait vert
    /// devant un serveur qui recopie sa propre version.
    #[test]
    fn un_web_decale_rend_un_numero_different_de_celui_du_serveur() {
        let dir = tempfile::tempdir().unwrap();
        // Volontairement décalée : ce n'est PAS `crate::version()`.
        std::fs::write(
            dir.path().join(FICHIER_VERSION),
            r#"{"version":"0.0.1-web"}"#,
        )
        .unwrap();

        let interface = version_interface(dir.path()).expect("version d'interface lisible");
        assert_eq!(interface, "0.0.1-web");
        assert_ne!(
            interface,
            crate::version(),
            "le serveur recopie sa propre version au lieu de lire celle de \
             l'interface : l'écart resterait invisible, ce qui est exactement \
             le défaut de #3380"
        );
    }

    /// LA CONTRE-ÉPREUVE. Pas de fichier ⇒ pas de numéro. Surtout pas celui du
    /// serveur : un repli le ferait passer pour la version de l'écran.
    #[test]
    fn sans_fichier_la_version_d_interface_n_est_pas_celle_du_serveur() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            version_interface(dir.path()),
            None,
            "un répertoire web sans version.json ne doit rien affirmer"
        );
        assert_ne!(
            version_interface(dir.path()),
            Some(crate::version().to_string()),
            "repli interdit : ce serait deux numéros identiques et l'écart perdu"
        );
    }
}
