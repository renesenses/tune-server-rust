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
//!
//! # Le 25/09/2026 : le parent reste, le SOCLE vient du titre
//!
//! GO de Bertrand : « regroupement automatique ». La fraternité des dossiers
//! reste la garde (même parent), mais le socle se lit désormais sur le TITRE
//! d'album d'abord, le nom de dossier à défaut : *Early Works* de Laurent
//! Garnier vit dans `1999-Early Works, Disc 1` et `2001-Early Works, Disc 2`,
//! deux dossiers dont les noms ne se ressemblent pas au marqueur près. Et
//! l'artiste compte de nouveau, À UNE EXCEPTION PRÈS, celle du piège n° 2 : un
//! disque classé « Various Artists » ne départage pas. Voir [`coffrets`].
//! L'application — au scan et au démarrage — vit dans
//! `crate::db::coffrets_auto`.

use std::collections::BTreeMap;

/// Un album, réduit à ce que le regroupement regarde.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlbumAGrouper {
    pub id: i64,
    pub titre: String,
    /// Le dossier d'UNE de ses pistes. Les disques d'un coffret n'en ont qu'un.
    pub dossier: String,
    /// L'artiste d'album. `None` : inconnu — il ne départage rien.
    pub artiste_id: Option<i64>,
    /// L'artiste d'album est « Various Artists » (ou apparenté).
    ///
    /// 🔴 Mesuré sur le .18 : *A Love Supreme, Disc 1* classé en « Various
    /// Artists », *Disc 2* en « John Coltrane », côte à côte sous
    /// `…/John Coltrane/`. Un tel disque ne départage pas : il est compté
    /// comme compatible avec l'artiste des autres. Deux artistes RÉELS
    /// différents, eux, séparent le groupe.
    pub artiste_de_compilation: bool,
}

/// Un coffret reconnu : ses disques, et le nom qu'il devra porter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coffret {
    /// L'IDENTITÉ STABLE du coffret : dossier parent + socle normalisé.
    ///
    /// Elle survit à un rescan, qui renouvelle les identifiants d'albums :
    /// c'est elle qu'on retient quand l'utilisateur DÉFAIT un regroupement,
    /// pour qu'il ne revienne pas.
    pub cle: String,
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

/// Ce qui relie un socle à son marqueur : « Titre, Disc 2 », « Titre - CD2 ».
const LIAISONS: [char; 11] = [
    ' ', '.', '_', '-', ',', '(', '[', ':', ';', '\u{2013}', '\u{2014}',
];

/// Le marqueur de disque EN FIN de nom, et le socle qui le précède.
///
/// Formes lues : « Titre, Disc 2 », « Titre CD2 », « Titre - Disque 3 »,
/// « Titre (CD 2) », « Titre [Disc 2] ». Le numéro est OBLIGATOIRE — un ou
/// deux chiffres, jamais zéro — et le mot de disque est un MOT : « Abcd 2 »
/// n'a pas de marqueur.
///
/// ⚠️ `vol` en est exclu, et c'est tout le sujet du piège n° 1.
///
/// 🔴 Le calcul se fait sur le nom D'ORIGINE, jamais sur sa version en
/// minuscules : `to_lowercase` change la longueur en octets de certains
/// caractères (« İ » passe de 2 à 3 octets), et découper l'original aux
/// indices de la copie paniquerait — au démarrage, puisque la passe
/// automatique y tourne.
pub fn marqueur_final(nom: &str) -> Option<(String, u32)> {
    let n = nom.trim_end();
    if let Some(r) = marqueur_entre_crochets(n) {
        return Some(r);
    }
    // On remonte les chiffres de fin.
    let avant_chiffres = n.trim_end_matches(|c: char| c.is_ascii_digit());
    let chiffres = &n[avant_chiffres.len()..];
    if chiffres.is_empty() || chiffres.len() > 2 {
        return None;
    }
    let numero: u32 = chiffres.parse().ok().filter(|&d| d > 0)?;
    let avant = avant_chiffres.trim_end_matches([' ', '.', '_', '-']);
    for mot in MOTS_DE_DISQUE {
        let Some(coupe) = avant.len().checked_sub(mot.len()) else {
            continue;
        };
        if !avant.is_char_boundary(coupe) || !avant[coupe..].eq_ignore_ascii_case(mot) {
            continue;
        }
        let devant = &avant[..coupe];
        // Un MOT, pas une fin de mot : « Abcd 2 » ne se lit pas « Ab » + « cd 2 ».
        if devant
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric())
        {
            continue;
        }
        let socle = devant.trim_end_matches(LIAISONS).trim_end();
        // Il doit rester quelque chose devant : « CD1 » seul est un dossier de
        // disque, pas un coffret nommé.
        if socle.is_empty() {
            return None;
        }
        return Some((socle.to_string(), numero));
    }
    None
}

/// « Titre (CD 2) », « Titre [Disc 2] » — le marqueur tient TOUT le contenu
/// des parenthèses ou crochets de fin. « Titre (Live CD 2) » n'en est pas un :
/// c'est [`super::numero_de_disque`] qui en juge, et elle est étroite.
fn marqueur_entre_crochets(n: &str) -> Option<(String, u32)> {
    let ouvrante = match n.chars().next_back()? {
        ')' => '(',
        ']' => '[',
        _ => return None,
    };
    let debut = n.rfind(ouvrante)?;
    let dedans = &n[debut + ouvrante.len_utf8()..n.len() - 1];
    let numero = super::numero_de_disque(dedans)?;
    if numero > 99 {
        return None;
    }
    let socle = n[..debut].trim_end_matches(LIAISONS).trim_end();
    if socle.is_empty() {
        return None;
    }
    Some((socle.to_string(), numero))
}

/// La clé de regroupement d'un socle : sans casse, sans espaces doublés, et
/// sans l'ANNÉE que le rangement met en tête des dossiers.
///
/// 🔴 Mesuré sur le .18 : *Early Works, Disc 1* vit dans
/// `…/Laurent Garnier/1999-Early Works, Disc 1`, *Disc 2* dans
/// `…/2001-Early Works, Disc 2`. Les titres s'accordent, les dossiers non.
/// Mesuré aussi : « Live At The It Club CD1 » / « Live at the It Club CD2 »,
/// « Ummagumma  CD1 » (deux espaces) / « Ummagumma CD2 ».
fn cle_de_socle(socle: &str) -> String {
    let bas = socle.to_lowercase();
    let mut s = bas.trim();
    let tete: Vec<char> = s.chars().take(4).collect();
    if tete.len() == 4 && tete.iter().all(|c| c.is_ascii_digit()) {
        // Quatre chiffres ASCII : l'indice 4 est une frontière de caractère.
        let reste = s[4..].trim_start();
        if let Some(r) = reste.strip_prefix(['-', '_', '.', '\u{2013}', '\u{2014}']) {
            let r = r.trim_start();
            if !r.is_empty() {
                s = r;
            }
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// Le marqueur d'un album : celui de son TITRE d'abord, celui de son DOSSIER
/// à défaut.
///
/// Le titre d'abord, parce que c'est lui que le ripper a écrit pareil sur
/// tous les disques (*Early Works* : titres d'accord, dossiers préfixés
/// d'années différentes). Le dossier à défaut, parce qu'un coffret déjà réuni
/// porte un titre SANS marqueur (« Early Works ») mais vit toujours dans le
/// dossier de son disque 1 — c'est ce qui permet d'y rattacher un disque
/// arrivé plus tard.
fn marqueur_de_l_album(a: &AlbumAGrouper, feuille: &str) -> Option<(String, u32)> {
    marqueur_final(&a.titre).or_else(|| marqueur_final(feuille))
}

/// Les coffrets ÉCLATÉS d'un lot d'albums.
///
/// # La règle — étroite, comme [`super::numero_de_disque`]
///
/// Des albums sont les disques d'un même coffret quand ils sont, TOUS :
///
/// 1. **frères** — même dossier parent. C'est ce qui empêche de rapprocher
///    deux *Greatest Hits, Disc 1* de deux artistes rangés ailleurs ;
/// 2. **du même socle** — même titre une fois le marqueur (« Disc N »,
///    « CDN », « (CD N) », « [Disc N] »…) retiré, sans casse, sans espaces
///    doublés, sans année de tête ;
/// 3. **du même artiste d'album** — un disque classé « Various Artists » ne
///    départage pas (*A Love Supreme*), deux artistes réels différents si ;
/// 4. **de numéros DISTINCTS** — deux « Disc 1 » sous un même socle sont deux
///    extractions, pas deux disques : le groupe entier est laissé tel quel,
///    plutôt que de choisir au hasard lequel garder.
///
/// Un socle qui ne rassemble qu'un seul disque n'est pas un coffret : c'est un
/// disque isolé (« Live at Tonic, Disc 2 » sans son disque 1), et on n'y
/// touche pas. Le modèle d'un coffret est un album qui en absorbe d'autres ;
/// un album seul n'a rien à absorber, et le renommer effacerait la seule
/// trace qu'il lui manque des disques.
pub fn coffrets(albums: &[AlbumAGrouper]) -> Vec<Coffret> {
    let mut par_socle: BTreeMap<(String, String), Vec<(u32, &AlbumAGrouper)>> = BTreeMap::new();
    for a in albums {
        let (parent, feuille) = parent_et_feuille(&a.dossier);
        let Some((socle, numero)) = marqueur_de_l_album(a, &feuille) else {
            continue;
        };
        par_socle
            .entry((parent, cle_de_socle(&socle)))
            .or_default()
            .push((numero, a));
    }
    par_socle
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .filter(|(_, v)| {
            let numeros: std::collections::BTreeSet<u32> = v.iter().map(|(n, _)| *n).collect();
            numeros.len() == v.len()
        })
        .filter(|(_, v)| {
            let artistes: std::collections::BTreeSet<i64> = v
                .iter()
                .filter(|(_, a)| !a.artiste_de_compilation)
                .filter_map(|(_, a)| a.artiste_id)
                .collect();
            artistes.len() <= 1
        })
        .map(|((parent, socle), mut v)| {
            v.sort_by_key(|(n, a)| (*n, a.id));
            // Le titre du coffret vient du TITRE D'ALBUM du premier disque,
            // marqueur retiré — pas du nom de dossier, qui porte souvent
            // l'année et l'artiste en préfixe (« 2008-The Quintessence… »).
            let titre = v
                .first()
                .map(|(_, a)| {
                    marqueur_final(&a.titre)
                        .map(|(s, _)| s)
                        .unwrap_or_else(|| a.titre.clone())
                })
                .unwrap_or_default();
            Coffret {
                cle: format!("{parent}\u{1f}{socle}"),
                parent,
                titre,
                disques: v.into_iter().map(|(n, a)| (n, a.id)).collect(),
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
            artiste_id: Some(1),
            artiste_de_compilation: false,
        }
    }

    fn alb_de(id: i64, titre: &str, dossier: &str, artiste: i64, va: bool) -> AlbumAGrouper {
        AlbumAGrouper {
            artiste_id: Some(artiste),
            artiste_de_compilation: va,
            ..alb(id, titre, dossier)
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
        // Entre parenthèses ou crochets, quand ils ne contiennent QUE le
        // marqueur (GO de Bertrand du 25/09/2026 : « Titre (CD N) »,
        // « Titre [Disc N] »).
        assert_eq!(marqueur_final("Pulse (CD 2)"), Some(("Pulse".into(), 2)));
        assert_eq!(
            marqueur_final("Casino Classics [Disc 2]"),
            Some(("Casino Classics".into(), 2))
        );
        assert_eq!(
            marqueur_final("Pulse (Live CD 2)"),
            None,
            "des parenthèses qui contiennent AUTRE CHOSE que le marqueur"
        );
        assert_eq!(
            marqueur_final("Album - Disque 3"),
            Some(("Album".into(), 3))
        );
        assert_eq!(
            marqueur_final("Antologia CD2"),
            Some(("Antologia".into(), 2))
        );
    }

    #[test]
    fn le_mot_de_disque_est_un_mot_entier() {
        // « Abcd 2 » finit par « cd 2 » sans que « cd » soit un mot.
        assert_eq!(marqueur_final("Abcd 2"), None);
        assert_eq!(marqueur_final("Disco 2"), None);
        // Et trois chiffres ne sont pas un numéro de disque.
        assert_eq!(marqueur_final("Pulse CD100"), None);
    }

    /// 🔴 `to_lowercase` allonge « İ » (2 → 3 octets). L'ancien calcul coupait
    /// l'ORIGINAL aux indices de la copie en minuscules : panique, au
    /// démarrage, puisque la passe automatique y tourne.
    #[test]
    fn un_caractere_qui_s_allonge_en_minuscules_ne_panique_pas() {
        assert_eq!(
            marqueur_final("İstanbul İİİ, Disc 2"),
            Some(("İstanbul İİİ".into(), 2))
        );
        assert_eq!(marqueur_final("Été (CD 1)"), Some(("Été".into(), 1)));
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
            alb_de(
                76,
                "A Love Supreme, Disc 1",
                &format!("{base}/1965-A Love Supreme, Disc 1"),
                99,
                true,
            ),
            alb_de(
                62,
                "A Love Supreme, Disc 2",
                &format!("{base}/1965-A Love Supreme, Disc 2"),
                7,
                false,
            ),
        ]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].disques.len(), 2);
    }

    /// …mais deux artistes RÉELS différents, sous un même parent et un même
    /// socle, ne sont pas un coffret. Contre-épreuve du précédent : la seule
    /// différence est le drapeau « Various Artists » du disque 1.
    #[test]
    fn deux_artistes_reels_differents_ne_font_pas_un_coffret() {
        let base = "/m/Compilations";
        let c = coffrets(&[
            alb_de(
                1,
                "Greatest Hits, Disc 1",
                &format!("{base}/Greatest Hits, Disc 1"),
                10,
                false,
            ),
            alb_de(
                2,
                "Greatest Hits, Disc 2",
                &format!("{base}/Greatest Hits, Disc 2"),
                20,
                false,
            ),
        ]);
        assert!(c.is_empty(), "{c:?}");
    }

    /// 🔴 Mesuré sur le .18 : *Early Works* (Laurent Garnier), ids 11049 et
    /// 11048. Les DOSSIERS portent des années différentes — `1999-Early Works,
    /// Disc 1`, `2001-Early Works, Disc 2` — et l'ancienne règle, qui ne
    /// lisait que le dossier, ne les réunissait pas. Les TITRES s'accordent.
    #[test]
    fn early_works_les_titres_s_accordent_quand_les_dossiers_divergent() {
        let base = "/data/music/NEW_FLAC/ELECTRO/Laurent Garnier";
        let c = coffrets(&[
            alb(
                11049,
                "Early Works, Disc 1",
                &format!("{base}/1999-Early Works, Disc 1"),
            ),
            alb(
                11048,
                "Early Works, Disc 2",
                &format!("{base}/2001-Early Works, Disc 2"),
            ),
        ]);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].disques, vec![(1, 11049), (2, 11048)]);
        assert_eq!(c[0].titre, "Early Works");
    }

    /// Mesuré sur le .18 : la casse et les espaces varient d'un disque à
    /// l'autre (« Live At The It Club CD1 » / « Live at the It Club CD2 »,
    /// « Ummagumma  CD1 » / « Ummagumma CD2 »).
    #[test]
    fn la_casse_et_les_espaces_ne_separent_pas() {
        let base = "/m/Pink Floyd";
        let c = coffrets(&[
            alb(1, "Ummagumma  CD1", &format!("{base}/Ummagumma CD1")),
            alb(2, "Ummagumma CD2", &format!("{base}/Ummagumma CD2")),
            alb(3, "Live At The It Club CD1", &format!("{base}/It Club CD1")),
            alb(4, "Live at the It Club CD2", &format!("{base}/It Club CD2")),
        ]);
        assert_eq!(c.len(), 2, "{c:?}");
    }

    /// Mesuré sur le .18 : un « Casino Classics CD 2 » (Monkey Mafia, VA, une
    /// piste) vit dans UN AUTRE parent que les deux disques de Saint Etienne.
    /// Il ne rejoint pas leur coffret.
    #[test]
    fn casino_classics_un_intrus_d_un_autre_parent_reste_dehors() {
        let r = "/data/music/NEW_FLAC/POP-ROCK";
        let c = coffrets(&[
            alb_de(
                11630,
                "Casino Classics, Disc 1",
                &format!("{r}/S/Saint Etienne/1996-Casino Classics, Disc 1"),
                5,
                false,
            ),
            alb_de(
                11631,
                "Casino Classics, Disc 2",
                &format!("{r}/S/Saint Etienne/1996-Casino Classics, Disc 2"),
                5,
                false,
            ),
            alb_de(
                11403,
                "Casino Classics CD2",
                &format!("{r}/M/Monkey Mafia/1996-Casino Classics CD 2"),
                99,
                true,
            ),
        ]);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].disques, vec![(1, 11630), (2, 11631)]);
    }

    /// Deux « Disc 1 » sous un même socle : deux EXTRACTIONS, pas deux
    /// disques. On ne choisit pas au hasard — le groupe reste tel quel.
    #[test]
    fn deux_disques_du_meme_numero_laissent_le_groupe_intact() {
        let base = "/m/X";
        let c = coffrets(&[
            alb(1, "Boite, Disc 1", &format!("{base}/Boite, Disc 1")),
            alb(2, "Boite (CD 1)", &format!("{base}/Boite (CD 1) 24-96")),
            alb(3, "Boite, Disc 2", &format!("{base}/Boite, Disc 2")),
        ]);
        assert!(c.is_empty(), "{c:?}");
    }

    /// « Live at Tonic, Disc 2 » SEUL : pas de coffret à un disque.
    #[test]
    fn un_disque_2_sans_disque_1_ne_fait_pas_de_coffret() {
        let c = coffrets(&[alb(
            10881,
            "Live at Tonic, Disc 2",
            "/m/Christian McBride/2006-Live at Tonic, Disc 2",
        )]);
        assert!(c.is_empty());
    }

    /// Un coffret DÉJÀ réuni porte un titre sans marqueur, mais vit dans le
    /// dossier de son disque 1 : un disque arrivé plus tard s'y rattache.
    #[test]
    fn un_coffret_reuni_accueille_un_disque_arrive_plus_tard() {
        let base = "/m/McBride";
        let c = coffrets(&[
            alb(
                10882,
                "Live at Tonic",
                &format!("{base}/2006-Live at Tonic, Disc 1"),
            ),
            alb(
                10883,
                "Live at Tonic, Disc 3",
                &format!("{base}/2006-Live at Tonic, Disc 3"),
            ),
        ]);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].cible(), Some(10882));
        assert_eq!(c[0].titre, "Live at Tonic");
    }

    #[test]
    fn la_cle_est_stable_et_ignore_l_annee_de_tete() {
        assert_eq!(cle_de_socle("1999-Early Works"), "early works");
        assert_eq!(cle_de_socle("2001 - Early  Works"), "early works");
        // Une année SEULE est un titre (« 1999 » de Prince), pas un préfixe.
        assert_eq!(cle_de_socle("1999"), "1999");
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
