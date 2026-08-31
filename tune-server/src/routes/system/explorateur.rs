//! Périmètre de l'explorateur de dossiers servi par le serveur (#1275).
//!
//! Le sélecteur de dossiers des réglages Bibliothèque ne peut pas être un
//! dialogue natif : ils sont proscrits dans les webviews de ce projet. Le
//! serveur expose donc l'arborescence et le client la dessine — ce qui met une
//! route de LECTURE DU SYSTÈME DE FICHIERS de la machine serveur à portée du
//! réseau local. `GET /system/browse-dirs` existe depuis longtemps et n'avait
//! aucune borne : elle partait de `/` (ou `C:\`), acceptait n'importe quel
//! `?path=`, et sondait chaque enfant. Sans rôle exigé, sur une installation
//! par défaut (`auth_enabled` absent ⇒ serveur ouvert « LAN de confiance »),
//! n'importe qui sur le réseau pouvait énumérer `/etc`, `/root`,
//! `C:\Users\…`, et cartographier la machine.
//!
//! Ce module porte les trois refus. Ils sont volontairement en **logique
//! pure** : testables sur n'importe quelle plateforme, donc un test ne peut
//! pas rester vert sur le CI Linux pendant que Windows perd la garde — c'est
//! exactement ainsi que #2016 a livré quatre fois le même défaut.
//!
//! Le périmètre retenu est **le disque MOINS les arbres système**, et non une
//! liste blanche de racines. Justification : le geste que #1275 doit rendre
//! possible est de désigner une racine de bibliothèque qui n'existe pas encore
//! dans les réglages — un NAS monté sur `/nas`, une clé sur `/media/…`, un
//! `D:\Musique`, un `/opt/musique`. Une liste blanche refuserait ces
//! dispositions légitimes et renverrait l'utilisateur à la saisie manuelle,
//! c'est-à-dire au défaut que l'issue demande de supprimer. La route
//! d'écriture qu'il alimente (`POST /system/music-dirs`) accepte déjà, elle,
//! n'importe quel chemin existant.

use tune_core::metadata::enrich_scope::sous_le_dossier;

/// Arbres système absolus : jamais un endroit où vit de la musique, toujours
/// un endroit où la reconnaissance d'une machine commence.
///
/// `/run` et `/media` en sont volontairement ABSENTS : c'est là que Linux
/// monte les disques amovibles (`/run/media/<user>/<clé>`), donc là que vit
/// souvent la bibliothèque qu'on vient désigner.
pub(crate) const ARBRES_SYSTEME: &[&str] = &[
    // Linux / BSD
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/proc",
    "/root",
    "/sbin",
    "/usr",
    "/sys",
    "/var",
    // macOS — `/etc` et `/var` y sont des liens vers `/private/…`, que la
    // forme canonique fait apparaître.
    "/System",
    "/Library",
    "/cores",
    "/private/etc",
    "/private/var",
];

/// Dossiers système de Windows, nommés RELATIVEMENT à la racine de leur
/// lecteur : ils existent sur `C:` comme sur `D:`.
pub(crate) const DOSSIERS_SYSTEME_WINDOWS: &[&str] = &[
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$Recycle.Bin",
    "$WinREAgent",
    "System Volume Information",
    "Recovery",
    "PerfLogs",
    "Config.Msi",
];

/// Motif du refus opposé à un chemin demandé. Le client n'en reçoit que le
/// libellé : jamais l'erreur système, jamais de quoi distinguer « ce dossier
/// existe mais je le refuse » de « ce dossier n'existe pas » — cette
/// différence-là est déjà un oracle de reconnaissance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refus {
    /// Chemin relatif, ou vide : l'explorateur ne travaille qu'en absolu.
    PasAbsolu,
    /// Le chemin contient un `..`. Il n'est jamais nécessaire — le client
    /// remonte par le champ `parent` de la réponse — et il est le seul moyen
    /// de faire mentir une garde de préfixe.
    RemonteVersLeParent,
    /// Le chemin tombe dans un arbre système.
    ArbreSysteme,
}

impl Refus {
    pub(crate) fn libelle(self) -> &'static str {
        match self {
            Refus::PasAbsolu => "path must be absolute",
            Refus::RemonteVersLeParent => "path must not contain '..'",
            Refus::ArbreSysteme => "path is outside the browsable perimeter",
        }
    }
}

/// Le chemin est-il écrit sous l'une des trois formes absolues ?
///
/// `Path::is_absolute()` ne convient pas : sur l'hôte Linux du CI il déclare
/// `D:\Musique` RELATIF, la garde refuserait donc tout chemin Windows tout en
/// restant verte. Le même angle mort a coûté #1837 et #2056.
pub(crate) fn est_absolu(chemin: &str) -> bool {
    chemin.starts_with('/')
        || crate::chemin_inaccessible::est_un_chemin_unc(chemin)
        || crate::chemin_inaccessible::lettre_de_lecteur(chemin).is_some()
}

/// Un segment `..` quelque part dans le chemin ?
///
/// Le découpage prend les DEUX séparateurs. `Path::components()` ne suffirait
/// pas : sur un hôte POSIX, `D:\Musique\..\..\etc` est UN seul composant
/// `Normal`, le `..` y est invisible, et la garde passerait.
pub(crate) fn remonte_vers_le_parent(chemin: &str) -> bool {
    chemin.split(['/', '\\']).any(|segment| segment == "..")
}

/// Le premier composant après une racine de lecteur Windows (`C:\Windows` ⇒
/// `Windows`). `None` pour tout ce qui n'est pas `X:` — un chemin POSIX, un
/// partage UNC dont le premier composant est un nom de partage, pas un
/// dossier système.
fn dossier_de_tete_windows(chemin: &str) -> Option<&str> {
    crate::chemin_inaccessible::lettre_de_lecteur(chemin)?;
    let reste = chemin[2..].trim_start_matches(['/', '\\']);
    let fin = reste.find(['/', '\\']).unwrap_or(reste.len());
    let tete = &reste[..fin];
    (!tete.is_empty()).then_some(tete)
}

/// Le chemin tombe-t-il dans un arbre système ?
///
/// Les deux jeux de règles sont évalués sur TOUTES les plateformes. Les
/// enfermer derrière un `cfg` rendrait la règle Windows intestable ailleurs
/// que sous Windows, où personne ne la fait tourner.
pub(crate) fn dans_un_arbre_systeme(chemin: &str) -> bool {
    if ARBRES_SYSTEME
        .iter()
        .any(|systeme| sous_le_dossier(chemin, systeme))
    {
        return true;
    }
    // La casse ne distingue rien sous Windows : `C:\WINDOWS` est `C:\Windows`.
    dossier_de_tete_windows(chemin).is_some_and(|tete| {
        DOSSIERS_SYSTEME_WINDOWS
            .iter()
            .any(|systeme| systeme.eq_ignore_ascii_case(tete))
    })
}

/// Forme canonique d'un chemin, débarrassée du préfixe « verbatim » que
/// Windows ajoute (`\\?\C:\…`) et que les règles ci-dessus ne reconnaîtraient
/// pas.
///
/// Sert à voir la CIBLE d'un lien symbolique : sans elle, un lien
/// `~/Musique/sys → /sys` posé dans une racine de bibliothèque ouvrirait
/// l'arbre système par un chemin dont le texte, lui, est irréprochable.
pub(crate) fn forme_canonique(chemin: &std::path::Path) -> Option<String> {
    let canonique = std::fs::canonicalize(chemin).ok()?;
    let texte = canonique.to_string_lossy().into_owned();
    if let Some(reste) = texte.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{reste}"));
    }
    if let Some(reste) = texte.strip_prefix(r"\\?\") {
        return Some(reste.to_string());
    }
    Some(texte)
}

/// La garde complète appliquée au chemin DEMANDÉ, avant toute lecture disque.
pub(crate) fn verifier_le_chemin_demande(chemin: &str) -> Result<(), Refus> {
    if !est_absolu(chemin) {
        return Err(Refus::PasAbsolu);
    }
    if remonte_vers_le_parent(chemin) {
        return Err(Refus::RemonteVersLeParent);
    }
    if dans_un_arbre_systeme(chemin) {
        return Err(Refus::ArbreSysteme);
    }
    Ok(())
}

/// Ce chemin, une fois résolu sur le disque, reste-t-il hors des arbres
/// système ? Complète [`verifier_le_chemin_demande`] pour le cas du lien
/// symbolique, où le texte et la cible divergent.
pub(crate) fn la_cible_reste_dans_le_perimetre(chemin: &std::path::Path) -> bool {
    match forme_canonique(chemin) {
        // Un chemin illisible ne prouve rien : c'est la lecture qui tranchera,
        // et elle rendra sa propre erreur.
        None => true,
        Some(canonique) => !dans_un_arbre_systeme(&canonique),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Les REFUS. Chacun tombe si l'on retire la garde correspondante :
    // c'est la contre-épreuve, et elle vaut plus que le chemin nominal.
    // -----------------------------------------------------------------

    /// Le `..` est refusé quel que soit le séparateur qui l'entoure. Le piège
    /// Windows a déjà mordu quatre fois sur ce dépôt (#2016) : une garde
    /// écrite avec le seul `/` laisse passer toute la forme antislash.
    #[test]
    fn un_retour_arriere_est_refuse_avec_les_deux_separateurs() {
        for chemin in [
            "/home/bertrand/../../etc",
            "/home/bertrand/..",
            "../etc/passwd",
            r"D:\Musique\..\..\Windows",
            r"D:\Musique\..",
            "D:/Musique/../Windows",
            r"\\NAS\Musique\..\..\etc",
        ] {
            assert!(
                remonte_vers_le_parent(chemin),
                "un « .. » non vu : {chemin:?}"
            );
        }
    }

    /// Contre-épreuve du séparateur : la formule naïve — celle que ce dépôt a
    /// écrite quatre fois — ne voit AUCUN des `..` de la forme Windows. Ce
    /// test échoue si quelqu'un revient à `split('/')`.
    #[test]
    fn l_ancien_decoupage_sur_le_seul_slash_ratait_la_forme_windows() {
        let naif = |chemin: &str| chemin.split('/').any(|s| s == "..");
        assert!(!naif(r"D:\Musique\..\..\Windows"));
        assert!(remonte_vers_le_parent(r"D:\Musique\..\..\Windows"));
    }

    /// Un chemin sain ne doit pas être pris pour une remontée : `..` est un
    /// SEGMENT, pas une sous-chaîne. Un dossier nommé « ..soundtracks »
    /// existe, et refuser l'accès à la bibliothèque serait une régression.
    #[test]
    fn un_nom_qui_commence_par_deux_points_n_est_pas_une_remontee() {
        assert!(!remonte_vers_le_parent("/mnt/musique/..soundtracks"));
        assert!(!remonte_vers_le_parent(r"D:\Musique\..bootlegs"));
    }

    /// Les arbres système sont refusés, dans les deux jeux de règles, depuis
    /// n'importe quel hôte.
    #[test]
    fn les_arbres_systeme_sont_refuses() {
        for chemin in [
            "/etc",
            "/etc/ssh",
            "/proc/1/environ",
            "/root",
            "/sys/class",
            "/var/lib",
            "/private/var/db",
            "/System/Library",
            r"C:\Windows",
            r"C:\Windows\System32\config",
            "C:/Windows/System32",
            r"c:\windows\system32",
            r"D:\Program Files (x86)",
            r"E:\$Recycle.Bin",
            r"C:\ProgramData\Tune",
        ] {
            assert!(dans_un_arbre_systeme(chemin), "laissé passer : {chemin:?}");
        }
    }

    /// Frontière de séparateur, cinquième occurrence de #2016 évitée : un
    /// dossier dont le NOM commence par celui d'un arbre système n'est pas
    /// dans cet arbre. `/etcetera` et `/vars` sont des dossiers ordinaires.
    #[test]
    fn un_prefixe_de_nom_n_est_pas_un_arbre_systeme() {
        for chemin in [
            "/etcetera/musique",
            "/vars",
            "/usrlocal/musique",
            "/systeme",
            r"C:\Windows Media",
            r"C:\ProgramData2",
        ] {
            assert!(
                !dans_un_arbre_systeme(chemin),
                "refusé à tort : {chemin:?} — préfixe de nom pris pour containment"
            );
        }
    }

    /// Ce que l'explorateur doit continuer à ouvrir. Sans ce test, resserrer
    /// le périmètre « pour être tranquille » casserait le geste que #1275
    /// demande, sans que rien ne le dise.
    #[test]
    fn les_endroits_ou_vit_la_musique_restent_ouverts() {
        for chemin in [
            "/",
            "/home/bertrand/Musique",
            "/Users/bertrand/Music",
            "/Volumes/Musique",
            "/mnt/nas/Jazz",
            "/media/bertrand/USB",
            "/run/media/bertrand/USB",
            "/srv/musique",
            "/opt/musique",
            "/data/music",
            "/nas/Blues 2",
            r"D:\Musique",
            r"C:\Users\bertrand\Music",
            r"\\NAS\Musique\Jazz",
            "//NAS/Musique/Jazz",
        ] {
            assert!(
                !dans_un_arbre_systeme(chemin),
                "fermé à tort : {chemin:?} — la musique vit là"
            );
        }
    }

    /// `Path::is_absolute()` aurait déclaré tous les chemins Windows relatifs
    /// sur l'hôte POSIX du CI : la garde aurait refusé Windows en entier tout
    /// en restant verte. Ce test fixe la reconnaissance des trois écritures.
    #[test]
    fn les_trois_ecritures_absolues_sont_reconnues_depuis_n_importe_quel_hote() {
        for chemin in [
            "/mnt/musique",
            r"D:\Musique",
            "D:/Musique",
            "Z:",
            r"\\NAS\Musique",
            "//NAS/Musique",
        ] {
            assert!(est_absolu(chemin), "déclaré relatif : {chemin:?}");
        }
        for chemin in ["", "Musique", "./Musique", "../Musique", "musique/jazz"] {
            assert!(!est_absolu(chemin), "déclaré absolu : {chemin:?}");
        }
        // Contre-épreuve de l'angle mort : sur un hôte POSIX, `is_absolute`
        // ment sur la forme Windows. C'est la raison d'être de `est_absolu`.
        #[cfg(not(target_os = "windows"))]
        assert!(!std::path::Path::new(r"D:\Musique").is_absolute());
    }

    /// La garde d'entrée refuse pour la BONNE raison — un message qui se
    /// trompe de cause envoie l'utilisateur chercher ailleurs.
    #[test]
    fn la_garde_d_entree_nomme_le_motif_du_refus() {
        assert_eq!(verifier_le_chemin_demande("Musique"), Err(Refus::PasAbsolu));
        assert_eq!(
            verifier_le_chemin_demande(r"D:\Musique\..\Windows"),
            Err(Refus::RemonteVersLeParent)
        );
        assert_eq!(
            verifier_le_chemin_demande("/etc/ssh"),
            Err(Refus::ArbreSysteme)
        );
        assert_eq!(verifier_le_chemin_demande("/mnt/musique"), Ok(()));
        assert_eq!(verifier_le_chemin_demande(r"D:\Musique"), Ok(()));
    }

    /// Le lien symbolique : le texte du chemin est irréprochable, la cible ne
    /// l'est pas. Sans la forme canonique, le refus ne se déclenche jamais.
    #[cfg(unix)]
    #[test]
    fn un_lien_qui_pointe_vers_un_arbre_systeme_est_refuse() {
        // `/tmp` et non `std::env::temp_dir()` : sous macOS ce dernier vit
        // sous `/private/var`, donc DÉJÀ hors périmètre, et le test passerait
        // pour la mauvaise raison. `/tmp` (`/private/tmp` après résolution)
        // est dans le périmètre sur les deux systèmes.
        let base = std::path::PathBuf::from("/tmp").join(tune_core::test_scratch::scratch_name(
            "tune-explorateur-lien-i1275",
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).expect("dossier de test");
        let lien = base.join("sys");
        std::os::unix::fs::symlink("/etc", &lien).expect("lien de test");

        // Le texte passe la garde d'entrée : c'est bien le piège.
        assert_eq!(
            verifier_le_chemin_demande(&lien.to_string_lossy()),
            Ok(()),
            "le texte du lien devrait passer — sinon ce test ne prouve rien"
        );
        // La cible, elle, est refusée.
        assert!(!la_cible_reste_dans_le_perimetre(&lien));
        // Contre-épreuve : un dossier ordinaire du même endroit passe.
        let ordinaire = base.join("Musique");
        std::fs::create_dir_all(&ordinaire).expect("dossier de test");
        assert!(la_cible_reste_dans_le_perimetre(&ordinaire));

        std::fs::remove_dir_all(&base).ok();
    }

    /// Le préfixe « verbatim » de Windows est retiré, sinon aucune règle ne
    /// reconnaîtrait `\\?\C:\Windows` comme `C:\Windows`.
    #[test]
    fn la_forme_verbatim_de_windows_est_ramenee_a_sa_forme_ordinaire() {
        // `forme_canonique` passe par le disque ; c'est le retrait du préfixe
        // qu'on vérifie ici, sur le même texte.
        let verbatim = r"\\?\C:\Windows\System32";
        let ordinaire = verbatim.strip_prefix(r"\\?\").unwrap();
        assert!(!dans_un_arbre_systeme(verbatim));
        assert!(dans_un_arbre_systeme(ordinaire));
    }
}
