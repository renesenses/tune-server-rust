//! Clé de dédoublonnage des pochettes d'une mosaïque.
//!
//! # Le défaut qu'elle corrige
//!
//! Une mosaïque montre quatre pochettes censées être DISTINCTES. Sur la
//! bibliothèque de Bertrand, elles ne l'étaient pas : la collection
//! « Classique » affichait quatre fois le même coffret Górecki alors qu'elle
//! compte 139 albums.
//!
//! Mesuré sur son serveur le 02/09/2026 : un même disque physique est stocké
//! comme PLUSIEURS lignes d'`albums`, une par artiste crédité, chacune avec son
//! propre fichier de pochette en cache. Quatre lignes, quatre chemins, une
//! seule image.
//!
//! | Collection | Titre | Lignes d'album |
//! |---|---|---|
//! | Classique | Les indispensables du piano (96kHz/24bit) | 13 |
//! | Bandes Originales | I Give It A Year | 14 |
//! | 2025 | Coco Maria Presents: New Dimensions In Latin Music | 11 |
//! | Blues | 75 Birthday Bash (Live) | 6 |
//!
//! # Pourquoi le TITRE seul, et pas artiste + titre
//!
//! Une première version groupait sur artiste + titre. C'était exactement à
//! côté : l'artiste est précisément ce qui VARIE d'une ligne à l'autre — treize
//! pianistes pour un seul disque. Le titre est ce qu'elles ont en commun.
//!
//! Le risque symétrique — deux albums homonymes d'artistes différents réduits à
//! une seule case — est réel mais sans conséquence ici : on ne choisit que
//! quatre pochettes parmi des dizaines, et l'album suivant prend la place. Le
//! coût d'un faux regroupement est une image différente ; celui d'un
//! regroupement manqué est la mosaïque entière remplie d'une seule pochette.
//!
//! # Le suffixe entre parenthèses est retiré
//!
//! « A Nonesuch Retrospective » et « A Nonesuch Retrospective (24bit) »
//! désignent la même pochette. Sans ce nettoyage, le coffret Górecki repassait
//! à deux cases au lieu de quatre : le titre seul ne suffisait pas.
//!
//! Seuls les groupes en FIN de titre sont retirés, et l'on s'arrête si le
//! titre en devient vide. « (What's the Story) Morning Glory? » garde donc son
//! titre entier, et « ( ) » de Sigur Rós — un titre fait de la seule
//! parenthèse — n'est pas réduit à rien.

/// Sépare deux albums dans une mosaïque : le titre, nettoyé de son suffixe.
///
/// Sans titre exploitable, la clé retombe sur le `chemin` de la pochette. Une
/// piste hors album n'a pas de titre d'album : toutes se regrouperaient sinon
/// sous la clé vide, et une playlist de titres épars ne montrerait qu'une seule
/// image.
pub fn cle_pochette(titre: Option<&str>, chemin: &str) -> String {
    let brut = titre.unwrap_or("").trim();
    let base = sans_suffixe(brut);
    if base.is_empty() {
        return chemin.to_lowercase();
    }
    base.to_lowercase()
}

/// Retire les groupes parenthésés en fin de titre, tant qu'il en reste.
///
/// S'arrête net si le retrait viderait le titre : mieux vaut garder un titre
/// entièrement parenthésé que de le confondre avec tous les autres.
fn sans_suffixe(titre: &str) -> &str {
    let mut t = titre.trim();
    loop {
        let Some(fin) = t.chars().last() else { break };
        let ouvrant = match fin {
            ')' => '(',
            ']' => '[',
            _ => break,
        };
        let Some(i) = t.rfind(ouvrant) else { break };
        let reste = t[..i].trim_end();
        if reste.is_empty() {
            break;
        }
        t = reste;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les cas VÉCUS, relevés sur le serveur de Bertrand le 02/09/2026.
    ///
    /// Chacun est une mosaïque qui montrait la même pochette plusieurs fois.
    #[test]
    fn un_disque_eclate_en_plusieurs_artistes_ne_fait_qu_une_case() {
        // Le coffret Górecki : quatre lignes d'album, quatre chemins.
        let coffret = [
            ("Henryk Gorecki", "A Nonesuch Retrospective", "e62ba1ec"),
            ("Dawn Upshaw", "A Nonesuch Retrospective", "96361306"),
            ("Kronos Quartet", "A Nonesuch Retrospective", "c3bfe09d"),
            (
                "London Philharmonic Orchestra",
                "A Nonesuch Retrospective",
                "6b20ebb8",
            ),
            // La réédition 24 bits, que le titre seul ne réunissait pas.
            (
                "Dawn Upshaw",
                "A Nonesuch Retrospective (24bit)",
                "879aea0e",
            ),
        ];
        let cles: std::collections::HashSet<String> = coffret
            .iter()
            .map(|(_, titre, chemin)| cle_pochette(Some(titre), chemin))
            .collect();
        assert_eq!(
            cles.len(),
            1,
            "le coffret Górecki occuperait encore {} cases de la mosaïque",
            cles.len()
        );
    }

    #[test]
    fn treize_pianistes_un_seul_disque() {
        let cles: std::collections::HashSet<String> = (0..13)
            .map(|i| {
                cle_pochette(
                    Some("Les indispensables du piano (96kHz/24bit)"),
                    &format!("chemin{i}"),
                )
            })
            .collect();
        assert_eq!(cles.len(), 1);
    }

    /// La contre-épreuve : la clé doit encore SÉPARER ce qui est différent.
    /// Sans elle, « ne montrer qu'une pochette » passerait aussi le test
    /// précédent.
    #[test]
    fn des_albums_differents_restent_separes() {
        let albums = [
            ("Way Out West", "aa"),
            ("Come Away With Me (5.1 Remix)", "bb"),
            ("Standards, Vol. 2", "cc"),
            ("The Köln Concert (Live - 24 janvier 1975)", "dd"),
        ];
        let cles: std::collections::HashSet<String> = albums
            .iter()
            .map(|(t, c)| cle_pochette(Some(t), c))
            .collect();
        assert_eq!(cles.len(), 4, "des albums distincts ont été confondus");
    }

    #[test]
    fn un_titre_entierement_parenthese_survit() {
        // « ( ) » de Sigur Rós et « (What's the Story) Morning Glory? »
        // d'Oasis : tous deux présents dans la collection Rock. Vider le titre
        // les enverrait sur la clé du chemin, ou pire, les confondrait.
        assert_eq!(cle_pochette(Some("( )"), "x"), "( )");
        assert_eq!(
            cle_pochette(Some("(What's the Story) Morning Glory?"), "x"),
            "(what's the story) morning glory?"
        );
    }

    #[test]
    fn sans_titre_la_cle_est_le_chemin() {
        // Pistes hors album : sinon toutes sous la clé vide, et une playlist de
        // titres épars n'afficherait qu'une seule pochette.
        assert_ne!(cle_pochette(None, "aa"), cle_pochette(None, "bb"));
        assert_ne!(
            cle_pochette(Some("   "), "aa"),
            cle_pochette(Some(""), "bb")
        );
    }

    #[test]
    fn la_casse_ne_separe_pas() {
        assert_eq!(
            cle_pochette(Some("Kind Of Miles"), "a"),
            cle_pochette(Some("kind of MILES"), "b")
        );
    }

    #[test]
    fn les_crochets_comptent_comme_des_parentheses() {
        assert_eq!(
            cle_pochette(Some("Theodora [192kHz/24bit]"), "a"),
            cle_pochette(Some("Theodora"), "b")
        );
    }
}
