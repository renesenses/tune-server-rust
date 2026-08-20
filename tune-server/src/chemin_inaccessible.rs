//! Pourquoi ce dossier est-il injoignable, et que peut y faire l'utilisateur ?
//!
//! Tune sait déjà DIRE qu'un dossier est injoignable plutôt que vide (#1412,
//! livré en v0.9.63) : `browse_directory` rend `accessible: false` et la raison
//! système dans `access_error`. Il reste que cette raison est celle du noyau —
//! « Le périphérique n'est pas prêt », « The system cannot find the path
//! specified » — et qu'elle n'indique à personne quoi faire.
//!
//! Or la cause la plus fréquente sous Windows a une réparation en un geste, et
//! elle est contre-intuitive : **une lettre de lecteur réseau appartient à la
//! session Windows qui l'a créée.** Le testeur du 04/08/2026 avait configuré
//! `Z:\EDF7-FE43\EverSoloMusic` ; son EverSolo y voyait 34 169 titres, Tune
//! annonçait « Dossier vide ». Le dossier local `D:\Musique` de la même machine
//! se scannait parfaitement — ce qui achève de désigner le réseau comme
//! coupable alors que le réseau va très bien. La réparation tient en une
//! substitution : `\\EDF7-FE43\EverSoloMusic` à la place de `Z:`.
//!
//! ## Pourquoi aucun appel système ici
//!
//! `WNetGetConnectionW` résout une lettre mappée vers son chemin UNC, et on
//! pourrait vouloir l'appeler pour proposer le chemin exact. **Ce serait inutile
//! précisément dans le cas qui nous occupe** : si le processus serveur ne voit
//! pas le montage, l'API ne le voit pas non plus — c'est la même session qui
//! interroge. Elle ne réussirait que là où tout marche déjà, et échouerait
//! partout où l'on a besoin d'elle.
//!
//! Le raisonnement de forme, lui, ne dépend d'aucune session : un chemin
//! injoignable commençant par une lettre de lecteur, sous Windows, mérite cet
//! avertissement quelle que soit la raison exacte du refus. On reste donc en
//! logique pure — testable sur n'importe quelle plateforme, sans FFI, sans
//! dépendance ajoutée.

/// La lettre de lecteur qui ouvre ce chemin, s'il y en a une.
///
/// Accepte `Z:`, `Z:\`, `Z:/sous/dossier`, et la minuscule — l'utilisateur
/// recopie ce que l'explorateur lui montre, la casse varie.
pub fn lettre_de_lecteur(chemin: &str) -> Option<char> {
    let mut c = chemin.chars();
    let lettre = c.next()?;
    if !lettre.is_ascii_alphabetic() || c.next()? != ':' {
        return None;
    }
    // `Z:` seul, ou suivi d'un séparateur. `Z:toto` est un chemin relatif au
    // répertoire courant du lecteur — une curiosité MS-DOS que Tune n'accepte
    // nulle part ailleurs, et qui ne doit pas déclencher ce conseil.
    match c.next() {
        None | Some('\\') | Some('/') => Some(lettre.to_ascii_uppercase()),
        Some(_) => None,
    }
}

/// Le chemin désigne-t-il un partage réseau en notation UNC (`\\serveur\part`) ?
pub fn est_un_chemin_unc(chemin: &str) -> bool {
    chemin.starts_with("\\\\") || chemin.starts_with("//")
}

/// La clé de traduction du conseil à donner pour ce chemin injoignable, et le
/// paramètre à y substituer.
///
/// Séparé de la mise en forme pour rester testable sans table de traduction :
/// c'est le CHOIX du conseil qui porte le raisonnement, pas sa rédaction.
///
/// Ne rend rien quand aucun conseil utile ne s'applique — mieux vaut la seule
/// erreur système qu'une phrase qui envoie chercher ailleurs.
pub fn cle_du_conseil(chemin: &str, sous_windows: bool) -> Option<(&'static str, String)> {
    if sous_windows {
        if let Some(lettre) = lettre_de_lecteur(chemin) {
            return Some(("browse.hint.windowsMappedDrive", format!("{lettre}:")));
        }
    }
    if est_un_chemin_unc(chemin) {
        return Some(("browse.hint.uncUnreachable", String::new()));
    }
    None
}

/// Le conseil, traduit dans la langue de la requête. `None` si aucun ne
/// s'applique.
pub fn conseil(lang: &str, chemin: &str) -> Option<String> {
    let (cle, lecteur) = cle_du_conseil(chemin, cfg!(windows))?;
    Some(crate::i18n::t(lang, cle).replace("{drive}", &lecteur))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_lettre_de_lecteur_est_reconnue_sous_toutes_ses_formes() {
        assert_eq!(lettre_de_lecteur(r"Z:\EDF7-FE43\EverSoloMusic"), Some('Z'));
        assert_eq!(lettre_de_lecteur(r"Z:\"), Some('Z'));
        assert_eq!(lettre_de_lecteur("Z:"), Some('Z'));
        assert_eq!(lettre_de_lecteur("z:/musique"), Some('Z'));
        assert_eq!(lettre_de_lecteur("D:/Musique"), Some('D'));
    }

    #[test]
    fn ce_qui_n_est_pas_une_lettre_de_lecteur_ne_declenche_rien() {
        assert_eq!(lettre_de_lecteur("/mnt/musique"), None);
        assert_eq!(lettre_de_lecteur(r"\\NAS\musique"), None);
        assert_eq!(lettre_de_lecteur(""), None);
        assert_eq!(lettre_de_lecteur("Z"), None);
        // `Z:toto` : chemin relatif au répertoire courant du lecteur. Tune ne
        // l'accepte nulle part, et le conseil sur les lettres mappées n'aurait
        // aucun sens ici.
        assert_eq!(lettre_de_lecteur("Z:toto"), None);
        // Un schéma d'URL ressemble à une lettre de lecteur sur deux caractères.
        assert_eq!(lettre_de_lecteur("smb://NAS/musique"), None);
    }

    #[test]
    fn un_chemin_unc_est_reconnu() {
        assert!(est_un_chemin_unc(r"\\EDF7-FE43\EverSoloMusic"));
        assert!(est_un_chemin_unc("//NAS/musique"));
        assert!(!est_un_chemin_unc("/mnt/musique"));
        assert!(!est_un_chemin_unc(r"Z:\musique"));
    }

    /// Le cas exact du testeur du 04/08/2026 : `Z:\EDF7-FE43\EverSoloMusic`
    /// annoncé « Dossier vide » alors que l'EverSolo y voit 34 169 titres.
    #[test]
    fn le_cas_du_lecteur_mappe_donne_le_conseil_qui_repare() {
        let (cle, lecteur) =
            cle_du_conseil(r"Z:\EDF7-FE43\EverSoloMusic", true).expect("aucun conseil rendu");
        assert_eq!(cle, "browse.hint.windowsMappedDrive");
        assert_eq!(lecteur, "Z:", "la lettre doit être citée telle qu'affichée");
    }

    /// Hors Windows, une lettre de lecteur ne veut rien dire : conseiller le
    /// chemin UNC à quelqu'un sous Linux l'enverrait dans le mur.
    #[test]
    fn hors_windows_la_lettre_de_lecteur_ne_conseille_rien() {
        assert!(cle_du_conseil(r"Z:\musique", false).is_none());
    }

    #[test]
    fn un_partage_unc_injoignable_a_son_propre_conseil() {
        let (cle, _) = cle_du_conseil(r"\\EDF7-FE43\EverSoloMusic", true).expect("aucun conseil");
        assert_eq!(cle, "browse.hint.uncUnreachable");
        // Et il vaut sur les deux plateformes : un partage monté reste un
        // partage, la question « le NAS est-il allumé ? » ne dépend pas de l'OS.
        assert!(cle_du_conseil("//NAS/musique", false).is_some());
    }

    /// Un chemin local ordinaire n'a pas de conseil à recevoir : la seule
    /// erreur système est plus honnête qu'une piste qui n'en est pas une.
    #[test]
    fn un_chemin_local_ne_recoit_aucun_conseil() {
        assert!(cle_du_conseil("/mnt/musique", false).is_none());
        assert!(cle_du_conseil("/home/bertrand/Musique", true).is_none());
    }

    /// Les dix langues doivent porter les deux clés : une traduction manquante
    /// renverrait la clé brute à l'écran (`browse.hint.windowsMappedDrive`).
    #[test]
    fn les_conseils_sont_traduits_dans_toutes_les_langues() {
        for cle in [
            "browse.hint.windowsMappedDrive",
            "browse.hint.uncUnreachable",
        ] {
            for lang in crate::i18n::SUPPORTED {
                let texte = crate::i18n::t(lang, cle);
                assert_ne!(
                    texte, cle,
                    "`{cle}` n'est pas traduite en «{lang}» — la clé brute partirait à l'écran"
                );
            }
        }
        // Et le paramètre doit être là pour être substitué, sinon le message
        // parle d'un lecteur sans jamais le nommer.
        for lang in crate::i18n::SUPPORTED {
            assert!(
                crate::i18n::t(lang, "browse.hint.windowsMappedDrive").contains("{drive}"),
                "le message «{lang}» ne cite pas le lecteur ({{drive}} absent)"
            );
        }
    }
}
