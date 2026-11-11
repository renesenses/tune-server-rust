//! Biographies cliquables (#7910).
//!
//! Une biographie d'artiste ou d'album est rendue au client en **segments
//! structurés** : du texte, du texte qui ouvre une page de Tune (lien interne),
//! ou du texte qui ouvre une page de la source (lien externe). Le client les
//! dessine avec des composants : aucun HTML venu d'une source n'est jamais
//! injecté côté web.
//!
//! - Liens internes, prioritaires : un artiste, un album ou un label (#8150)
//!   de la bibliothèque cité dans le texte. Correspondance exacte, insensible
//!   à la casse et aux accents, sur des mots entiers ; un album se reconnaît
//!   aussi à son titre sans mentions d'édition ni de disque (#8150). Les noms
//!   courts, les mots courants, les nombres seuls et les noms portés par
//!   plusieurs entités sont écartés — mais les exemplaires d'un même album
//!   d'un même artiste ne comptent que pour un (#8150). Un seul lien par
//!   entité ; l'entité de la page (et, sur un album, son artiste) n'est
//!   jamais liée.
//! - Liens externes : ceux du HTML d'origine (Last.fm), gardés au lieu d'être
//!   effacés, mais seulement en `https` et sur les domaines des sources
//!   ([`DOMAINES_DES_SOURCES`]). Tout le reste redevient du texte.
//!
//! Le champ `bio` en texte brut ne change pas : les anciens clients le lisent
//! comme avant. Les liens externes sont rangés à côté de la bio, avec
//! l'empreinte du texte qu'ils décrivent : une bio réécrite (à la main, par le
//! proxy communautaire, par un autre enrichissement) ne reprend jamais les
//! liens d'un autre texte.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization as _;
use unicode_normalization::char::is_combining_mark;

use crate::db::backend::DbBackend;

/// Les domaines des sources de biographies : Wikipédia, Last.fm, Qobuz. Un
/// sous-domaine compte (`fr.wikipedia.org`, `www.last.fm`) ; un domaine qui
/// les imite (`last.fm.exemple.com`, `evil-last.fm`) non.
pub const DOMAINES_DES_SOURCES: [&str; 3] = ["wikipedia.org", "last.fm", "qobuz.com"];

/// Un nom de moins de lettres que ce seuil (espaces non comptés) n'est jamais
/// lié : « U2 », « Air », « Yes » se lisent trop souvent ailleurs.
pub const LETTRES_MIN: usize = 4;

/// Au-delà, un nom n'est plus cherché (borne du coût de la recherche).
const MOTS_MAX: usize = 8;

/// Mots seuls qui sont aussi des noms d'artistes ou d'albums, mais qu'une
/// biographie emploie surtout dans leur sens commun. Forme pliée (minuscules,
/// sans accents).
const MOTS_COURANTS: &[&str] = &[
    "acoustic",
    "after",
    "again",
    "album",
    "albums",
    "amour",
    "anthology",
    "band",
    "bass",
    "before",
    "best",
    "black",
    "blue",
    "blues",
    "bonus",
    "chanson",
    "chansons",
    "chapter",
    "choir",
    "classic",
    "classics",
    "coeur",
    "collection",
    "compilation",
    "concert",
    "debut",
    "deluxe",
    "demo",
    "demos",
    "dream",
    "drums",
    "edition",
    "ensemble",
    "essential",
    "essentials",
    "europe",
    "everything",
    "first",
    "free",
    "from",
    "gold",
    "greatest",
    "groupe",
    "group",
    "guitar",
    "heart",
    "help",
    "hits",
    "home",
    "hiver",
    "intro",
    "jazz",
    "label",
    "life",
    "live",
    "love",
    "monde",
    "more",
    "music",
    "musique",
    "night",
    "nothing",
    "nuit",
    "only",
    "orchestra",
    "orchestre",
    "paris",
    "part",
    "piano",
    "platinum",
    "plus",
    "quartet",
    "record",
    "records",
    "remastered",
    "remix",
    "remixes",
    "rock",
    "rouge",
    "session",
    "sessions",
    "singles",
    "something",
    "songs",
    "soul",
    "still",
    "studio",
    "summer",
    "temps",
    "then",
    "this",
    "time",
    "tour",
    "trio",
    "unknown",
    "unplugged",
    "untitled",
    "voix",
    "volume",
    "when",
    "where",
    "white",
    "with",
    "world",
];

/// Noms génériques jamais liés, quel que soit leur nombre de mots.
const NOMS_GENERIQUES: &[&str] = &[
    "unknown artist",
    "unknown album",
    "various artists",
    "artistes divers",
    "divers artistes",
    "artiste inconnu",
    "album inconnu",
    "sans titre",
    "greatest hits",
    "best of",
    "the best of",
    "the very best of",
    "the collection",
    "the album",
    "love songs",
    "bonus tracks",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LienExterne {
    /// Le texte du lien, tel qu'il figure dans la bio en texte brut.
    pub texte: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GenreDeLien {
    Artiste,
    Album,
}

impl GenreDeLien {
    fn nom(self) -> &'static str {
        match self {
            GenreDeLien::Artiste => "artiste",
            GenreDeLien::Album => "album",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct LienInterne {
    #[serde(rename = "type")]
    pub genre: GenreDeLien,
    pub id: i64,
}

/// #8129 — un lien vers la page d'un artiste ou d'un album CHEZ UN SERVICE
/// (Qobuz), que Tune ouvre dans sa propre fiche de service. `id` est
/// l'identifiant du service, en texte (un album Qobuz a un identifiant
/// alphanumérique).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct LienDeService {
    pub service: String,
    #[serde(rename = "type")]
    pub genre: GenreDeLien,
    pub id: String,
}

/// Un morceau de la bio : du texte seul, du texte avec un lien interne
/// (`lien`), vers un label de la bibliothèque (`label`, #8150), vers une
/// fiche de service (`lien_service`, #8129), ou avec un lien externe (`url`) ;
/// jamais deux à la fois.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BioSegment {
    pub texte: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lien: Option<LienInterne>,
    /// #8150 — le nom du label tel que la bibliothèque l'écrit : la page
    /// `/library/labels/{nom}/artists`. Un label n'a pas d'identifiant en
    /// base ; un champ à part laisse les anciens clients sur du texte.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lien_service: Option<LienDeService>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// L'URL, normalisée, si elle est en `https` sur un domaine de source ; `None`
/// sinon (`javascript:`, `http:`, `data:`, domaine étranger ou imitateur,
/// identifiants ou port dans l'adresse).
pub fn url_autorisee(brute: &str) -> Option<String> {
    let brute = brute.trim();
    if brute.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let url = reqwest::Url::parse(brute).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return None;
    }
    let hote = url.host_str()?.to_ascii_lowercase();
    let hote = hote.trim_end_matches('.');
    let permis = DOMAINES_DES_SOURCES
        .iter()
        .any(|d| hote == *d || hote.ends_with(&format!(".{d}")));
    permis.then(|| url.to_string())
}

/// Les liens `<a href>` d'un HTML de source, filtrés par [`url_autorisee`].
///
/// Le texte d'un lien est pris comme `strip_html` le laisse dans la bio (balises
/// retirées, entités non décodées) : c'est ce texte qu'on retrouvera ensuite
/// dans la bio en texte brut.
pub fn liens_depuis_html(html: &str) -> Vec<LienExterne> {
    let balise = regex::Regex::new(r"<[^>]+>").expect("regex des balises");
    let mut vus = HashSet::new();
    let mut liens = Vec::new();
    for (href, contenu) in ancres(html) {
        let Some(url) = url_autorisee(&href) else {
            continue;
        };
        let texte = balise.replace_all(contenu, "").trim().to_string();
        if texte.is_empty() || !vus.insert(url.clone()) {
            continue;
        }
        liens.push(LienExterne { texte, url });
    }
    liens
}

/// Les ancres `<a href>` d'un HTML de source : (`href` avec `&amp;` décodé,
/// contenu HTML de l'ancre), dans l'ordre du texte.
pub(crate) fn ancres(html: &str) -> Vec<(String, &str)> {
    static ANCRE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?is)<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|'([^']*)')[^>]*>(.*?)</a\s*>"#,
        )
        .expect("regex des ancres")
    });
    ANCRE
        .captures_iter(html)
        .map(|c| {
            let href = c
                .get(1)
                .or_else(|| c.get(2))
                .map(|m| m.as_str())
                .unwrap_or("");
            (
                href.replace("&amp;", "&"),
                c.get(3).map(|m| m.as_str()).unwrap_or(""),
            )
        })
        .collect()
}

/// Minuscules, sans diacritiques : « Björk » et « bjork » se rejoignent.
fn plier(mot: &str) -> String {
    mot.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

struct Mot {
    debut: usize,
    fin: usize,
    plie: String,
}

/// Les mots d'un texte : suites maximales de lettres et de chiffres, avec leur
/// position en octets.
fn mots(texte: &str) -> Vec<Mot> {
    let mut sortie = Vec::new();
    let mut debut: Option<usize> = None;
    for (i, c) in texte.char_indices() {
        let dans_mot = c.is_alphanumeric() || is_combining_mark(c);
        match (dans_mot, debut) {
            (true, None) => debut = Some(i),
            (false, Some(d)) => {
                sortie.push(Mot {
                    debut: d,
                    fin: i,
                    plie: plier(&texte[d..i]),
                });
                debut = None;
            }
            _ => {}
        }
    }
    if let Some(d) = debut {
        sortie.push(Mot {
            debut: d,
            fin: texte.len(),
            plie: plier(&texte[d..]),
        });
    }
    sortie
}

/// Ce qui sépare deux mots d'un même nom : espaces, trait d'union,
/// apostrophe, « / », « & », « + », ou un point collé (« R.E.M »). Une
/// ponctuation de phrase ou un retour à la ligne coupe le nom.
fn liaison_dans_un_nom(entre: &str) -> bool {
    if entre.is_empty() || entre.contains(['\n', '\r']) {
        return false;
    }
    if entre.contains('.') {
        return entre.chars().all(|c| c == '.');
    }
    entre.chars().all(|c| {
        matches!(
            c,
            ' ' | '\u{a0}' | '\t' | '-' | '\'' | '’' | '/' | '&' | '+'
        )
    })
}

/// La clé d'un nom (mots pliés joints par une espace) et son nombre de mots,
/// ou `None` si le nom ne doit jamais être lié.
fn cle_retenue(nom: &str) -> Option<(String, usize)> {
    let m = mots(nom);
    if m.is_empty() || m.len() > MOTS_MAX {
        return None;
    }
    let cle = m
        .iter()
        .map(|x| x.plie.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let lettres = m.iter().map(|x| x.plie.chars().count()).sum::<usize>();
    if lettres < LETTRES_MIN {
        return None;
    }
    if m.iter().all(|x| x.plie.chars().all(|c| c.is_numeric())) {
        return None;
    }
    if m.len() == 1 && MOTS_COURANTS.contains(&cle.as_str()) {
        return None;
    }
    if NOMS_GENERIQUES.contains(&cle.as_str()) {
        return None;
    }
    Some((cle, m.len()))
}

/// #8150 — les mots qui font d'un suffixe une mention d'édition, comme le
/// regroupement des versions (#7617, `tune-server`, `oeuvres.rs`), dont la
/// liste n'est pas visible d'ici. Forme pliée.
const MOTS_D_EDITION: &[&str] = &[
    "remaster",
    "deluxe",
    "edition",
    "anniversary",
    "expanded",
    "bonus",
    "collector",
    "reissue",
    "re-issue",
    "hi-res",
    "hires",
    "24-bit",
    "24 bit",
    "24bit",
    "mono",
    "stereo",
    "legacy",
    "sacd",
    "mfsl",
];

/// #8150 — les mots qui ouvrent une mention de disque : « Disc 1 »,
/// « CD 2 », « Disque 1 ». Forme pliée.
const MOTS_DE_DISQUE: &[&str] = &["disc", "disk", "disque", "cd"];

/// Les délimiteurs d'un suffixe : ceux du regroupement des versions, plus la
/// virgule de « The Wall, Disc 1 ».
const DELIMITEURS_DE_SUFFIXE: [&str; 4] = [" (", " [", " - ", ", "];

/// Un suffixe est-il une mention d'édition ou de disque ? Un mot d'édition,
/// une année seule, ou un mot de disque suivi au plus d'un numéro.
fn est_mention_d_edition_ou_de_disque(suffixe: &str) -> bool {
    let s = plier(suffixe.trim());
    if s.is_empty() {
        return false;
    }
    let annee_seule = s.len() == 4 && s.chars().all(|c| c.is_ascii_digit());
    if annee_seule || MOTS_D_EDITION.iter().any(|m| s.contains(m)) {
        return true;
    }
    let m = mots(&s);
    match m.as_slice() {
        [mot] => MOTS_DE_DISQUE.contains(&mot.plie.as_str()),
        [mot, numero] => {
            MOTS_DE_DISQUE.contains(&mot.plie.as_str())
                && numero.plie.chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

/// #8150 — le titre sans ses mentions finales d'édition ou de disque :
/// « The Wall (Remastered 2011 Version) » et « The Wall, Disc 1 » donnent
/// « The Wall » ; « Wish You Were Here (Live) » reste entier. `None` si le
/// titre n'en porte aucune.
pub fn titre_de_base(titre: &str) -> Option<String> {
    let mut reste = titre.trim();
    loop {
        let coupe = DELIMITEURS_DE_SUFFIXE
            .iter()
            .filter_map(|d| reste.rfind(d).map(|pos| (pos, *d)))
            .max_by_key(|(pos, _)| *pos);
        let Some((pos, d)) = coupe else { break };
        let corps = reste[pos + d.len()..].trim();
        let corps = match d {
            " (" => corps.strip_suffix(')').unwrap_or(corps),
            " [" => corps.strip_suffix(']').unwrap_or(corps),
            _ => corps,
        };
        let base = reste[..pos].trim_end();
        if base.is_empty() || !est_mention_d_edition_ou_de_disque(corps) {
            break;
        }
        reste = base;
    }
    (reste != titre.trim()).then(|| reste.to_string())
}

/// #8150 — un album tel que l'index le voit : son artiste sépare les
/// exemplaires d'un même album (un lien) des homonymes (aucun lien) ; sa
/// source et sa résolution désignent l'exemplaire lié.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumDeLIndex {
    pub id: i64,
    pub titre: String,
    pub artist_id: Option<i64>,
    /// `albums.source = 'local'`.
    pub local: bool,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
}

/// La cible d'un nom de la bibliothèque : une page à identifiant, ou la page
/// d'un label, à son nom (#8150).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LienDeBibliotheque {
    Interne(LienInterne),
    Label(String),
}

/// La force d'un nom quand plusieurs entités le portent : un artiste
/// l'emporte sur un titre d'album, un titre sur un label, un label sur un
/// titre sans ses mentions d'édition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rang {
    Artiste,
    Album,
    Label,
    AlbumDeBase,
}

#[derive(Debug, Clone)]
enum Porteur {
    Artiste(i64),
    /// L'indice de l'album dans la liste reçue.
    Album(usize),
    Label(String),
}

/// L'ordre du choix de l'exemplaire, celui des versions d'une œuvre (#7617) :
/// local avant service, meilleure fréquence, meilleure profondeur, plus
/// ancien album.
fn meilleur_d_abord(a: &AlbumDeLIndex, b: &AlbumDeLIndex) -> std::cmp::Ordering {
    b.local
        .cmp(&a.local)
        .then(b.sample_rate.unwrap_or(0).cmp(&a.sample_rate.unwrap_or(0)))
        .then(b.bit_depth.unwrap_or(0).cmp(&a.bit_depth.unwrap_or(0)))
        .then(a.id.cmp(&b.id))
}

/// Les noms d'artistes, de titres d'albums et de labels que Tune sait ouvrir.
#[derive(Debug, Default)]
pub struct IndexDesNoms {
    /// `None` : nom porté par plusieurs entités, jamais lié.
    noms: HashMap<String, Option<LienDeBibliotheque>>,
    /// #8150 — album → les exemplaires liés à sa place : la page d'un
    /// exemplaire ne lie pas vers un autre exemplaire du même album.
    exemplaires: HashMap<i64, Vec<i64>>,
    mots_max: usize,
}

impl IndexDesNoms {
    /// Les artistes et des albums sans artiste connu : deux albums de même
    /// titre y restent ambigus. Voir [`IndexDesNoms::construire_avec`].
    pub fn construire(
        artistes: impl IntoIterator<Item = (i64, String)>,
        albums: impl IntoIterator<Item = (i64, String)>,
    ) -> Self {
        let albums = albums.into_iter().map(|(id, titre)| AlbumDeLIndex {
            id,
            titre,
            ..Default::default()
        });
        Self::construire_avec(artistes, albums, std::iter::empty())
    }

    /// Un artiste l'emporte sur un album de même nom (album éponyme) ; deux
    /// artistes de même nom rendent le nom ambigu. #8150 : des albums de même
    /// titre et du même artiste sont les exemplaires d'un album — le nom mène
    /// au meilleur ([`meilleur_d_abord`]), d'où s'ouvrent ses autres
    /// versions ; d'artistes différents (ou inconnus), ils restent ambigus.
    /// Un album se nomme aussi par son [`titre_de_base`], plus faiblement ;
    /// un label, plus faiblement qu'un titre exact.
    pub fn construire_avec(
        artistes: impl IntoIterator<Item = (i64, String)>,
        albums: impl IntoIterator<Item = AlbumDeLIndex>,
        labels: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut porteurs: HashMap<String, Vec<(Rang, Porteur)>> = HashMap::new();
        let mut mots_max = 0;
        let mut porter = |nom: &str, rang: Rang, porteur: Porteur| {
            let Some((cle, n)) = cle_retenue(nom) else {
                return;
            };
            mots_max = usize::max(mots_max, n);
            porteurs.entry(cle).or_default().push((rang, porteur));
        };
        for (id, nom) in artistes {
            porter(&nom, Rang::Artiste, Porteur::Artiste(id));
        }
        let albums: Vec<AlbumDeLIndex> = albums.into_iter().collect();
        for (i, a) in albums.iter().enumerate() {
            porter(&a.titre, Rang::Album, Porteur::Album(i));
            if let Some(base) = titre_de_base(&a.titre) {
                porter(&base, Rang::AlbumDeBase, Porteur::Album(i));
            }
        }
        for nom in labels {
            let nom = nom.trim().to_string();
            porter(&nom, Rang::Label, Porteur::Label(nom.clone()));
        }

        let mut index = IndexDesNoms {
            mots_max,
            ..Default::default()
        };
        for (cle, liste) in porteurs {
            let lien = index.resoudre(&liste, &albums);
            index.noms.insert(cle, lien);
        }
        for v in index.exemplaires.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        index
    }

    /// L'entité que désigne un nom, d'après ses porteurs du rang le plus fort.
    fn resoudre(
        &mut self,
        liste: &[(Rang, Porteur)],
        albums: &[AlbumDeLIndex],
    ) -> Option<LienDeBibliotheque> {
        let fort = liste.iter().map(|(r, _)| *r).min()?;
        let du_rang = |r: Rang| liste.iter().filter(move |(x, _)| *x == r).map(|(_, p)| p);
        match fort {
            Rang::Artiste => {
                let ids: HashSet<i64> = du_rang(fort)
                    .filter_map(|p| match p {
                        Porteur::Artiste(id) => Some(*id),
                        _ => None,
                    })
                    .collect();
                let [id] = ids.into_iter().collect::<Vec<_>>()[..] else {
                    return None;
                };
                Some(LienDeBibliotheque::Interne(LienInterne {
                    genre: GenreDeLien::Artiste,
                    id,
                }))
            }
            Rang::Label => {
                let noms: HashSet<&String> = du_rang(fort)
                    .filter_map(|p| match p {
                        Porteur::Label(n) => Some(n),
                        _ => None,
                    })
                    .collect();
                let [nom] = noms.into_iter().collect::<Vec<_>>()[..] else {
                    return None;
                };
                Some(LienDeBibliotheque::Label(nom.clone()))
            }
            Rang::Album | Rang::AlbumDeBase => {
                let indices = |r: Rang| {
                    du_rang(r)
                        .filter_map(|p| match p {
                            Porteur::Album(i) => Some(*i),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                };
                let mut pris = indices(fort);
                let artiste = artiste_commun(&pris, albums)?;
                // Un titre exact retient aussi les éditions du même artiste
                // (« The Wall » et « The Wall (Remastered) ») : toutes sont
                // des exemplaires du même album. D'un autre artiste, le
                // titre exact l'emporte.
                if fort == Rang::Album {
                    let de_base = indices(Rang::AlbumDeBase);
                    let mut ensemble = pris.clone();
                    ensemble.extend(&de_base);
                    if artiste.is_some() && artiste_commun(&ensemble, albums) == Some(artiste) {
                        pris = ensemble;
                    }
                }
                let meilleur = pris
                    .iter()
                    .map(|i| &albums[*i])
                    .min_by(|a, b| meilleur_d_abord(a, b))?;
                for i in &pris {
                    self.exemplaires
                        .entry(albums[*i].id)
                        .or_default()
                        .push(meilleur.id);
                }
                Some(LienDeBibliotheque::Interne(LienInterne {
                    genre: GenreDeLien::Album,
                    id: meilleur.id,
                }))
            }
        }
    }

    /// L'entité de la bibliothèque que ce nom désignerait dans une bio, s'il
    /// y en a une seule (#8129 : sur une fiche de service, l'artiste ou
    /// l'album de la page n'est pas lié vers son homonyme local). Un label
    /// n'a pas d'identifiant : `None`.
    pub fn lien_du_nom(&self, nom: &str) -> Option<LienInterne> {
        let (cle, _) = cle_retenue(nom)?;
        match self.noms.get(&cle)? {
            Some(LienDeBibliotheque::Interne(l)) => Some(*l),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.noms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.noms.is_empty()
    }

    /// Les liens internes trouvés dans `texte` : (début, fin, lien), positions
    /// en octets, dans l'ordre du texte. Voir
    /// [`IndexDesNoms::liens_de_la_bibliotheque`] ; les labels n'y sont pas.
    pub fn liens_internes(
        &self,
        texte: &str,
        exclus: &[LienInterne],
    ) -> Vec<(usize, usize, LienInterne)> {
        self.liens_de_la_bibliotheque(texte, exclus)
            .into_iter()
            .filter_map(|(d, f, l)| match l {
                LienDeBibliotheque::Interne(l) => Some((d, f, l)),
                LienDeBibliotheque::Label(_) => None,
            })
            .collect()
    }

    /// Les noms de la bibliothèque trouvés dans `texte` : (début, fin, cible),
    /// positions en octets, dans l'ordre du texte. Le plus long nom
    /// l'emporte ; un nom d'un seul mot doit commencer par une majuscule dans
    /// le texte. Un album exclu l'est avec ses autres exemplaires (#8150).
    pub fn liens_de_la_bibliotheque(
        &self,
        texte: &str,
        exclus: &[LienInterne],
    ) -> Vec<(usize, usize, LienDeBibliotheque)> {
        let m = mots(texte);
        let mut deja: HashSet<LienDeBibliotheque> = HashSet::new();
        for l in exclus {
            deja.insert(LienDeBibliotheque::Interne(*l));
            if l.genre == GenreDeLien::Album {
                for id in self.exemplaires.get(&l.id).into_iter().flatten() {
                    deja.insert(LienDeBibliotheque::Interne(LienInterne {
                        genre: GenreDeLien::Album,
                        id: *id,
                    }));
                }
            }
        }
        let mut trouves = Vec::new();
        let mut i = 0;
        'mots: while i < m.len() {
            let n_max = self.mots_max.min(m.len() - i);
            let mut cle = String::new();
            let mut candidats = Vec::with_capacity(n_max);
            for n in 1..=n_max {
                let k = i + n - 1;
                if n > 1 {
                    if !liaison_dans_un_nom(&texte[m[k - 1].fin..m[k].debut]) {
                        break;
                    }
                    cle.push(' ');
                }
                cle.push_str(&m[k].plie);
                candidats.push(cle.clone());
            }
            for (n, cle) in candidats.iter().enumerate().rev() {
                let n = n + 1;
                let Some(Some(lien)) = self.noms.get(cle) else {
                    continue;
                };
                if n == 1
                    && !texte[m[i].debut..]
                        .chars()
                        .next()
                        .is_some_and(char::is_uppercase)
                {
                    continue;
                }
                if !deja.insert(lien.clone()) {
                    continue;
                }
                trouves.push((m[i].debut, m[i + n - 1].fin, lien.clone()));
                i += n;
                continue 'mots;
            }
            i += 1;
        }
        trouves
    }
}

/// L'artiste commun à ces albums : `Some(None)` pour un seul album (ou un
/// seul identifiant) sans artiste connu, `None` si leurs artistes diffèrent
/// ou si l'un d'eux manque alors qu'ils sont plusieurs.
fn artiste_commun(indices: &[usize], albums: &[AlbumDeLIndex]) -> Option<Option<i64>> {
    let ids: HashSet<i64> = indices.iter().map(|i| albums[*i].id).collect();
    let premier = albums.get(*indices.first()?)?;
    if ids.len() == 1 {
        return Some(premier.artist_id);
    }
    let artiste = premier.artist_id?;
    indices
        .iter()
        .all(|i| albums[*i].artist_id == Some(artiste))
        .then_some(Some(artiste))
}

/// Découpe `texte` en segments : liens internes d'abord, puis liens externes
/// (première occurrence de leur texte hors d'un lien interne, une fois par
/// URL, et seulement si l'URL passe [`url_autorisee`]).
pub fn segments_de_la_bio(
    texte: &str,
    index: &IndexDesNoms,
    exclus: &[LienInterne],
    externes: &[LienExterne],
) -> Vec<BioSegment> {
    segments_avec_liens_de_service(texte, index, exclus, &[], externes)
}

/// #8129 — [`segments_de_la_bio`], avec en plus les liens du HTML d'origine
/// qui désignent une fiche de service (texte du lien, cible). Ils passent en
/// premier — c'est la source qui les a posés —, à la première occurrence
/// libre de leur texte, une fois par cible ; les noms de la bibliothèque et
/// les liens externes se placent ensuite, hors de leurs zones.
pub fn segments_avec_liens_de_service(
    texte: &str,
    index: &IndexDesNoms,
    exclus: &[LienInterne],
    de_service: &[(String, LienDeService)],
    externes: &[LienExterne],
) -> Vec<BioSegment> {
    enum Cible {
        Interne(LienInterne),
        Label(String),
        Service(LienDeService),
        Externe(String),
    }
    fn libre<T>(zones: &[(usize, usize, T)], d: usize, f: usize) -> bool {
        zones.iter().all(|(zd, zf, _)| f <= *zd || d >= *zf)
    }
    let mut zones: Vec<(usize, usize, Cible)> = Vec::new();
    let mut cibles = HashSet::new();
    for (t, lien) in de_service {
        if t.is_empty() || cibles.contains(lien) {
            continue;
        }
        if let Some((d, t)) = texte
            .match_indices(t.as_str())
            .find(|(d, t)| libre(&zones, *d, d + t.len()))
        {
            cibles.insert(lien.clone());
            zones.push((d, d + t.len(), Cible::Service(lien.clone())));
        }
    }
    let internes: Vec<_> = index
        .liens_de_la_bibliotheque(texte, exclus)
        .into_iter()
        .filter(|(d, f, _)| libre(&zones, *d, *f))
        .collect();
    zones.extend(internes.into_iter().map(|(d, f, l)| {
        let cible = match l {
            LienDeBibliotheque::Interne(l) => Cible::Interne(l),
            LienDeBibliotheque::Label(nom) => Cible::Label(nom),
        };
        (d, f, cible)
    }));
    let mut urls = HashSet::new();
    for e in externes {
        let Some(url) = url_autorisee(&e.url) else {
            continue;
        };
        if e.texte.is_empty() || urls.contains(&url) {
            continue;
        }
        let place = texte
            .match_indices(e.texte.as_str())
            .find(|(d, t)| libre(&zones, *d, d + t.len()));
        if let Some((d, t)) = place {
            urls.insert(url.clone());
            zones.push((d, d + t.len(), Cible::Externe(url)));
        }
    }
    zones.sort_by_key(|(d, _, _)| *d);

    let mut segments = Vec::new();
    let mut curseur = 0;
    for (d, f, cible) in zones {
        if d > curseur {
            segments.push(BioSegment {
                texte: texte[curseur..d].to_string(),
                lien: None,
                label: None,
                lien_service: None,
                url: None,
            });
        }
        let (lien, label, lien_service, url) = match cible {
            Cible::Interne(l) => (Some(l), None, None, None),
            Cible::Label(n) => (None, Some(n), None, None),
            Cible::Service(l) => (None, None, Some(l), None),
            Cible::Externe(u) => (None, None, None, Some(u)),
        };
        segments.push(BioSegment {
            texte: texte[d..f].to_string(),
            lien,
            label,
            lien_service,
            url,
        });
        curseur = f;
    }
    if curseur < texte.len() {
        segments.push(BioSegment {
            texte: texte[curseur..].to_string(),
            lien: None,
            label: None,
            lien_service: None,
            url: None,
        });
    }
    segments
}

/// L'index des noms de la bibliothèque : tous les artistes, tous les albums
/// (avec leur artiste, leur source et leur résolution, #8150) et les labels
/// de l'onglet Labels.
pub fn charger_index(db: &Arc<dyn DbBackend>) -> Result<IndexDesNoms, String> {
    let artistes: Vec<(i64, String)> = db
        .query_many("SELECT id, name FROM artists", &[])?
        .into_iter()
        .filter_map(|cols| Some((cols.first()?.as_i64()?, cols.get(1)?.as_string()?)))
        .collect();
    let albums: Vec<AlbumDeLIndex> = db
        .query_many(
            "SELECT id, title, artist_id, source, sample_rate, bit_depth FROM albums",
            &[],
        )?
        .into_iter()
        .filter_map(|cols| {
            Some(AlbumDeLIndex {
                id: cols.first()?.as_i64()?,
                titre: cols.get(1)?.as_string()?,
                artist_id: cols.get(2).and_then(|c| c.as_i64()),
                local: cols
                    .get(3)
                    .and_then(|c| c.as_string())
                    .is_none_or(|s| s == "local"),
                sample_rate: cols.get(4).and_then(|c| c.as_i64()),
                bit_depth: cols.get(5).and_then(|c| c.as_i64()),
            })
        })
        .collect();
    let labels = crate::db::album_repo::AlbumRepo::with_backend(db.clone())
        .labels_de_la_bibliotheque()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|l| l.nom);
    Ok(IndexDesNoms::construire_avec(artistes, albums, labels))
}

/// Au-delà, l'index partagé est relu même si la bibliothèque n'a ni gagné ni
/// perdu d'entrée (un artiste renommé).
const DUREE_DE_L_INDEX: std::time::Duration = std::time::Duration::from_secs(600);

/// Ce qui change quand on ajoute ou retire un artiste ou un album.
fn signature_de_la_bibliotheque(db: &Arc<dyn DbBackend>) -> Vec<i64> {
    db.query_one(
        "SELECT (SELECT COUNT(*) FROM artists), (SELECT COALESCE(MAX(id), 0) FROM artists), \
         (SELECT COUNT(*) FROM albums), (SELECT COALESCE(MAX(id), 0) FROM albums)",
        &[],
    )
    .ok()
    .flatten()
    .map(|cols| cols.iter().map(|c| c.as_i64().unwrap_or(-1)).collect())
    .unwrap_or_default()
}

/// L'index d'une base, gardé tant que la bibliothèque n'a pas changé (nombre
/// et plus grand identifiant des artistes et des albums) et au plus
/// [`DUREE_DE_L_INDEX`]. Partagé par les bios de la bibliothèque (#7910) et
/// celles des services (#8129) : un `Arc` neuf veut dire « index rebâti ».
pub fn index_partage(db: &Arc<dyn DbBackend>) -> Arc<IndexDesNoms> {
    use std::sync::{Mutex, OnceLock, Weak};
    use std::time::Instant;
    /// Un `Weak` garde l'allocation : l'adresse ne peut pas être reprise par
    /// une autre base tant que l'entrée existe (bases de test successives).
    type Entree = (Weak<dyn DbBackend>, Vec<i64>, Instant, Arc<IndexDesNoms>);
    static INDEX: OnceLock<Mutex<Vec<Entree>>> = OnceLock::new();
    let index = INDEX.get_or_init(|| Mutex::new(Vec::new()));
    let signature = signature_de_la_bibliotheque(db);
    let ici = Arc::downgrade(db);
    {
        let mut entrees = index.lock().unwrap_or_else(|e| e.into_inner());
        entrees.retain(|(base, ..)| base.strong_count() > 0);
        if let Some((_, sig, bati, i)) = entrees.iter().find(|(b, ..)| Weak::ptr_eq(b, &ici))
            && *sig == signature
            && bati.elapsed() < DUREE_DE_L_INDEX
        {
            return i.clone();
        }
    }
    // Le verrou n'est jamais tenu pendant la lecture de la bibliothèque : une
    // grosse base ferait attendre toutes les bios, de toutes les bases.
    let neuf = Arc::new(charger_index(db).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "bio_segments_index_failed");
        IndexDesNoms::default()
    }));
    let mut entrees = index.lock().unwrap_or_else(|e| e.into_inner());
    let entree = (ici.clone(), signature, Instant::now(), neuf.clone());
    match entrees.iter().position(|(b, ..)| Weak::ptr_eq(b, &ici)) {
        Some(p) => entrees[p] = entree,
        None => entrees.push(entree),
    }
    neuf
}

/// Empreinte stable (FNV-1a 64 bits) d'un texte de bio.
pub fn empreinte(texte: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for o in texte.as_bytes() {
        h ^= u64::from(*o);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn cle_des_liens(genre: GenreDeLien, id: i64) -> String {
    format!("bio_liens:{}:{id}", genre.nom())
}

#[derive(Serialize, Deserialize)]
struct LiensRanges {
    empreinte: String,
    liens: Vec<LienExterne>,
}

/// Range les liens externes d'une bio qu'on vient d'enregistrer, avec
/// l'empreinte de son texte. Sans lien, l'entrée précédente est effacée.
pub fn ranger_liens_externes(
    db: &Arc<dyn DbBackend>,
    genre: GenreDeLien,
    id: i64,
    texte: &str,
    liens: &[LienExterne],
) {
    let reglages = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    let cle = cle_des_liens(genre, id);
    if liens.is_empty() {
        reglages.delete(&cle).ok();
        return;
    }
    let valeur = LiensRanges {
        empreinte: empreinte(texte),
        liens: liens.to_vec(),
    };
    if let Ok(json) = serde_json::to_string(&valeur) {
        reglages.set(&cle, &json).ok();
    }
}

/// Les liens externes rangés pour CE texte de bio ; aucun si la bio a changé
/// depuis (empreinte différente).
pub fn liens_externes_ranges(
    db: &Arc<dyn DbBackend>,
    genre: GenreDeLien,
    id: i64,
    texte: &str,
) -> Vec<LienExterne> {
    let reglages = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    let cle = cle_des_liens(genre, id);
    let Some(json) = reglages.get(&cle).ok().flatten() else {
        return Vec::new();
    };
    match serde_json::from_str::<LiensRanges>(&json) {
        Ok(r) if r.empreinte == empreinte(texte) => r.liens,
        Ok(r) => {
            // La bio ENREGISTRÉE a été réécrite depuis (synchronisation
            // communautaire, saisie à la main) : ces liens ne la décrivent
            // plus, la ligne s'efface. Une bio servie dans une autre langue
            // que celle enregistrée ne l'efface pas.
            if bio_enregistree(db, genre, id).is_none_or(|b| empreinte(&b) != r.empreinte) {
                reglages.delete(&cle).ok();
            }
            Vec::new()
        }
        Err(_) => {
            reglages.delete(&cle).ok();
            Vec::new()
        }
    }
}

fn bio_enregistree(db: &Arc<dyn DbBackend>, genre: GenreDeLien, id: i64) -> Option<String> {
    let table = match genre {
        GenreDeLien::Artiste => "artists",
        GenreDeLien::Album => "albums",
    };
    let marque = match db.engine() {
        crate::db::engine::Engine::Sqlite => "?",
        crate::db::engine::Engine::Postgres => "$1",
    };
    db.query_one(
        &format!("SELECT bio FROM {table} WHERE id = {marque}"),
        &[&id],
    )
    .ok()
    .flatten()?
    .first()?
    .as_string()
}

#[cfg(test)]
#[path = "bio_segments_7910_tests.rs"]
mod bio_segments_7910_tests;

#[cfg(test)]
#[path = "bio_segments_8150_tests.rs"]
mod bio_segments_8150_tests;
