//! Biographies cliquables (#7910).
//!
//! Une biographie d'artiste ou d'album est rendue au client en **segments
//! structurés** : du texte, du texte qui ouvre une page de Tune (lien interne),
//! ou du texte qui ouvre une page de la source (lien externe). Le client les
//! dessine avec des composants : aucun HTML venu d'une source n'est jamais
//! injecté côté web.
//!
//! - Liens internes, prioritaires : un artiste ou un album de la bibliothèque
//!   cité dans le texte. Correspondance exacte, insensible à la casse et aux
//!   accents, sur des mots entiers. Les noms courts, les mots courants, les
//!   nombres seuls et les noms portés par plusieurs entités sont écartés. Un
//!   seul lien par entité ; l'entité de la page (et, sur un album, son
//!   artiste) n'est jamais liée.
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

/// Un morceau de la bio : du texte seul, du texte avec un lien interne
/// (`lien`), ou du texte avec un lien externe (`url`), jamais les deux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BioSegment {
    pub texte: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lien: Option<LienInterne>,
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
    let ancre = regex::Regex::new(
        r#"(?is)<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|'([^']*)')[^>]*>(.*?)</a\s*>"#,
    )
    .expect("regex des ancres");
    let balise = regex::Regex::new(r"<[^>]+>").expect("regex des balises");
    let mut vus = HashSet::new();
    let mut liens = Vec::new();
    for c in ancre.captures_iter(html) {
        let href = c
            .get(1)
            .or_else(|| c.get(2))
            .map(|m| m.as_str())
            .unwrap_or("");
        let Some(url) = url_autorisee(&href.replace("&amp;", "&")) else {
            continue;
        };
        let texte = balise
            .replace_all(c.get(3).map(|m| m.as_str()).unwrap_or(""), "")
            .trim()
            .to_string();
        if texte.is_empty() || !vus.insert(url.clone()) {
            continue;
        }
        liens.push(LienExterne { texte, url });
    }
    liens
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

/// Les noms d'artistes et de titres d'albums que Tune sait ouvrir.
#[derive(Debug, Default)]
pub struct IndexDesNoms {
    /// `None` : nom porté par plusieurs entités, jamais lié.
    noms: HashMap<String, Option<LienInterne>>,
    mots_max: usize,
}

impl IndexDesNoms {
    /// Un artiste l'emporte sur un album de même nom (album éponyme) ; deux
    /// artistes, ou deux albums, de même nom rendent le nom ambigu.
    pub fn construire(
        artistes: impl IntoIterator<Item = (i64, String)>,
        albums: impl IntoIterator<Item = (i64, String)>,
    ) -> Self {
        let mut index = IndexDesNoms::default();
        for (id, nom) in artistes {
            index.ajouter(
                &nom,
                LienInterne {
                    genre: GenreDeLien::Artiste,
                    id,
                },
            );
        }
        for (id, titre) in albums {
            index.ajouter(
                &titre,
                LienInterne {
                    genre: GenreDeLien::Album,
                    id,
                },
            );
        }
        index
    }

    fn ajouter(&mut self, nom: &str, lien: LienInterne) {
        let Some((cle, n)) = cle_retenue(nom) else {
            return;
        };
        self.mots_max = self.mots_max.max(n);
        match self.noms.get_mut(&cle) {
            None => {
                self.noms.insert(cle, Some(lien));
            }
            Some(place) => match place {
                Some(deja) if deja.id == lien.id && deja.genre == lien.genre => {}
                Some(deja)
                    if deja.genre == GenreDeLien::Artiste && lien.genre == GenreDeLien::Album => {}
                _ => *place = None,
            },
        }
    }

    pub fn len(&self) -> usize {
        self.noms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.noms.is_empty()
    }

    /// Les liens internes trouvés dans `texte` : (début, fin, lien), positions
    /// en octets, dans l'ordre du texte. Le plus long nom l'emporte ; un nom
    /// d'un seul mot doit commencer par une majuscule dans le texte.
    pub fn liens_internes(
        &self,
        texte: &str,
        exclus: &[LienInterne],
    ) -> Vec<(usize, usize, LienInterne)> {
        let m = mots(texte);
        let mut deja: HashSet<LienInterne> = exclus.iter().copied().collect();
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
                if !deja.insert(*lien) {
                    continue;
                }
                trouves.push((m[i].debut, m[i + n - 1].fin, *lien));
                i += n;
                continue 'mots;
            }
            i += 1;
        }
        trouves
    }
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
    enum Cible {
        Interne(LienInterne),
        Externe(String),
    }
    let mut zones: Vec<(usize, usize, Cible)> = index
        .liens_internes(texte, exclus)
        .into_iter()
        .map(|(d, f, l)| (d, f, Cible::Interne(l)))
        .collect();
    let mut urls = HashSet::new();
    for e in externes {
        let Some(url) = url_autorisee(&e.url) else {
            continue;
        };
        if e.texte.is_empty() || urls.contains(&url) {
            continue;
        }
        let libre = texte.match_indices(e.texte.as_str()).find(|(d, t)| {
            let f = d + t.len();
            zones.iter().all(|(zd, zf, _)| f <= *zd || *d >= *zf)
        });
        if let Some((d, t)) = libre {
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
                url: None,
            });
        }
        let (lien, url) = match cible {
            Cible::Interne(l) => (Some(l), None),
            Cible::Externe(u) => (None, Some(u)),
        };
        segments.push(BioSegment {
            texte: texte[d..f].to_string(),
            lien,
            url,
        });
        curseur = f;
    }
    if curseur < texte.len() {
        segments.push(BioSegment {
            texte: texte[curseur..].to_string(),
            lien: None,
            url: None,
        });
    }
    segments
}

/// L'index des noms de la bibliothèque : tous les artistes et tous les albums.
pub fn charger_index(db: &Arc<dyn DbBackend>) -> Result<IndexDesNoms, String> {
    let paires = |sql: &str| -> Result<Vec<(i64, String)>, String> {
        Ok(db
            .query_many(sql, &[])?
            .into_iter()
            .filter_map(|cols| Some((cols.first()?.as_i64()?, cols.get(1)?.as_string()?)))
            .collect())
    };
    let artistes = paires("SELECT id, name FROM artists")?;
    let albums = paires("SELECT id, title FROM albums")?;
    Ok(IndexDesNoms::construire(artistes, albums))
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
    let Some(json) = reglages.get(&cle_des_liens(genre, id)).ok().flatten() else {
        return Vec::new();
    };
    match serde_json::from_str::<LiensRanges>(&json) {
        Ok(r) if r.empreinte == empreinte(texte) => r.liens,
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[path = "bio_segments_7910_tests.rs"]
mod bio_segments_7910_tests;
