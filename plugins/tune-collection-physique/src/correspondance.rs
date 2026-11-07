//! Rapprocher un disque de la collection de la bibliothèque et des services.
//!
//! # Dans la bibliothèque (cadrage de #7195)
//!
//! Dans cet ordre, le premier qui répond l'emporte :
//!
//! 1. **MusicBrainz** : les identifiants Discogs reliés dans MusicBrainz — le
//!    pressage (release) ou l'album (groupe de parutions, par le master
//!    Discogs) — contre `musicbrainz_release_id` /
//!    `musicbrainz_release_group_id` de l'album local. C'est aussi ainsi qu'un
//!    CD extrait se retrouve : l'extraction le balise avec le pressage que
//!    l'identifiant MusicBrainz du disque a désigné.
//! 2. **Code-barre** : celui de la fiche Discogs contre celui de l'album.
//! 3. **Artiste + titre normalisés**, comme les albums manquants (#6288) :
//!    minuscules, sans accents ni ponctuation, sans les mentions entre
//!    parenthèses (« (Remastered) »). L'artiste ET le titre doivent
//!    concorder. Faute d'album au titre identique, la clé assouplie de
//!    #7981 ([`cle_de_titre`], [`cles_d_un_artiste`]) : sans la mention
//!    d'édition après « - » ou « : » (« Abbey Road - Remastered 2009 »),
//!    sans « The » en tête du titre, « & » et « and » équivalents,
//!    « Beatles, The » lu « The Beatles », un artiste commun suffisant
//!    (« Miles Davis; John Coltrane »). Si cette clé désigne PLUSIEURS albums
//!    distincts, on ne choisit pas ([`Rapprochement::Plusieurs`]).
//!
//! Seuls les albums LOCAUX comptent (`source = local`) : un favori de service
//! n'est pas « dans ma bibliothèque ».
//!
//! # En streaming
//!
//! Qobuz et TIDAL, par code-barre (la recherche du service avec le code),
//! sinon par artiste + titre. Un album de service n'est retenu que si son
//! TITRE normalisé est celui du disque (ou sa clé assouplie) ; par artiste +
//! titre, son artiste aussi. Par code-barre, un titre qui CONTIENT celui du
//! disque suffit (« Abbey Road (Super Deluxe Edition) 2019 Mix ») : le code
//! a déjà désigné l'album. Mieux vaut « nulle part » qu'un mauvais album.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tune_core::metadata::albums_manquants::titre_normalise;

use crate::discogs::{Disque, code_barre_normalise};

/// Un album local, tel que la comparaison le lit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumLocal {
    pub id: i64,
    pub titre: String,
    pub artiste: Option<String>,
    pub code_barre: Option<String>,
    pub mb_release: Option<String>,
    pub mb_groupe: Option<String>,
    /// Le badge de qualité de l'album (`hi-res`, `cd`, `dsd`, `lossy`).
    pub qualite: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    /// La pochette de l'album (`albums.cover_path`), telle que les routes de
    /// la bibliothèque la rendent : l'écran la sert par `/library/artwork`.
    pub pochette: Option<String>,
}

/// Les identifiants MusicBrainz reliés à un disque Discogs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiensMusicBrainz {
    pub releases: Vec<String>,
    pub groupes: Vec<String>,
}

/// Comment un rapprochement a été fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Par {
    MusicbrainzPressage,
    MusicbrainzAlbum,
    CodeBarre,
    ArtisteTitre,
}

/// La clé d'artiste : « The Beatles » et « Beatles » se rejoignent, ainsi que
/// « Various » (Discogs) et « Various Artists ».
pub fn artiste_normalise(nom: &str) -> String {
    let n = titre_normalise(nom);
    let n = n.strip_prefix("the ").map(str::to_string).unwrap_or(n);
    match n.as_str() {
        "various" | "various artists" | "artistes divers" | "divers" => "various".to_string(),
        _ => n,
    }
}

/// Les mots qui, après « - » ou « : », font d'un bout de titre une mention
/// d'édition (« Abbey Road - Remastered 2009 », « Rumours: Super Deluxe »).
const MOTS_D_EDITION: &[&str] = &[
    "remaster",
    "remastered",
    "remasterise",
    "remasterisee",
    "remasterises",
    "deluxe",
    "edition",
    "anniversary",
    "anniversaire",
    "expanded",
    "reissue",
    "version",
    "mix",
    "mono",
    "stereo",
    "bonus",
];

/// En fin de titre SANS séparateur, seuls ces mots marquent une édition
/// (« Abbey Road Remastered ») : « Dub Version » ou « Mono » peuvent être
/// le titre même.
const MOTS_D_EDITION_EN_FIN: &[&str] = &[
    "remaster",
    "remastered",
    "remasterise",
    "remasterisee",
    "deluxe",
    "edition",
    "anniversary",
    "anniversaire",
    "expanded",
    "reissue",
];

/// Les mots qui accompagnent une mention d'édition en fin de titre
/// (« Super Deluxe Edition 2011 », « 30th Anniversary »).
const MOTS_D_ACCOMPAGNEMENT: &[&str] = &[
    "super",
    "legacy",
    "collector",
    "collectors",
    "special",
    "limited",
    "edt",
    "ed",
];

fn est_annee_ou_rang(mot: &str) -> bool {
    let chiffres: String = mot.chars().take_while(char::is_ascii_digit).collect();
    let reste = &mot[chiffres.len()..];
    !chiffres.is_empty()
        && ((chiffres.len() == 4 && reste.is_empty())
            || matches!(reste, "th" | "st" | "nd" | "rd" | "e" | "eme" | "er"))
}

/// Le titre sans sa mention d'édition après un séparateur (« - », « – »,
/// « — », « : ») : « Abbey Road - Remastered 2009 » → « Abbey Road ». Un
/// séparateur suivi d'autre chose (« The Haçienda - Play By 01/96 ») reste.
pub fn titre_sans_edition(titre: &str) -> &str {
    let mut debut = 0;
    while let Some((i, sep)) = [" - ", " – ", " — ", ": "]
        .iter()
        .filter_map(|sep| titre[debut..].find(sep).map(|i| (debut + i, *sep)))
        .min_by_key(|(i, _)| *i)
    {
        let reste = titre_normalise(&titre[i + sep.len()..]);
        let tete = titre[..i].trim();
        if !tete.is_empty() && reste.split(' ').any(|m| MOTS_D_EDITION.contains(&m)) {
            return tete;
        }
        debut = i + sep.len();
    }
    titre
}

/// La clé de titre assouplie (#7981) : [`titre_sans_edition`], normalisé,
/// sans mention d'édition en fin (« … Remastered 2009 ») ni « The » en tête
/// (« The Wall » et « Wall »). Propre au greffon : `titre_normalise` de
/// tune-core sert aussi aux albums manquants et n'est pas touché.
pub fn cle_de_titre(titre: &str) -> String {
    let n = titre_normalise(titre_sans_edition(titre));
    let mut mots: Vec<&str> = n.split(' ').filter(|m| !m.is_empty()).collect();
    // La fin : une suite de mots d'édition, d'accompagnement, d'années ou de
    // rangs, qui contient au moins un vrai mot d'édition.
    let mut coupe = mots.len();
    let mut edition = false;
    while coupe > 1 {
        let m = mots[coupe - 1];
        if MOTS_D_EDITION_EN_FIN.contains(&m) {
            edition = true;
        } else if !(MOTS_D_ACCOMPAGNEMENT.contains(&m) || est_annee_ou_rang(m)) {
            break;
        }
        coupe -= 1;
    }
    if edition {
        mots.truncate(coupe);
    }
    if mots.len() > 1 && mots[0] == "the" {
        mots.remove(0);
    }
    mots.join(" ")
}

/// Les clés d'UN nom d'artiste (#7981) : le nom entier, puis chacun des
/// artistes qu'il joint (« ; », « / », « feat. », « ft. », « featuring »,
/// « with », « & The » / « And The » d'un groupe d'accompagnement). Chaque
/// clé suit [`artiste_normalise`], « Beatles, The » lu « The Beatles », et
/// « and » / « et » ignorés (« Simon & Garfunkel » = « Simon and
/// Garfunkel »).
pub fn cles_d_un_artiste(nom: &str) -> HashSet<String> {
    fn cle(nom: &str) -> String {
        let nom = nom.trim();
        let nom = match nom.len().checked_sub(5) {
            Some(i) if nom.is_char_boundary(i) && nom[i..].eq_ignore_ascii_case(", the") => {
                format!("The {}", &nom[..i])
            }
            _ => nom.to_string(),
        };
        artiste_normalise(&nom)
            .split(' ')
            .filter(|m| !matches!(*m, "and" | "et"))
            .collect::<Vec<_>>()
            .join(" ")
    }
    let bas = nom.to_lowercase();
    let mut cles: HashSet<String> = HashSet::new();
    cles.insert(cle(&bas));
    let mut parties = vec![bas.clone()];
    for sep in [
        ";",
        " / ",
        " feat. ",
        " feat ",
        " ft. ",
        " featuring ",
        " with ",
        " & the ",
        " and the ",
    ] {
        parties = parties
            .iter()
            .flat_map(|p| p.split(sep).map(str::to_string).collect::<Vec<_>>())
            .collect();
    }
    if parties.len() > 1 {
        cles.extend(parties.iter().map(|p| cle(p)));
    }
    cles.retain(|k| !k.is_empty());
    cles
}

fn mbid(m: &str) -> String {
    m.trim().to_ascii_lowercase()
}

/// L'index de la bibliothèque, construit une fois par requête.
pub struct Index<'a> {
    albums: &'a [AlbumLocal],
    par_release: HashMap<String, usize>,
    par_groupe: HashMap<String, usize>,
    par_code: HashMap<String, usize>,
    par_titre: HashMap<String, Vec<usize>>,
    /// Par [`cle_de_titre`] (#7981).
    par_cle: HashMap<String, Vec<usize>>,
}

/// Ce que la bibliothèque répond pour un disque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rapprochement<'a> {
    Trouve(&'a AlbumLocal, Par),
    /// Aucun album local ne répond.
    Aucun,
    /// La clé assouplie désigne plusieurs albums DISTINCTS (tant) : on ne
    /// choisit pas.
    Plusieurs(usize),
}

impl<'a> Rapprochement<'a> {
    pub fn trouve(self) -> Option<(&'a AlbumLocal, Par)> {
        match self {
            Rapprochement::Trouve(a, p) => Some((a, p)),
            _ => None,
        }
    }
}

impl<'a> Index<'a> {
    pub fn new(albums: &'a [AlbumLocal]) -> Self {
        let mut par_release = HashMap::new();
        let mut par_groupe = HashMap::new();
        let mut par_code = HashMap::new();
        let mut par_titre: HashMap<String, Vec<usize>> = HashMap::new();
        let mut par_cle: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, a) in albums.iter().enumerate() {
            if let Some(r) = a.mb_release.as_deref().map(mbid).filter(|r| !r.is_empty()) {
                par_release.entry(r).or_insert(i);
            }
            if let Some(g) = a.mb_groupe.as_deref().map(mbid).filter(|g| !g.is_empty()) {
                par_groupe.entry(g).or_insert(i);
            }
            if let Some(c) = a.code_barre.as_deref().and_then(code_barre_normalise) {
                par_code.entry(c).or_insert(i);
            }
            let t = titre_normalise(&a.titre);
            if !t.is_empty() {
                par_titre.entry(t).or_default().push(i);
            }
            let k = cle_de_titre(&a.titre);
            if !k.is_empty() {
                par_cle.entry(k).or_default().push(i);
            }
        }
        Self {
            albums,
            par_release,
            par_groupe,
            par_code,
            par_titre,
            par_cle,
        }
    }

    /// L'album local du disque, et comment il a été trouvé.
    pub fn trouver(
        &self,
        d: &Disque,
        codes: &[String],
        mb: Option<&LiensMusicBrainz>,
    ) -> Option<(&'a AlbumLocal, Par)> {
        self.rapprocher(d, codes, mb).trouve()
    }

    /// Ce que la bibliothèque répond pour le disque : l'album et comment il
    /// a été trouvé, aucun, ou plusieurs albums qu'on ne départage pas.
    pub fn rapprocher(
        &self,
        d: &Disque,
        codes: &[String],
        mb: Option<&LiensMusicBrainz>,
    ) -> Rapprochement<'a> {
        if let Some(mb) = mb {
            for r in &mb.releases {
                if let Some(&i) = self.par_release.get(&mbid(r)) {
                    return Rapprochement::Trouve(&self.albums[i], Par::MusicbrainzPressage);
                }
            }
            for g in &mb.groupes {
                if let Some(&i) = self.par_groupe.get(&mbid(g)) {
                    return Rapprochement::Trouve(&self.albums[i], Par::MusicbrainzAlbum);
                }
            }
        }
        for c in codes.iter().filter_map(|c| code_barre_normalise(c)) {
            if let Some(&i) = self.par_code.get(&c) {
                return Rapprochement::Trouve(&self.albums[i], Par::CodeBarre);
            }
        }
        let artistes = cles_d_artiste(d);
        if artistes.is_empty() {
            return Rapprochement::Aucun;
        }
        let meme_artiste = |a: &AlbumLocal| {
            a.artiste
                .as_deref()
                .is_some_and(|n| !cles_d_un_artiste(n).is_disjoint(&artistes))
        };
        // Le titre identique d'abord : le premier album qui répond, comme
        // avant #7981 (un même album importé deux fois n'est pas ambigu).
        let t = titre_normalise(&d.titre);
        if !t.is_empty() {
            for &i in self.par_titre.get(&t).into_iter().flatten() {
                if meme_artiste(&self.albums[i]) {
                    return Rapprochement::Trouve(&self.albums[i], Par::ArtisteTitre);
                }
            }
        }
        // Puis la clé assouplie : un seul album distinct, ou on ne choisit pas.
        let k = cle_de_titre(&d.titre);
        if k.is_empty() {
            return Rapprochement::Aucun;
        }
        let mut retenus: Vec<usize> = Vec::new();
        let mut distincts: HashSet<(String, String)> = HashSet::new();
        for &i in self.par_cle.get(&k).into_iter().flatten() {
            let a = &self.albums[i];
            if meme_artiste(a) {
                retenus.push(i);
                distincts.insert((
                    titre_normalise(&a.titre),
                    a.artiste
                        .as_deref()
                        .map(artiste_normalise)
                        .unwrap_or_default(),
                ));
            }
        }
        match (retenus.first(), distincts.len()) {
            (None, _) => Rapprochement::Aucun,
            (Some(&i), 1) => Rapprochement::Trouve(&self.albums[i], Par::ArtisteTitre),
            (Some(_), n) => Rapprochement::Plusieurs(n),
        }
    }
}

/// Les clés d'artiste d'un disque : l'affichage complet, et chacun de ses
/// artistes (« Miles Davis Quintet » ou « Miles Davis » côté bibliothèque).
pub fn cles_d_artiste(d: &Disque) -> HashSet<String> {
    let mut cles: HashSet<String> = d.artistes.iter().map(|a| artiste_normalise(a)).collect();
    cles.insert(artiste_normalise(&d.artiste));
    for a in d.artistes.iter().chain(std::iter::once(&d.artiste)) {
        cles.extend(cles_d_un_artiste(a));
    }
    cles.retain(|k| !k.is_empty());
    cles
}

/// Un album proposé par un service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumDeService {
    pub id: String,
    pub titre: String,
    pub artiste: String,
}

/// L'album d'un service retenu pour un disque, parmi les résultats d'une
/// recherche. Par code-barre, le titre suffit (le code a déjà désigné le
/// disque ; l'artiste s'écrit souvent autrement), et un titre qui contient
/// celui du disque aussi (« … - Remastered », « … OKNOTOK 1997 2017 ») ; par
/// artiste + titre, les deux doivent concorder. Le titre identique passe
/// avant la clé assouplie ([`cle_de_titre`]).
pub fn choisir_sur_le_service<'a>(
    d: &Disque,
    candidats: &'a [AlbumDeService],
    par_code_barre: bool,
) -> Option<&'a AlbumDeService> {
    let t = titre_normalise(&d.titre);
    if t.is_empty() {
        return None;
    }
    let k = cle_de_titre(&d.titre);
    let artistes = cles_d_artiste(d);
    let artiste_ok = |c: &AlbumDeService| {
        par_code_barre || !cles_d_un_artiste(&c.artiste).is_disjoint(&artistes)
    };
    // `petit` en tête de `grand`, ou ailleurs s'il fait au moins deux mots :
    // un mot seul (« Love ») se retrouve dans trop de titres.
    let contient = |grand: &str, petit: &str| {
        !petit.is_empty()
            && (grand.starts_with(&format!("{petit} "))
                || (petit.contains(' ') && format!(" {grand} ").contains(&format!(" {petit} "))))
    };
    candidats
        .iter()
        .find(|c| titre_normalise(&c.titre) == t && artiste_ok(c))
        .or_else(|| {
            candidats
                .iter()
                .find(|c| !k.is_empty() && cle_de_titre(&c.titre) == k && artiste_ok(c))
        })
        .or_else(|| {
            if !par_code_barre {
                return None;
            }
            candidats.iter().find(|c| {
                let kc = cle_de_titre(&c.titre);
                contient(&kc, &k) || contient(&k, &kc)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discogs::Famille;

    fn disque(artiste: &str, titre: &str) -> Disque {
        Disque {
            instance_id: 1,
            release_id: 10,
            master_id: None,
            artistes: vec![artiste.to_string()],
            artiste: artiste.to_string(),
            titre: titre.to_string(),
            annee: None,
            labels: vec![],
            numeros_catalogue: vec![],
            formats: vec!["Vinyl".into()],
            precisions: vec![],
            famille: Famille::Vinyle,
            ajoute_le: None,
            vignette: None,
            pochette: None,
            dossier: None,
            genres: vec![],
            styles: vec![],
        }
    }

    fn album(id: i64, artiste: &str, titre: &str) -> AlbumLocal {
        AlbumLocal {
            id,
            titre: titre.into(),
            artiste: Some(artiste.into()),
            ..Default::default()
        }
    }

    #[test]
    fn artiste_et_titre_doivent_concorder_tous_les_deux() {
        let albums = vec![
            album(1, "Miles Davis", "Kind of Blue (Legacy Edition)"),
            album(2, "Bill Evans", "Kind of Blue"),
        ];
        let idx = Index::new(&albums);
        let (a, par) = idx
            .trouver(&disque("Miles Davis", "Kind Of Blue"), &[], None)
            .unwrap();
        assert_eq!((a.id, par), (1, Par::ArtisteTitre));
        assert!(
            idx.trouver(&disque("John Coltrane", "Kind of Blue"), &[], None)
                .is_none(),
            "même titre, autre artiste : pas le même disque"
        );
    }

    #[test]
    fn musicbrainz_passe_avant_le_code_barre_et_le_titre() {
        let mut a1 = album(1, "X", "Autre titre");
        a1.mb_release = Some("AAAAAAAA-0000-0000-0000-000000000001".into());
        let mut a2 = album(2, "X", "Titre");
        a2.code_barre = Some("0 12345 67890 5".into());
        let albums = vec![a1, a2];
        let idx = Index::new(&albums);
        let mb = LiensMusicBrainz {
            releases: vec!["aaaaaaaa-0000-0000-0000-000000000001".into()],
            groupes: vec![],
        };
        let d = disque("X", "Titre");
        let codes = vec!["012345678905".to_string()];
        assert_eq!(
            idx.trouver(&d, &codes, Some(&mb)).map(|(a, p)| (a.id, p)),
            Some((1, Par::MusicbrainzPressage))
        );
        assert_eq!(
            idx.trouver(&d, &codes, None).map(|(a, p)| (a.id, p)),
            Some((2, Par::CodeBarre))
        );
    }

    #[test]
    fn the_et_various_se_rejoignent() {
        assert_eq!(
            artiste_normalise("The Beatles"),
            artiste_normalise("Beatles")
        );
        assert_eq!(
            artiste_normalise("Various"),
            artiste_normalise("Various Artists")
        );
    }

    #[test]
    fn un_service_n_est_retenu_que_sur_un_titre_identique() {
        let d = disque("Miles Davis", "Kind of Blue");
        let c = vec![
            AlbumDeService {
                id: "a".into(),
                titre: "Kind of Blue Live".into(),
                artiste: "Miles Davis".into(),
            },
            AlbumDeService {
                id: "b".into(),
                titre: "Kind Of Blue (Remastered)".into(),
                artiste: "Miles Davis".into(),
            },
        ];
        assert_eq!(
            choisir_sur_le_service(&d, &c, false).map(|a| a.id.as_str()),
            Some("b")
        );
        let autre = vec![AlbumDeService {
            id: "c".into(),
            titre: "Kind of Blue".into(),
            artiste: "Tribute Band".into(),
        }];
        assert!(choisir_sur_le_service(&d, &autre, false).is_none());
        assert_eq!(
            choisir_sur_le_service(&d, &autre, true).map(|a| a.id.as_str()),
            Some("c"),
            "par code-barre, le titre suffit"
        );
    }
    /// #7981 : les disques que la comparaison stricte laissait « nulle
    /// part » alors que l'album était dans la bibliothèque, sous une autre
    /// écriture. Chaque ligne : le disque Discogs, l'album local.
    #[test]
    fn repli_artiste_titre_assoupli_7981() {
        let cas = [
            (
                ("The Beatles", "Abbey Road - Remastered 2009"),
                ("The Beatles", "Abbey Road"),
            ),
            (
                ("Fleetwood Mac", "Rumours: Super Deluxe"),
                ("Fleetwood Mac", "Rumours"),
            ),
            (
                ("Nirvana", "Nevermind - 30th Anniversary Super Deluxe"),
                ("Nirvana", "Nevermind"),
            ),
            (("Pink Floyd", "Wall"), ("Pink Floyd", "The Wall")),
            (
                ("Simon & Garfunkel", "Bookends"),
                ("Simon and Garfunkel", "Bookends"),
            ),
            (
                ("Miles Davis", "Kind of Blue"),
                ("Miles Davis; John Coltrane", "Kind of Blue"),
            ),
            (
                ("Miles Davis", "Kind of Blue"),
                ("Miles Davis feat. John Coltrane", "Kind of Blue"),
            ),
            (
                ("Prince And The Revolution", "Purple Rain"),
                ("Prince", "Purple Rain"),
            ),
            (("Beatles, The", "Revolver"), ("The Beatles", "Revolver")),
            (
                ("Radiohead", "OK Computer Remastered"),
                ("Radiohead", "OK Computer"),
            ),
        ];
        for ((da, dt), (la, lt)) in cas {
            let albums = vec![album(7, la, lt)];
            let idx = Index::new(&albums);
            assert_eq!(
                idx.rapprocher(&disque(da, dt), &[], None),
                Rapprochement::Trouve(&albums[0], Par::ArtisteTitre),
                "« {da} — {dt} » devait retrouver « {la} — {lt} »"
            );
        }
    }

    /// Contre-épreuve du repli : ce qui n'est pas le même disque reste à part.
    #[test]
    fn le_repli_assoupli_ne_confond_pas_deux_disques_7981() {
        let albums = vec![
            album(1, "Miles Davis", "Kind of Blue Live"),
            album(2, "Bill Evans", "Abbey Road"),
            album(3, "Various", "The Haçienda"),
        ];
        let idx = Index::new(&albums);
        for (a, t) in [
            ("Miles Davis", "Kind of Blue"),
            ("The Beatles", "Abbey Road - Remastered 2009"),
            ("Various", "The Haçienda - Play By 01/96"),
        ] {
            assert_eq!(
                idx.rapprocher(&disque(a, t), &[], None),
                Rapprochement::Aucun,
                "« {a} — {t} » ne devait rien retrouver"
            );
        }
    }

    /// Deux albums locaux distincts répondent à la clé assouplie : on ne
    /// choisit pas. Le même album importé deux fois, si.
    #[test]
    fn plusieurs_candidats_distincts_on_ne_choisit_pas_7981() {
        let albums = vec![
            album(1, "Fleetwood Mac", "Rumours (Deluxe)"),
            album(2, "Fleetwood Mac", "Rumours Super Deluxe Edition"),
        ];
        let idx = Index::new(&albums);
        let d = disque("Fleetwood Mac", "Rumours: 35th Anniversary Edition");
        assert_eq!(idx.rapprocher(&d, &[], None), Rapprochement::Plusieurs(2));
        assert!(idx.trouver(&d, &[], None).is_none());

        let doublon = vec![
            album(1, "Fleetwood Mac", "Rumours - Remastered"),
            album(2, "Fleetwood Mac", "Rumours - Remastered"),
        ];
        let idx = Index::new(&doublon);
        assert_eq!(
            idx.trouver(&d, &[], None).map(|(a, p)| (a.id, p)),
            Some((1, Par::ArtisteTitre))
        );
        // Le titre identique passe avant : pas d'ambiguïté.
        let exact = vec![
            album(1, "Fleetwood Mac", "Rumours Super Deluxe Edition"),
            album(2, "Fleetwood Mac", "Rumours"),
        ];
        let idx = Index::new(&exact);
        assert_eq!(
            idx.trouver(&disque("Fleetwood Mac", "Rumours"), &[], None)
                .map(|(a, _)| a.id),
            Some(2)
        );
    }

    #[test]
    fn la_mention_d_edition_se_coupe_seulement_si_c_en_est_une_7981() {
        assert_eq!(
            titre_sans_edition("Abbey Road - Remastered 2009"),
            "Abbey Road"
        );
        assert_eq!(titre_sans_edition("Rumours: Super Deluxe"), "Rumours");
        assert_eq!(
            titre_sans_edition("The Haçienda - Play By 01/96"),
            "The Haçienda - Play By 01/96"
        );
        assert_eq!(cle_de_titre("Abbey Road (Remastered 2009)"), "abbey road");
        assert_eq!(cle_de_titre("Live 1975"), "live 1975");
        assert_eq!(cle_de_titre("The The"), "the");
        assert_eq!(cle_de_titre("Dub Version"), "dub version");
    }

    /// #7981, côté service : par code-barre, un titre qui CONTIENT celui du
    /// disque ; par artiste + titre, la clé assouplie.
    #[test]
    fn un_service_retrouve_le_disque_sous_une_autre_ecriture_7981() {
        let d = disque("The Beatles", "Abbey Road");
        let qobuz = vec![AlbumDeService {
            id: "q".into(),
            titre: "Abbey Road (Super Deluxe Edition) 2019 Mix".into(),
            artiste: "The Beatles".into(),
        }];
        assert_eq!(
            choisir_sur_le_service(&d, &qobuz, true).map(|a| a.id.as_str()),
            Some("q"),
            "par code-barre, le titre qui contient celui du disque suffit"
        );
        assert!(
            choisir_sur_le_service(&d, &qobuz, false).is_none(),
            "par artiste + titre, « 2019 Mix » n'est pas une mention d'édition seule"
        );
        let d = disque("Simon & Garfunkel", "Bookends - Remastered");
        let qobuz = vec![AlbumDeService {
            id: "b".into(),
            titre: "Bookends".into(),
            artiste: "Simon and Garfunkel".into(),
        }];
        assert_eq!(
            choisir_sur_le_service(&d, &qobuz, false).map(|a| a.id.as_str()),
            Some("b")
        );
        // Un mot seul contenu dans un autre titre ne suffit pas.
        let d = disque("X", "Love");
        let autre = vec![AlbumDeService {
            id: "z".into(),
            titre: "Greatest Love Songs".into(),
            artiste: "Y".into(),
        }];
        assert!(choisir_sur_le_service(&d, &autre, true).is_none());
    }
}
