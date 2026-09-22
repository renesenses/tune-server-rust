//! Regrouper un coffret ÉCLATÉ en un album par disque — chantier « coffrets ».
//!
//! # Le fait, mesuré sur le .18 le 19/09/2026
//!
//! 21 séries, 68 albums : un coffret dont chaque disque est devenu un album à
//! part entière. *Radio Nova — La boîte Bleue* en compte vingt-cinq.
//!
//! ```text
//! #455  d1  Radio Nova - La boîte Bleue, Disc 1
//! #482  d2  Radio Nova - La boîte Bleue, Disc 2
//! …
//! ```
//!
//! Contrairement aux vieux rips (voir [`super::disques_abimes`]), les tags de
//! DISQUE sont ici justes : chaque album porte le sien. Il ne manque que le
//! regroupement.
//!
//! # 🔴 Pourquoi le DOSSIER, et pas le titre
//!
//! Trois pièges, tous mesurés, qu'un regroupement par titre ne passe pas :
//!
//! 1. **Un volume n'est pas un disque.** *Standards, Vol. 2* de Keith Jarrett
//!    et les *B-Sides Vol. 1 / Vol. 2* de Radiohead sont de vrais albums
//!    distincts. Le marqueur ne reconnaît donc que `cd`, `disc`, `disque`,
//!    `disk` — jamais `vol`.
//! 2. **L'artiste ment.** *A Love Supreme, Disc 1* est classé en « Various
//!    Artists » et *Disc 2* en « John Coltrane », alors que les deux dossiers
//!    sont côte à côte sous `…/John Coltrane/`. Grouper par artiste perd le
//!    coffret ; grouper en ignorant l'artiste rapprocherait deux *Greatest
//!    Hits, Disc 1* sans rapport.
//! 3. **Le titre d'album peut porter « Vol. » dans son SOCLE** : *Jazz in
//!    Paris: Saint-Germain-Des-Prés, Vol. 3 1946-1956, Disc 1*. Seul le
//!    marqueur FINAL compte.
//!
//! Le dossier tranche les trois : les disques d'un coffret sont des dossiers
//! FRÈRES, sous un même parent, dont les noms ne diffèrent que par le
//! marqueur. C'est un fait de rangement, pas une déduction — et c'est ce que
//! le ripper a écrit.
//!
//! Relevé du même jour : 73 disques ISOLÉS portent un marqueur sans avoir de
//! frère. Aucun ne doit être touché.

use std::collections::BTreeMap;

/// Un album, réduit à ce que le regroupement regarde.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlbumAGrouper {
    pub id: i64,
    pub titre: String,
    /// Le dossier d'UNE de ses pistes. Les disques d'un coffret n'en ont qu'un.
    pub dossier: String,
}

/// Un coffret reconnu : ses disques, et le nom qu'il devra porter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coffret {
    /// Le dossier parent commun — l'identité du coffret.
    pub parent: String,
    /// Le titre d'album sans son marqueur de disque.
    pub titre: String,
    /// `(numéro de disque, id d'album)`, par numéro croissant.
    pub disques: Vec<(u32, i64)>,
}

impl Coffret {
    /// L'album qui ABSORBE les autres : le disque de plus petit numéro.
    ///
    /// Pas « le plus fourni », comme pour un doublon : ici les disques sont
    /// complémentaires, et le disque 1 est le seul choix qui ne surprenne
    /// personne.
    pub fn cible(&self) -> Option<i64> {
        self.disques.first().map(|(_, id)| *id)
    }

    /// Ceux qui seront absorbés.
    pub fn absorbes(&self) -> Vec<i64> {
        self.disques.iter().skip(1).map(|(_, id)| *id).collect()
    }
}

/// Les mots qui désignent un disque. UNE seule liste, partagée par la
/// détection automatique ([`marqueur_final`]) et par la composition à la main
/// ([`titre_commun`]) : deux copies divergeraient à la première addition.
///
/// ⚠️ `vol` n'en fait pas partie, et c'est tout le sujet du piège n° 1.
pub const MOTS_DE_DISQUE: [&str; 4] = ["cd", "disc", "disque", "disk"];

/// Le marqueur de disque EN FIN de nom, et le socle qui le précède.
///
/// ⚠️ `vol` en est exclu, et c'est tout le sujet du piège n° 1.
pub fn marqueur_final(nom: &str) -> Option<(String, u32)> {
    let n = nom.trim_end();
    let bas = n.to_lowercase();
    for mot in MOTS_DE_DISQUE {
        let mut i = bas.len();
        // On remonte les chiffres de fin.
        let chiffres: String = bas
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if chiffres.is_empty() || chiffres.len() > 2 {
            continue;
        }
        i -= chiffres.len();
        let avant = bas[..i].trim_end_matches([' ', '.', '_', '-']);
        if !avant.ends_with(mot) {
            continue;
        }
        let socle =
            avant[..avant.len() - mot.len()].trim_end_matches([' ', '.', '_', '-', ',', '(', '[']);
        let numero: u32 = chiffres.parse().ok()?;
        if numero == 0 {
            return None;
        }
        // Il doit rester quelque chose devant : « CD1 » seul est un dossier de
        // disque, pas un coffret nommé.
        if socle.is_empty() {
            return None;
        }
        return Some((n[..socle.len()].trim_end().to_string(), numero));
    }
    None
}

/// LE TITRE D'UN COFFRET COMPOSÉ À LA MAIN — le plus long préfixe COMMUN.
///
/// Bertrand, 20/09/2026 : « je voudrais créer un coffret pour 101 de Depeche
/// Mode », dont la bibliothèque porte deux albums *101 - Disc A* (9 pistes) et
/// *101 - Disc B* (11 pistes).
///
/// 🔴 POURQUOI PAS [`marqueur_final`]. Elle ne lit que des CHIFFRES
/// (`is_ascii_digit`, une ou deux positions) : *Disc A* et *Disc B* lui sont
/// invisibles, et c'est précisément le cas de Bertrand. Le regroupement
/// automatique ne pouvait donc pas le voir — celui-ci est composé à la main,
/// et il ne doit dépendre d'AUCUN vocabulaire de marqueur. Le préfixe commun
/// n'en connaît aucun : il marche sur *Disc A/B*, sur *CD1/CD2*, sur
/// *Première partie / Deuxième partie*, et sur ce qui n'a pas encore été
/// inventé.
///
/// ⚠️ LA COUPE NE TOMBE JAMAIS AU MILIEU D'UN MOT. *Abbey Road* et *Abbey
/// Roadshow* ont « Abbey Road » en préfixe commun, mais le second continue sur
/// une lettre : nommer le coffret « Abbey Road » inventerait un titre. On
/// recule alors jusqu'à la dernière séparation.
///
/// Rend `None` quand il ne reste rien d'utilisable : l'appelant garde alors le
/// titre de l'album cible plutôt que d'en inventer un.
pub fn titre_commun(titres: &[String]) -> Option<String> {
    if titres.len() < 2 {
        return None;
    }
    // Le préfixe commun, CARACTÈRE par caractère : un `&str[..n]` sur des
    // octets couperait un accent en deux et paniquerait.
    let premier: Vec<char> = titres[0].chars().collect();
    let mut n = premier.len();
    for t in &titres[1..] {
        let autre: Vec<char> = t.chars().collect();
        let mut i = 0;
        while i < n && i < autre.len() && premier[i] == autre[i] {
            i += 1;
        }
        n = i;
        if n == 0 {
            return None;
        }
    }
    // La coupe tombe-t-elle au MILIEU d'un mot ? Elle le fait dès qu'un des
    // titres continue sur une lettre ou un chiffre — « Reiner RCA, CD01 » et
    // « …CD02 » divergent à l'intérieur de « CD01 ». On recule alors jusqu'à
    // la dernière séparation ; sans séparation, il n'y a pas de titre honnête.
    if titres
        .iter()
        .any(|t| t.chars().nth(n).is_some_and(|c| c.is_alphanumeric()))
    {
        n = premier[..n].iter().rposition(|c| !c.is_alphanumeric())? + 1;
    }
    let socle = rogner_separateurs(&premier[..n].iter().collect::<String>());
    // 🔴 LE DERNIER MOT DU SOCLE PEUT ÊTRE LE MARQUEUR LUI-MÊME.
    //
    // « 101 - Disc A » et « 101 - Disc B » ne divergent qu'à la lettre finale :
    // le préfixe commun vaut « 101 - Disc », et s'arrêter là nommerait le
    // coffret « 101 - Disc ». Le mot de trop est celui que
    // [`marqueur_final`] connaît déjà — on réemploie SA liste plutôt que d'en
    // écrire une seconde, qui divergerait à la première addition.
    //
    // ⚠️ Et seulement celle-là : « Le Ring — Première partie » / « …Deuxième
    // partie » s'arrête à « Le Ring », parce que « Ring » n'en fait pas
    // partie. Sans cette réserve, on retirerait un mot du titre.
    let socle = match socle.rsplit_once(|c: char| c.is_whitespace()) {
        Some((avant, dernier))
            // ⚠️ Rogné des DEUX côtés : « Pulse (disque 1) » laisse
            // « (disque » en dernier mot, parenthèse ouvrante comprise.
            if MOTS_DE_DISQUE.contains(&dernier.trim_matches(SEPARATEURS).to_lowercase().as_str()) =>
        {
            rogner_separateurs(avant)
        }
        _ => socle,
    };
    if socle.is_empty() { None } else { Some(socle) }
}

/// Les séparateurs qu'un titre traîne autour de son socle — tirets longs
/// compris (« Le Ring — Première partie »).
const SEPARATEURS: [char; 14] = [
    ' ', '.', '_', '-', ',', ':', ';', '(', ')', '[', ']', '/', '\u{2013}', '\u{2014}',
];

/// Le socle, débarrassé de ce qu'il traîne À DROITE. La gauche est gardée :
/// c'est le début du titre.
fn rogner_separateurs(s: &str) -> String {
    s.trim().trim_end_matches(SEPARATEURS).trim().to_string()
}

/// Le parent d'un dossier, et son nom de feuille.
fn parent_et_feuille(dossier: &str) -> (String, String) {
    match dossier.rsplit_once('/') {
        Some((p, f)) => (p.to_string(), f.to_string()),
        None => (String::new(), dossier.to_string()),
    }
}

/// Les coffrets ÉCLATÉS d'un lot d'albums.
///
/// Un socle qui ne rassemble qu'un seul disque n'est pas un coffret : c'est un
/// disque isolé, et on n'y touche pas.
pub fn coffrets(albums: &[AlbumAGrouper]) -> Vec<Coffret> {
    let mut par_socle: BTreeMap<(String, String), Vec<(u32, i64, String)>> = BTreeMap::new();
    for a in albums {
        let (parent, feuille) = parent_et_feuille(&a.dossier);
        let Some((socle, numero)) = marqueur_final(&feuille) else {
            continue;
        };
        par_socle
            .entry((parent, socle.to_lowercase()))
            .or_default()
            .push((numero, a.id, a.titre.clone()));
    }
    par_socle
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((parent, _), mut v)| {
            v.sort_by_key(|(n, id, _)| (*n, *id));
            // Le titre du coffret vient du TITRE D'ALBUM du premier disque,
            // marqueur retiré — pas du nom de dossier, qui porte souvent
            // l'année et l'artiste en préfixe (« 2008-The Quintessence… »).
            let titre = v
                .first()
                .map(|(_, _, t)| {
                    marqueur_final(t)
                        .map(|(s, _)| s)
                        .unwrap_or_else(|| t.clone())
                })
                .unwrap_or_default();
            Coffret {
                parent,
                titre,
                disques: v.into_iter().map(|(n, id, _)| (n, id)).collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alb(id: i64, titre: &str, dossier: &str) -> AlbumAGrouper {
        AlbumAGrouper {
            id,
            titre: titre.into(),
            dossier: dossier.into(),
        }
    }

    #[test]
    fn le_marqueur_final_se_lit_sous_ses_formes() {
        // La ponctuation de liaison part avec le marqueur : c'est un TITRE
        // qu'on rend, pas un fragment de nom de dossier.
        assert_eq!(
            marqueur_final("A Love Supreme, Disc 2"),
            Some(("A Love Supreme".into(), 2))
        );
        assert_eq!(marqueur_final("Pulse CD1"), Some(("Pulse".into(), 1)));
        assert_eq!(
            marqueur_final("Pulse (CD 2)"),
            None,
            "un marqueur entre parenthèses fermées n'est pas final"
        );
        assert_eq!(
            marqueur_final("Album - Disque 3"),
            Some(("Album".into(), 3))
        );
    }

    /// 🔴 PIÈGE N° 1 — mesuré : un volume n'est pas un disque.
    #[test]
    fn un_volume_n_est_pas_un_disque() {
        assert_eq!(marqueur_final("Standards, Vol. 2"), None);
        assert_eq!(marqueur_final("B-Sides Vol. 1"), None);
    }

    #[test]
    fn un_marqueur_sans_socle_ou_a_zero_ne_compte_pas() {
        assert_eq!(marqueur_final("CD1"), None);
        assert_eq!(marqueur_final("Disc 0"), None);
        assert_eq!(marqueur_final("Album"), None);
    }

    /// Le cas RÉEL de Chet Baker, chemins relevés sur le .18.
    #[test]
    fn deux_dossiers_freres_font_un_coffret() {
        let base = "/data/music/NEW_FLAC/JAZZ CLASSIC/Chet Baker";
        let c = coffrets(&[
            alb(
                39,
                "The Quintessence (…), Disc 1",
                &format!("{base}/2008-The Quintessence (…), Disc 1"),
            ),
            alb(
                38,
                "The Quintessence (…), Disc 2",
                &format!("{base}/2008-The Quintessence (…), Disc 2"),
            ),
        ]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].cible(), Some(39), "le disque 1 absorbe");
        assert_eq!(c[0].absorbes(), vec![38]);
        assert_eq!(
            c[0].titre, "The Quintessence (…)",
            "la virgule de liaison ne reste pas dans le titre"
        );
    }

    /// 🔴 PIÈGE N° 2 — mesuré : l'artiste ment, le dossier non.
    #[test]
    fn l_artiste_peut_diverger_le_dossier_tranche() {
        let base = "/data/music/NEW_FLAC/JAZZ CLASSIC/John Coltrane";
        // #76 est classé « Various Artists », #62 « John Coltrane » : la
        // fonction ne regarde pas l'artiste, et les réunit quand même.
        let c = coffrets(&[
            alb(
                76,
                "A Love Supreme, Disc 1",
                &format!("{base}/1965-A Love Supreme, Disc 1"),
            ),
            alb(
                62,
                "A Love Supreme, Disc 2",
                &format!("{base}/1965-A Love Supreme, Disc 2"),
            ),
        ]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].disques.len(), 2);
    }

    /// 🔴 …et la contre-épreuve : deux parents DIFFÉRENTS ne se rejoignent pas,
    /// même sous un socle identique. C'est ce qui empêche de rapprocher deux
    /// « Greatest Hits, Disc 1 » sans rapport.
    #[test]
    fn deux_parents_differents_ne_font_pas_un_coffret() {
        let c = coffrets(&[
            alb(
                1,
                "Greatest Hits, Disc 1",
                "/m/Artiste A/Greatest Hits, Disc 1",
            ),
            alb(
                2,
                "Greatest Hits, Disc 1",
                "/m/Artiste B/Greatest Hits, Disc 1",
            ),
        ]);
        assert!(c.is_empty(), "{c:?}");
    }

    /// 🔴 PIÈGE N° 3 — « Vol. 3 » dans le SOCLE, « Disc N » comme marqueur.
    #[test]
    fn un_volume_dans_le_socle_ne_gene_pas_le_marqueur() {
        let base = "/m/VA";
        let c = coffrets(&[
            alb(
                465,
                "Jazz in Paris: …, Vol. 3 1946-1956, Disc 1",
                &format!("{base}/2005-Jazz in Paris, Vol. 3, Disc 1"),
            ),
            alb(
                458,
                "Jazz in Paris: …, Vol. 3 1946-1956, Disc 2",
                &format!("{base}/2005-Jazz in Paris, Vol. 3, Disc 2"),
            ),
        ]);
        assert_eq!(c.len(), 1);
        assert!(
            c[0].titre.contains("Vol. 3"),
            "le volume reste dans le titre : {}",
            c[0].titre
        );
    }

    /// Un disque ISOLÉ n'est pas un coffret — 73 cas mesurés sur le .18.
    #[test]
    fn un_disque_isole_n_est_jamais_touche() {
        let c = coffrets(&[alb(
            991,
            "The Song Remains The Same (CD 2)",
            "/m/LZ/1976-The Song Remains The Same, CD 2",
        )]);
        assert!(c.is_empty());
    }

    #[test]
    fn les_disques_sont_ordonnes_et_la_cible_est_le_premier() {
        let base = "/m/VA/2008-Boite";
        let c = coffrets(&[
            alb(3, "Boite, Disc 10", &format!("{base}, Disc 10")),
            alb(1, "Boite, Disc 2", &format!("{base}, Disc 2")),
            alb(2, "Boite, Disc 1", &format!("{base}, Disc 1")),
        ]);
        assert_eq!(c.len(), 1);
        // 🔴 Ordre NUMÉRIQUE : « Disc 10 » ne passe pas avant « Disc 2 ».
        assert_eq!(c[0].disques, vec![(1, 2), (2, 1), (10, 3)]);
        assert_eq!(c[0].cible(), Some(2));
        assert_eq!(c[0].absorbes(), vec![1, 3]);
    }

    // ------------------------------------------------------------------
    // `titre_commun` — le coffret composé À LA MAIN
    // ------------------------------------------------------------------

    fn t(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn le_cas_de_bertrand_101_disc_a_et_disc_b() {
        // 🔴 Exactement ce que montre sa capture du 20/09/2026 : deux albums
        // Depeche Mode, 9 et 11 pistes. `marqueur_final` n'y voit RIEN — la
        // lettre n'est pas un chiffre — et c'est pour cela que cette
        // fonction-ci existe.
        assert_eq!(marqueur_final("101 - Disc A"), None);
        assert_eq!(
            titre_commun(&t(&["101 - Disc A", "101 - Disc B"])),
            Some("101".to_string())
        );
    }

    #[test]
    fn il_ne_connait_aucun_vocabulaire_de_marqueur() {
        // Ni « disc », ni « cd » : le préfixe commun se moque du mot employé.
        assert_eq!(
            titre_commun(&t(&[
                "Le Ring — Première partie",
                "Le Ring — Deuxième partie"
            ])),
            Some("Le Ring".to_string())
        );
        assert_eq!(
            titre_commun(&t(&[
                "Reiner RCA, CD01",
                "Reiner RCA, CD02",
                "Reiner RCA, CD03"
            ])),
            Some("Reiner RCA".to_string())
        );
    }

    #[test]
    fn deux_titres_identiques_rendent_ce_titre() {
        assert_eq!(
            titre_commun(&t(&["The Wall", "The Wall"])),
            Some("The Wall".to_string())
        );
    }

    #[test]
    fn la_coupe_ne_tombe_jamais_au_milieu_d_un_mot() {
        // « Abbey Road » est bien le préfixe commun — mais le second titre
        // continue sur une lettre. Nommer le coffret « Abbey Road »
        // inventerait un titre que personne n'a écrit : on recule.
        assert_eq!(
            titre_commun(&t(&["Abbey Road", "Abbey Roadshow"])),
            Some("Abbey".to_string())
        );
        // Et quand il n'y a aucune séparation où reculer, on ne rend rien.
        assert_eq!(titre_commun(&t(&["Kind", "Kinder"])), None);
    }

    #[test]
    fn sans_rien_de_commun_il_ne_rend_rien() {
        assert_eq!(titre_commun(&t(&["Blue Train", "Giant Steps"])), None);
        // Un seul album n'est pas un coffret.
        assert_eq!(titre_commun(&t(&["101 - Disc A"])), None);
        assert_eq!(titre_commun(&[]), None);
    }

    #[test]
    fn il_ne_coupe_pas_un_accent_en_deux() {
        // Le préfixe se calcule en CARACTÈRES : un découpage par octets
        // paniquerait au milieu du « é ».
        assert_eq!(
            titre_commun(&t(&["Été 1970 / disque un", "Été 1970 / disque deux"])),
            Some("Été 1970".to_string())
        );
    }

    #[test]
    fn la_ponctuation_de_fin_est_retiree() {
        assert_eq!(
            titre_commun(&t(&["Pulse (disque 1)", "Pulse (disque 2)"])),
            Some("Pulse".to_string())
        );
    }
}
