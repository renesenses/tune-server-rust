//! Compilations éparpillées sur plusieurs dossiers.
//!
//! Un dossier est un album ([`super::album_folder`]) — sauf quand le même
//! disque a été rangé une fois par artiste. Les téléchargements Qobuz font
//! exactement ça pour les compilations :
//!
//! ```text
//! Qobuz/Corte Real/OUF L'anthologie Souterraine 2015-2017/01 - Opium.flac
//! Qobuz/Alligator/OUF L'anthologie Souterraine 2015-2017/03 - Rafale.flac
//! ```
//!
//! Une seule anthologie, quarante-et-un dossiers, donc quarante-et-une entrées
//! d'album d'une piste chacune (#1440 : 22 familles, 172 fausses entrées sur
//! une bibliothèque de 2 144 albums).
//!
//! Recoller ces dossiers demande de la prudence : « Live » et « Greatest Hits »
//! sont des titres que des dizaines d'artistes portent légitimement. Le seul
//! critère solide est la **conjonction** de trois faits — même nom de dossier,
//! dossiers parents frères, et numéros de piste qui ne se chevauchent pas.

use std::path::Path;

/// Deux dossiers d'album sont-ils les éclats possibles d'un même disque ?
///
/// Vrai quand ils portent le **même nom**, sous des parents **différents**,
/// eux-mêmes enfants d'un **même dossier**. C'est la forme que produit un
/// rangement par artiste :
///
/// ```text
/// racine/Artiste A/Le Disque/   ┐ frères éparpillés
/// racine/Artiste B/Le Disque/   ┘
/// ```
///
/// Faux dès qu'un seul de ces trois faits manque — notamment si les deux
/// dossiers ont le même parent (deux éditions côte à côte, pas une
/// compilation) ou si les noms diffèrent (`2005-Greatest Hits` contre
/// `1992-Greatest Hits`, cas réel de la bibliothèque .18).
pub fn is_scattered_sibling(a: &str, b: &str) -> bool {
    let (pa, pb) = (Path::new(a), Path::new(b));
    if pa == pb {
        return false;
    }
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    if name(pa) != name(pb) || name(pa).is_none() {
        return false;
    }
    let (parent_a, parent_b) = match (pa.parent(), pb.parent()) {
        (Some(x), Some(y)) => (x, y),
        _ => return false,
    };
    // Même parent ⇒ deux dossiers voisins du même artiste, pas un éparpillement.
    if parent_a == parent_b {
        return false;
    }
    match (parent_a.parent(), parent_b.parent()) {
        // Le grand-parent doit être un VRAI dossier commun. La racine du
        // système de fichiers n'en est pas un : deux artistes posés à `/`
        // n'ont rien qui les relie, et fusionner là-dessus serait fusionner
        // sur rien. Dans le doute, on refuse.
        (Some(ga), Some(gb)) => ga == gb && !ga.as_os_str().is_empty() && ga.parent().is_some(),
        _ => false,
    }
}

/// Peut-on rattacher une piste portant `track_number` à un album qui occupe
/// déjà les numéros `taken` ?
///
/// C'est le garde-fou qui distingue une compilation éclatée d'un homonyme :
/// deux « Greatest Hits » différents commencent tous deux à la piste 1, alors
/// que les éclats d'une même anthologie se partagent la numérotation.
///
/// Une piste sans numéro ne prouve rien : on refuse, plutôt que de fusionner
/// sur une intuition.
pub fn track_number_is_free(track_number: Option<i32>, taken: &[i32]) -> bool {
    match track_number {
        Some(n) if n > 0 => !taken.contains(&n),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Chemins RÉELS relevés sur la bibliothèque de .18 (#1440).
    const OUF_CORTE_REAL: &str =
        "/mnt/recordings_usb/Qobuz/Corte Real/OUF L'anthologie Souterraine 2015-2017";
    const OUF_ALLIGATOR: &str =
        "/mnt/recordings_usb/Qobuz/Alligator/OUF L'anthologie Souterraine 2015-2017";
    const GREATEST_BENATAR: &str = "/data/music/NEW_FLAC/POP-ROCK/P/Pat Benatar/2005-Greatest Hits";
    const GREATEST_POLICE: &str = "/data/music/NEW_FLAC/POP-ROCK/P/Police/1992-Greatest Hits";

    #[test]
    fn the_real_scattered_anthology_is_recognised() {
        // Le cas signalé : une anthologie rangée par artiste de piste.
        assert!(is_scattered_sibling(OUF_CORTE_REAL, OUF_ALLIGATOR));
        assert!(is_scattered_sibling(OUF_ALLIGATOR, OUF_CORTE_REAL));
    }

    #[test]
    fn two_real_greatest_hits_are_never_merged() {
        // Pat Benatar (20 pistes, n° 1..20) et Police (16 pistes, n° 1..16) :
        // même titre d'album, artistes différents, et DEUX raisons de refuser —
        // les noms de dossier portent l'année, et les numéros se chevauchent.
        assert!(!is_scattered_sibling(GREATEST_BENATAR, GREATEST_POLICE));
        assert!(!track_number_is_free(Some(1), &[1, 2, 3]));
    }

    #[test]
    fn same_named_folders_under_one_parent_would_still_be_refused_on_numbers() {
        // Si les deux dossiers portaient le MÊME nom (« Greatest Hits » sans
        // année), la parenté ne suffirait pas : c'est le chevauchement des
        // numéros qui tranche.
        let a = "/data/music/P/Pat Benatar/Greatest Hits";
        let b = "/data/music/P/Police/Greatest Hits";
        assert!(is_scattered_sibling(a, b), "la forme est bien fraternelle");
        assert!(
            !track_number_is_free(Some(1), &[1, 2, 3, 4]),
            "mais la piste 1 est déjà prise : refus"
        );
    }

    #[test]
    fn tracks_of_the_real_anthology_do_not_collide() {
        // Corte Real occupe la piste 1, Alligator la 3 : elles se complètent.
        assert!(track_number_is_free(Some(3), &[1]));
        assert!(track_number_is_free(Some(1), &[3, 7, 12]));
    }

    #[test]
    fn a_track_without_a_number_never_merges() {
        assert!(!track_number_is_free(None, &[]));
        assert!(!track_number_is_free(Some(0), &[]));
    }

    #[test]
    fn siblings_under_the_same_parent_are_not_scattered() {
        // Deux éditions côte à côte chez le même artiste : même parent.
        let a = "/music/Pink Floyd/Animals";
        let b = "/music/Pink Floyd/Animals";
        assert!(!is_scattered_sibling(a, b), "identiques");
        assert!(!is_scattered_sibling(
            "/music/Pink Floyd/CD1",
            "/music/Pink Floyd/CD2"
        ));
    }

    #[test]
    fn different_roots_are_not_scattered() {
        // Même nom, même profondeur, mais racines distinctes : deux
        // bibliothèques, deux disques.
        assert!(!is_scattered_sibling(
            "/mnt/a/Artiste/Le Disque",
            "/mnt/b/Artiste/Le Disque"
        ));
    }

    #[test]
    fn a_folder_at_the_root_has_no_grandparent() {
        assert!(!is_scattered_sibling("/A/Disque", "/B/Disque"));
    }

    #[test]
    fn folder_name_comparison_ignores_case() {
        assert!(is_scattered_sibling(
            "/r/Artiste A/Le Disque",
            "/r/Artiste B/LE DISQUE"
        ));
    }
}
