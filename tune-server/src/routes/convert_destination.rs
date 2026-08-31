//! Périmètre d'ÉCRITURE du convertisseur (#2944).
//!
//! Bilou (fil forum 1095) : « On a accès au seul répertoire de la bibliothèque
//! alors que pour contrôler le travail de conversion on aurait plutôt besoin
//! d'accéder à un répertoire à créer ou existant, répertoire de travail ».
//! Autrement dit : ranger le résultat directement sur le serveur, au lieu de
//! télécharger une archive puis de la décompresser à la main.
//!
//! #2943 a fait DIRE au serveur ce qu'il fait — sortie dans
//! `/tmp/tune-convert/{job_id}`, archive ZIP, sources en lecture seule. Ce
//! module-ci CHANGE cette vérité pour le mode facultatif où l'appelant désigne
//! un dossier : c'est donc la première fois qu'un client de ce serveur choisit
//! un chemin d'ÉCRITURE. La classe de défaut correspondante est la plus grave
//! qui soit, et elle ne se rattrape pas après coup.
//!
//! ## Ce qui borne l'écriture
//!
//! **Rien n'est inscriptible que l'exploitant n'ait NOMMÉ lui-même.** Le
//! périmètre est l'union de deux ensembles, tous deux déclarés côté serveur :
//!
//! 1. les **racines de bibliothèque** (`music_dirs`) — l'exploitant a déjà
//!    déclaré que le serveur gère la musique qui s'y trouve ;
//! 2. un **dossier de travail** facultatif, réglage `converter_output_root` —
//!    la réponse au besoin de Bilou : ranger AILLEURS que dans la
//!    bibliothèque, pour ne pas y déverser des doublons.
//!
//! Sans l'un ni l'autre, il n'y a pas de périmètre et toute destination est
//! refusée. Un serveur qui n'a rien déclaré n'écrit nulle part.
//!
//! ## Pourquoi les gardes sont écrites en logique de CHAÎNE
//!
//! Elles réutilisent telles quelles celles de l'explorateur de dossiers
//! (#1275, [`super::system::explorateur`]) — `est_absolu`,
//! `remonte_vers_le_parent`, `dans_un_arbre_systeme`, `forme_canonique` — et
//! le prédicat de containment [`sous_le_dossier`] (#2016). Aucune n'est
//! réécrite ici : une deuxième implémentation du même contrat, c'est
//! exactement ce qui a livré quatre fois le défaut de #2016.
//!
//! Le motif de fond : `Path::components()` voit `D:\x\..\y` comme UN SEUL
//! composant `Normal` sur un hôte POSIX — le `..` y est invisible — et
//! `Path::is_absolute()` y déclare relatif tout chemin Windows. Une garde
//! écrite avec `Path` **refuserait Windows en entier en restant verte** sur le
//! CI Linux. C'est l'angle mort exact de #1837 et #2056.
//!
//! ## La polarité du doute, ici, est INVERSÉE
//!
//! L'explorateur, qui ne fait que LIRE, laisse passer un chemin qu'il ne sait
//! pas résoudre : la lecture rendra sa propre erreur. Une écriture ne peut pas
//! se le permettre. Ici, « je ne sais pas prouver que c'est dedans » vaut
//! REFUS — voir [`la_cible_reste_dans_le_perimetre`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tune_core::metadata::enrich_scope::sous_le_dossier;

use super::system::explorateur;

/// Réglage nommant un dossier de travail hors bibliothèque. Absent par défaut :
/// tant que l'exploitant ne l'a pas posé, le périmètre se réduit aux racines de
/// bibliothèque, et s'il n'y en a aucune il n'y a pas de périmètre du tout.
pub(crate) const CLE_RACINE_DE_TRAVAIL: &str = "converter_output_root";

/// Motif du refus opposé à une destination demandée.
///
/// Le client n'en reçoit que le libellé. Comme pour l'explorateur, on ne lui
/// donne jamais de quoi distinguer « ce dossier existe mais je le refuse » de
/// « ce dossier n'existe pas » : cette différence est déjà un oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusDestination {
    /// Chemin relatif, ou vide. Une destination ne se dit qu'en absolu.
    PasAbsolu,
    /// Le chemin contient un segment `..`. Il n'est jamais nécessaire, et il
    /// est le seul moyen de faire mentir une garde de préfixe.
    RemonteVersLeParent,
    /// Le chemin tombe dans un arbre système.
    ArbreSysteme,
    /// Le serveur n'a aucun périmètre d'écriture déclaré.
    AucunPerimetre,
    /// Le chemin est bien formé mais hors des racines déclarées — ou sa CIBLE
    /// l'est, ce que seule la forme canonique fait apparaître.
    HorsPerimetre,
}

impl RefusDestination {
    pub(crate) fn libelle(self) -> &'static str {
        match self {
            RefusDestination::PasAbsolu => "destination must be an absolute path",
            RefusDestination::RemonteVersLeParent => "destination must not contain '..'",
            RefusDestination::ArbreSysteme => "destination is outside the writable perimeter",
            RefusDestination::AucunPerimetre => {
                "no writable destination is configured on this server"
            }
            RefusDestination::HorsPerimetre => "destination is outside the writable perimeter",
        }
    }
}

/// Retire le séparateur final d'un chemin, sans jamais vider une racine.
///
/// `sous_le_dossier` le fait déjà pour le dossier, mais on compare aussi des
/// chemins ENTRE eux (dédoublonnage des racines) : autant les normaliser une
/// fois pour toutes.
fn sans_separateur_final(chemin: &str) -> &str {
    let coupe = chemin.trim_end_matches(['/', '\\']);
    if coupe.is_empty() { chemin } else { coupe }
}

/// Les racines dans lesquelles ce serveur s'autorise à écrire.
///
/// Les racines elles-mêmes passent la garde d'entrée : une racine relative,
/// porteuse d'un `..` ou tombant dans un arbre système n'est pas un périmètre,
/// c'est une erreur de réglage, et l'accepter ouvrirait par le réglage ce que
/// la garde ferme par la requête.
pub(crate) fn racines_autorisees(
    dirs_bibliotheque: &[String],
    racine_de_travail: Option<&str>,
) -> Vec<String> {
    let mut racines: Vec<String> = Vec::new();
    for brut in dirs_bibliotheque
        .iter()
        .map(String::as_str)
        .chain(racine_de_travail)
    {
        let candidat = sans_separateur_final(brut.trim());
        if candidat.is_empty() {
            continue;
        }
        if explorateur::verifier_le_chemin_demande(candidat).is_err() {
            tracing::warn!(racine = %candidat, "convertisseur_racine_ecriture_ignoree");
            continue;
        }
        if !racines.iter().any(|deja| deja == candidat) {
            racines.push(candidat.to_string());
        }
    }
    racines
}

/// La garde appliquée au TEXTE de la destination demandée, avant tout accès
/// disque. Rend le chemin retenu, débarrassé de son séparateur final.
pub(crate) fn verifier_la_destination(
    demande: &str,
    racines: &[String],
) -> Result<PathBuf, RefusDestination> {
    let demande = sans_separateur_final(demande.trim());
    if !explorateur::est_absolu(demande) {
        return Err(RefusDestination::PasAbsolu);
    }
    if explorateur::remonte_vers_le_parent(demande) {
        return Err(RefusDestination::RemonteVersLeParent);
    }
    if explorateur::dans_un_arbre_systeme(demande) {
        return Err(RefusDestination::ArbreSysteme);
    }
    if racines.is_empty() {
        return Err(RefusDestination::AucunPerimetre);
    }
    if !racines
        .iter()
        .any(|racine| sous_le_dossier(demande, racine))
    {
        return Err(RefusDestination::HorsPerimetre);
    }
    Ok(PathBuf::from(demande))
}

/// Le plus proche ancêtre EXISTANT d'un chemin, et les segments qui restent à
/// créer sous lui.
///
/// Bilou demande « un répertoire à créer ou existant » : la destination peut
/// donc ne pas exister encore, et `std::fs::canonicalize` échoue sur ce qui
/// n'existe pas. On canonise ce qui existe, puis on rattache le reste — ce qui
/// suffit à voir un lien symbolique posé en chemin, qui est le seul cas où le
/// texte et la cible divergent.
fn plus_proche_ancetre_existant(chemin: &Path) -> Option<(PathBuf, Vec<OsString>)> {
    let mut a_creer: Vec<OsString> = Vec::new();
    let mut courant = chemin.to_path_buf();
    loop {
        if courant.exists() {
            a_creer.reverse();
            return Some((courant, a_creer));
        }
        let nom = courant.file_name()?.to_os_string();
        let parent = courant.parent()?.to_path_buf();
        if parent.as_os_str().is_empty() {
            return None;
        }
        a_creer.push(nom);
        courant = parent;
    }
}

/// Forme canonique d'un chemin qui peut ne pas exister encore.
pub(crate) fn forme_canonique_approchee(chemin: &Path) -> Option<String> {
    let (existant, a_creer) = plus_proche_ancetre_existant(chemin)?;
    let mut canonique = PathBuf::from(explorateur::forme_canonique(&existant)?);
    for segment in a_creer {
        canonique.push(segment);
    }
    Some(canonique.to_string_lossy().into_owned())
}

/// La destination, une fois RÉSOLUE sur le disque, reste-t-elle dans le
/// périmètre ?
///
/// Complète [`verifier_la_destination`] pour le cas du lien symbolique, où le
/// texte est irréprochable et la cible ne l'est pas : un lien
/// `<bibliothèque>/travail → /etc` suffit sinon à écrire dans `/etc` par un
/// chemin que la garde de texte accepte.
///
/// ⚠️ **Un chemin qu'on ne sait pas résoudre est REFUSÉ.** L'explorateur de
/// #1275, qui ne fait que lire, laisse passer dans ce cas — la lecture rendra
/// son erreur. Une écriture n'a pas ce filet : le doute doit fermer.
pub(crate) fn la_cible_reste_dans_le_perimetre(chemin: &Path, racines: &[String]) -> bool {
    let Some(canonique) = forme_canonique_approchee(chemin) else {
        return false;
    };
    if explorateur::dans_un_arbre_systeme(&canonique) {
        return false;
    }
    racines.iter().any(|racine| {
        // La racine se canonise aussi : sous macOS `/tmp` est `/private/tmp`,
        // et comparer une cible canonique à une racine qui ne l'est pas
        // refuserait une destination parfaitement légitime.
        let racine_canonique =
            explorateur::forme_canonique(Path::new(racine)).unwrap_or_else(|| racine.to_string());
        sous_le_dossier(&canonique, &racine_canonique)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn racines() -> Vec<String> {
        racines_autorisees(
            &["/mnt/musique".to_string(), "/nas/Jazz".to_string()],
            Some("/srv/travail"),
        )
    }

    // -----------------------------------------------------------------
    // Les CINQ refus. Chacun tombe si l'on retire la garde correspondante.
    // -----------------------------------------------------------------

    /// Hors périmètre : le chemin est absolu, sain, hors arbre système — et
    /// pourtant il n'est dans aucune racine déclarée. Sans cette garde, le
    /// convertisseur écrirait n'importe où sur le disque du serveur.
    #[test]
    fn une_destination_hors_du_perimetre_est_refusee() {
        for chemin in [
            "/home/quelqu_un/autre",
            "/opt/ailleurs",
            "/mnt",
            "/nas",
            "/srv",
            "/tmp/tune-convert",
        ] {
            assert_eq!(
                verifier_la_destination(chemin, &racines()),
                Err(RefusDestination::HorsPerimetre),
                "laissé passer hors périmètre : {chemin:?}"
            );
        }
    }

    /// Frontière de SÉPARATEUR, et pas préfixe de nom (#2016) : `/mnt/musique2`
    /// n'est pas sous `/mnt/musique`. C'est le défaut que ce dépôt a livré
    /// quatre fois, et il serait ici une écriture hors périmètre.
    #[test]
    fn un_prefixe_de_nom_n_est_pas_dans_le_perimetre() {
        for chemin in ["/mnt/musique2", "/nas/Jazz2/Sortie", "/srv/travailleurs"] {
            assert_eq!(
                verifier_la_destination(chemin, &racines()),
                Err(RefusDestination::HorsPerimetre),
                "préfixe de nom pris pour containment : {chemin:?}"
            );
        }
        // Contre-épreuve du prédicat naïf que la garde ne doit PAS être :
        let naif = |chemin: &str| chemin.starts_with("/mnt/musique");
        assert!(naif("/mnt/musique2"));
        assert!(!sous_le_dossier("/mnt/musique2", "/mnt/musique"));
    }

    /// Le `..` est refusé AVANT toute comparaison de préfixe, et avec les DEUX
    /// séparateurs. Sans lui, `/mnt/musique/../../etc` passe la garde de
    /// containment tout en désignant `/etc`.
    #[test]
    fn un_retour_arriere_est_refuse_avec_les_deux_separateurs() {
        for chemin in [
            "/mnt/musique/../../etc",
            "/mnt/musique/..",
            "/nas/Jazz/../../root",
            r"D:\Musique\..\..\Windows",
            "D:/Musique/../Windows",
        ] {
            assert_eq!(
                verifier_la_destination(chemin, &racines()),
                Err(RefusDestination::RemonteVersLeParent),
                "un « .. » non vu : {chemin:?}"
            );
        }
        // Contre-épreuve du découpage naïf : il ne voit AUCUN `..` de la forme
        // Windows, et la destination serait acceptée.
        let naif = |chemin: &str| chemin.split('/').any(|s| s == "..");
        assert!(!naif(r"D:\Musique\..\..\Windows"));
    }

    /// Un dossier dont le NOM commence par deux points n'est pas une remontée :
    /// refuser `..bootlegs` serait une régression, pas une garde.
    #[test]
    fn un_nom_qui_commence_par_deux_points_reste_accepte() {
        assert_eq!(
            verifier_la_destination("/mnt/musique/..bootlegs", &racines()),
            Ok(PathBuf::from("/mnt/musique/..bootlegs"))
        );
    }

    /// Chemin relatif : refusé, et pour le bon motif.
    #[test]
    fn une_destination_relative_est_refusee() {
        for chemin in ["", "sortie", "./sortie", "musique/converti", "   "] {
            assert_eq!(
                verifier_la_destination(chemin, &racines()),
                Err(RefusDestination::PasAbsolu),
                "déclaré absolu : {chemin:?}"
            );
        }
    }

    /// Arbre système : refusé avant même la comparaison de périmètre, sur les
    /// DEUX jeux de règles et depuis n'importe quel hôte.
    #[test]
    fn un_arbre_systeme_est_refuse_depuis_n_importe_quel_hote() {
        for chemin in [
            "/etc",
            "/etc/cron.d",
            "/root/.ssh",
            "/usr/local/bin",
            "/var/lib/tune",
            r"C:\Windows\System32",
            r"C:\ProgramData\Tune",
            "c:/windows/temp",
        ] {
            assert_eq!(
                verifier_la_destination(chemin, &racines()),
                Err(RefusDestination::ArbreSysteme),
                "arbre système laissé passer : {chemin:?}"
            );
        }
    }

    /// L'angle mort de #1837 / #2056, transposé à l'écriture : sur un hôte
    /// POSIX, `Path::is_absolute()` déclare RELATIF tout chemin Windows. Une
    /// garde écrite avec `Path` refuserait Windows en entier tout en restant
    /// VERTE sur le CI Linux — le défaut serait invisible jusqu'à ce qu'un
    /// testeur sous Windows le rencontre.
    #[test]
    fn un_chemin_windows_reste_reconnu_absolu_sur_un_hote_posix() {
        let racines_windows = racines_autorisees(&[r"D:\Musique".to_string()], None);
        assert_eq!(racines_windows, vec![r"D:\Musique".to_string()]);
        assert_eq!(
            verifier_la_destination(r"D:\Musique\Converti", &racines_windows),
            Ok(PathBuf::from(r"D:\Musique\Converti"))
        );
        assert_eq!(
            verifier_la_destination(r"E:\Ailleurs", &racines_windows),
            Err(RefusDestination::HorsPerimetre)
        );
        // Le mensonge que la garde contourne. Si ce `assert` tombe un jour,
        // c'est que `Path` a changé — pas que la garde est inutile.
        #[cfg(not(target_os = "windows"))]
        assert!(!Path::new(r"D:\Musique\Converti").is_absolute());
    }

    /// Aucun périmètre déclaré ⇒ aucune destination acceptée. Un serveur qui
    /// n'a ni racine de bibliothèque ni dossier de travail n'écrit nulle part,
    /// même pour un administrateur.
    #[test]
    fn sans_perimetre_declare_toute_destination_est_refusee() {
        let aucune: Vec<String> = racines_autorisees(&[], None);
        assert!(aucune.is_empty());
        assert_eq!(
            verifier_la_destination("/mnt/musique", &aucune),
            Err(RefusDestination::AucunPerimetre)
        );
    }

    /// Une racine de réglage malformée n'ouvre rien : elle est écartée du
    /// périmètre au lieu d'y entrer. Sinon `converter_output_root=/` ou
    /// `= ../..` deviendrait la porte que la garde de requête ferme.
    #[test]
    fn une_racine_malformee_est_ecartee_du_perimetre() {
        for mauvaise in ["", "   ", "relatif/travail", "/mnt/../etc", "/etc/tune"] {
            let r = racines_autorisees(&[], Some(mauvaise));
            assert!(r.is_empty(), "racine acceptée à tort : {mauvaise:?}");
        }
        // Une racine correcte, elle, entre bien.
        assert_eq!(
            racines_autorisees(&[], Some("/srv/travail/")),
            vec!["/srv/travail".to_string()]
        );
    }

    // -----------------------------------------------------------------
    // Le TÉMOIN anti-régression : ce que le convertisseur doit continuer
    // d'accepter. Sans lui, resserrer « pour être tranquille » casserait le
    // geste que #2944 demande, sans que rien ne le dise.
    // -----------------------------------------------------------------

    #[test]
    fn une_destination_legitime_est_acceptee() {
        for chemin in [
            "/mnt/musique",
            "/mnt/musique/Converti",
            "/mnt/musique/Converti/",
            "/nas/Jazz/24-96",
            "/srv/travail",
            "/srv/travail/2026-08/lot 1",
        ] {
            assert!(
                verifier_la_destination(chemin, &racines()).is_ok(),
                "refusé à tort : {chemin:?}"
            );
        }
    }

    /// La racine elle-même est dans son propre périmètre — `sous_le_dossier`
    /// rend `true` pour l'égalité, et une destination qui EST la racine est le
    /// cas le plus courant.
    #[test]
    fn la_racine_est_dans_son_propre_perimetre() {
        assert!(verifier_la_destination("/srv/travail", &racines()).is_ok());
        assert!(verifier_la_destination("/srv/travail/", &racines()).is_ok());
    }

    // -----------------------------------------------------------------
    // La CIBLE, quand le texte et le disque divergent.
    // -----------------------------------------------------------------

    /// Le lien symbolique : le texte du chemin est irréprochable — il est même
    /// DANS le périmètre — et sa cible n'y est pas. Sans la forme canonique, le
    /// convertisseur écrirait hors bibliothèque par un chemin que la garde de
    /// texte accepte.
    #[cfg(unix)]
    #[test]
    fn un_lien_qui_sort_du_perimetre_est_refuse() {
        // `/tmp` et non `std::env::temp_dir()` : sous macOS ce dernier vit sous
        // `/private/var`, donc DÉJÀ hors périmètre, et le test passerait pour
        // la mauvaise raison.
        let base = PathBuf::from("/tmp").join(tune_core::test_scratch::scratch_name(
            "tune-convert-destination-i2944",
        ));
        std::fs::remove_dir_all(&base).ok();
        let racine = base.join("bibliotheque");
        let dehors = base.join("dehors");
        std::fs::create_dir_all(&racine).expect("racine de test");
        std::fs::create_dir_all(&dehors).expect("dossier de test");

        let racines = vec![racine.to_string_lossy().into_owned()];

        let lien = racine.join("evasion");
        std::os::unix::fs::symlink(&dehors, &lien).expect("lien de test");

        // Le TEXTE passe la garde d'entrée : c'est bien le piège.
        assert!(
            verifier_la_destination(&lien.to_string_lossy(), &racines).is_ok(),
            "le texte du lien devrait passer — sinon ce test ne prouve rien"
        );
        // La CIBLE, elle, est refusée.
        assert!(
            !la_cible_reste_dans_le_perimetre(&lien, &racines),
            "un lien sortant a été accepté"
        );
        // Et un sous-dossier À CRÉER sous le lien l'est aussi.
        assert!(!la_cible_reste_dans_le_perimetre(
            &lien.join("lot"),
            &racines
        ));

        // Témoins verts : un dossier ordinaire, et un sous-dossier qui
        // n'existe pas encore — « un répertoire à créer », la demande de Bilou.
        let ordinaire = racine.join("Converti");
        std::fs::create_dir_all(&ordinaire).expect("dossier de test");
        assert!(la_cible_reste_dans_le_perimetre(&ordinaire, &racines));
        assert!(la_cible_reste_dans_le_perimetre(
            &racine.join("pas encore/la"),
            &racines
        ));
        assert!(la_cible_reste_dans_le_perimetre(&racine, &racines));

        std::fs::remove_dir_all(&base).ok();
    }

    /// Un lien qui vise un ARBRE SYSTÈME est refusé par la règle des arbres
    /// système, pas seulement par celle du périmètre — les deux doivent mordre.
    #[cfg(unix)]
    #[test]
    fn un_lien_qui_vise_un_arbre_systeme_est_refuse() {
        let base = PathBuf::from("/tmp").join(tune_core::test_scratch::scratch_name(
            "tune-convert-destination-sys-i2944",
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).expect("racine de test");
        let racines = vec![base.to_string_lossy().into_owned()];
        let lien = base.join("sys");
        std::os::unix::fs::symlink("/etc", &lien).expect("lien de test");

        assert!(verifier_la_destination(&lien.to_string_lossy(), &racines).is_ok());
        assert!(!la_cible_reste_dans_le_perimetre(&lien, &racines));

        std::fs::remove_dir_all(&base).ok();
    }

    /// La polarité du doute : un chemin dont AUCUN ancêtre n'existe ne peut pas
    /// être prouvé dans le périmètre, donc il est REFUSÉ. C'est la différence
    /// assumée avec l'explorateur en lecture seule de #1275, qui laisse passer.
    ///
    /// ⚠️ « Irrésolvable » ne veut PAS dire « inexistant ». Un dossier qui
    /// n'existe pas encore sous une racine qui, elle, existe, se résout très
    /// bien par son plus proche ancêtre — et il DOIT être accepté, c'est
    /// littéralement le « répertoire à créer » que Bilou demande. La première
    /// écriture de ce test l'ignorait et affirmait le contraire ; c'est
    /// l'assertion qui était fausse, pas la garde. Le témoin vert ci-dessous
    /// fige la distinction pour que personne ne « corrige » la garde dans ce
    /// sens-là.
    #[test]
    fn un_chemin_irresolvable_est_refuse_et_non_laisse_passer() {
        let racines = vec!["/mnt/musique".to_string()];
        // Forme Windows sur hôte POSIX : `parent()` ne rend rien, aucun ancêtre
        // n'existe, rien n'est prouvable ⇒ refus.
        #[cfg(not(target_os = "windows"))]
        assert!(!la_cible_reste_dans_le_perimetre(
            Path::new(r"D:\Musique"),
            &racines
        ));

        // Le témoin vert, sur de vrais dossiers : sous une racine existante, un
        // sous-dossier encore inexistant est accepté.
        #[cfg(unix)]
        {
            let base = PathBuf::from("/tmp").join(tune_core::test_scratch::scratch_name(
                "tune-convert-a-creer-i2944",
            ));
            std::fs::remove_dir_all(&base).ok();
            std::fs::create_dir_all(&base).expect("racine de test");
            let racines = vec![base.to_string_lossy().into_owned()];
            assert!(
                la_cible_reste_dans_le_perimetre(&base.join("pas/encore/la"), &racines),
                "« un répertoire à créer » a été refusé"
            );
            std::fs::remove_dir_all(&base).ok();
        }
    }
}
