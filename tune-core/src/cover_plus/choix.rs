//! Cover+ (#8452) — juger et classer les réponses d'iTunes Search.
//!
//! Portage fidèle de `itunes_choix.py`, le code de référence de
//! radio-metadata.fr (11/11/2026). Les règles et les mesures qui les fondent
//! sont dans la spécification, § 3.3 à 3.5 ; elles sont rappelées ici au plus
//! court, à côté de la ligne qui les applique.
//!
//! **La règle franche** : un résultat n'est retenu que si l'artiste ET le
//! titre concordent. Une mauvaise pochette est pire que pas de pochette :
//! l'écran garde alors le logo de la station.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

fn re(motif: &str) -> Regex {
    Regex::new(motif).expect("motif Cover+ valide")
}

static PARENTHESES: LazyLock<Regex> = LazyLock::new(|| re(r"\([^)]*\)|\[[^\]]*\]"));
static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| re(r"[^a-z0-9]+"));
static ARTICLE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?:the|les|la|le|los|las|el|die|der|das) "));
static COLLAB_SEP: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\s*,\s*|\s+(?:&|and|et|feat\.?|ft\.?|featuring|x|with|vs\.?)\s+|\s*[;/]\s*")
});
static TAILLE_ARTWORK: LazyLock<Regex> = LazyLock::new(|| re(r"/\d+x\d+bb\.(?:jpg|png)$"));

/// Minuscules, sans accents.
fn minuscules_sans_accents(t: &str) -> String {
    t.to_lowercase()
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .collect()
}

/// La normalisation des deux côtés avant de comparer : minuscules, sans
/// accents, sans le contenu des parenthèses et des crochets, `&` devenu
/// `and`, toute autre ponctuation réduite à des espaces.
pub fn norme(t: &str) -> String {
    let t = minuscules_sans_accents(t);
    let t = PARENTHESES.replace_all(&t, " ");
    let t = t.replace('&', " and ");
    NON_ALNUM.replace_all(&t, " ").trim().to_string()
}

/// Comme [`norme`], mais en GARDANT le contenu des parenthèses.
pub fn norme_pleine(t: &str) -> String {
    let t = minuscules_sans_accents(t).replace('&', " and ");
    NON_ALNUM.replace_all(&t, " ").trim().to_string()
}

pub fn sans_article(n: &str) -> String {
    ARTICLE.replace(n, "").into_owned()
}

fn sans_espaces(t: &str) -> String {
    t.replace(' ', "")
}

/// La règle franche (assouplie le 11/11) :
/// - égalité, ou l'un est le début de l'autre suivi d'un espace ;
/// - l'article en tête est ignoré : « The Pretenders » = « Pretenders » ;
/// - en dernier recours, égalité SANS les espaces : « Mink De Ville » =
///   « Mink DeVille ». Égalité seulement, jamais de préfixe sans espaces.
pub fn concorde(source: &str, candidat: &str) -> bool {
    let (s, c) = (norme(source), norme(candidat));
    if s.is_empty() || c.is_empty() {
        return false;
    }
    let (s2, c2) = (sans_article(&s), sans_article(&c));
    for a in [&s, &s2] {
        for b in [&c, &c2] {
            if !a.is_empty()
                && !b.is_empty()
                && (a == b || b.starts_with(&format!("{a} ")) || a.starts_with(&format!("{b} ")))
            {
                return true;
            }
        }
    }
    sans_espaces(&s2) == sans_espaces(&c2)
}

/// Le premier nom d'une collaboration : « Lady Gaga, Bruno Mars » → « Lady Gaga ».
pub fn artiste_principal(a: &str) -> &str {
    COLLAB_SEP.splitn(a, 2).next().unwrap_or(a)
}

/// La règle franche sur l'artiste, plus le cas des collaborations écrites
/// autrement : « Lady Gaga, Bruno Mars » = « Lady Gaga & Bruno Mars ». Le
/// titre doit concorder lui aussi, ce qui garde la règle sûre.
pub fn concorde_artiste(source: &str, candidat: &str) -> bool {
    if concorde(source, candidat) {
        return true;
    }
    let (a, b) = (artiste_principal(source), artiste_principal(candidat));
    !norme(a).is_empty() && (a != source || b != candidat) && meme_nom(a, b)
}

/// Égalité stricte de deux noms d'artiste, article et espaces ignorés. Pas de
/// préfixe : « Tank » ne vaut pas « Tank And The Bangas ».
pub fn meme_nom(a: &str, b: &str) -> bool {
    let x = sans_espaces(&sans_article(&norme(a)));
    let y = sans_espaces(&sans_article(&norme(b)));
    !x.is_empty() && x == y
}

/// `artworkUrl100` agrandie : `…/100x100bb.jpg` → `…/600x600bb.jpg`. Une
/// adresse d'une autre forme est rendue telle quelle.
pub fn grande(url: &str, taille: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    Some(
        TAILLE_ARTWORK
            .replace(url, format!("/{taille}.jpg").as_str())
            .into_owned(),
    )
}

// --- variantes à écarter : remix, live, karaoké, reprises… ---------------

/// Une marque ne pénalise que si le titre diffusé ne la porte pas lui-même :
/// « Alors on danse » ne doit pas tomber sur le remix de Kanye West, mais
/// « Alors on danse (Dubdogz Remix) » doit, lui, garder ce remix.
static MARQUES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("remix", r"\bremix(?:ed)?\b|\brmx\b"),
        ("mix", r"\b(?:extended|club|dub|vip) mix\b"),
        ("live", r"\blive\b|\ben (?:concert|public)\b|\bunplugged\b"),
        (
            "karaoke",
            r"\bkaraok[eé]\b|\boriginally performed\b|\bmade famous\b|\bin the style of\b",
        ),
        ("instrumental", r"\binstrumental\b"),
        ("acoustique", r"\bacousti(?:c|que)\b"),
        (
            "version",
            r"\b(?:piano|lounge|jazz|orchestral|string|symphonic|acoustic|reggae|bossa|lofi|lo-fi|8-bit|8 bit) version\b|\bversion (?:piano|lounge|jazz|acoustique)\b",
        ),
        (
            "reprise",
            r"\btribute\b|\bcover\b|\bhommage\b|\blullab(?:y|ies)\b|\bberceuse\b|\brockabye\b|\bperforms\b",
        ),
        ("réenregistré", r"\bre-?recorded\b|\btaylor'?s version\b"),
    ]
    .into_iter()
    .map(|(nom, motif)| (nom, re(motif)))
    .collect()
});

/// Les best-of portent la date de sortie du MORCEAU, pas celle du disque :
/// sans ce filtre, ils passent devant l'album d'origine au critère de la date.
static BEST_OF: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(best of|greatest hits|the very best|anthology|anthologie|essential|the collection|gold|ultimate|hits|int[eé]grale|platinum|complete (?:recordings|singles))\b",
    )
});
static REEDITION: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(deluxe|remaster(?:ed)?|anniversary|expanded|edition|[eé]dition|bonus|legacy|collector)\b",
    )
});
static COMPIL: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)^(various artists|multi-?interpr[eè]tes|artistes divers|verschiedene interpreten|varios artistas)$",
    )
});
static INDICATIONS: LazyLock<Regex> = LazyLock::new(|| re(r"\(([^)]*)\)|\[([^\]]*)\]|\s-\s(.*)$"));
static MOTS_VERSION: LazyLock<Regex> = LazyLock::new(|| {
    re(r"\b(?:remix(?:ed)?|rmx|mix|edit|version|extended|club|dub|vip|radio|original|live)\b")
});
static ESPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));

/// Une indication de version du titre diffusé : la marque et sa phrase
/// normalisée, par exemple `("remix", "poirier remix")`.
pub type Version = (&'static str, String);

/// Les indications de version du titre diffusé.
pub fn version_diffusee(titre: &str) -> Vec<Version> {
    let mut out = Vec::new();
    for c in INDICATIONS.captures_iter(titre) {
        let x = c
            .get(1)
            .or_else(|| c.get(2))
            .or_else(|| c.get(3))
            .map(|m| m.as_str())
            .unwrap_or("");
        let bas = x.to_lowercase();
        for (nom, motif) in MARQUES.iter() {
            if motif.is_match(&bas) {
                out.push((*nom, norme_pleine(x)));
            }
        }
    }
    out
}

/// Le nom du remixeur, tiré de la version diffusée : « poirier » pour
/// « (Poirier remix) ».
pub fn remixeurs(voulues: &[Version]) -> Vec<String> {
    voulues
        .iter()
        .filter(|(nom, _)| *nom == "remix" || *nom == "mix")
        .filter_map(|(_, phrase)| {
            let r = MOTS_VERSION.replace_all(phrase, " ");
            let r = ESPACES.replace_all(&r, " ").trim().to_string();
            (!r.is_empty()).then_some(r)
        })
        .collect()
}

/// 0 : même version exactement (même remixeur) ; 1 : même sorte de version ;
/// 2 : pas la version diffusée.
fn rang_version(voulues: &[Version], texte_candidat: &str) -> u8 {
    if voulues.is_empty() {
        return 0;
    }
    let t = norme_pleine(texte_candidat);
    if voulues
        .iter()
        .any(|(_, p)| !p.is_empty() && t.contains(p.as_str()))
    {
        return 0;
    }
    let noms: HashSet<&str> = voulues.iter().map(|(n, _)| *n).collect();
    let bas = texte_candidat.to_lowercase();
    if MARQUES
        .iter()
        .any(|(nom, motif)| noms.contains(nom) && motif.is_match(&bas))
    {
        return 1;
    }
    2
}

fn marques(texte: &str, source_titre: &str) -> Vec<&'static str> {
    let (t, s) = (texte.to_lowercase(), source_titre.to_lowercase());
    MARQUES
        .iter()
        .filter(|(_, motif)| motif.is_match(&t) && !motif.is_match(&s))
        .map(|(nom, _)| *nom)
        .collect()
}

fn texte(x: &Value, cle: &str) -> String {
    x.get(cle)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Une pochette retenue, dans l'ordre de préférence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pochette {
    pub url: String,
    pub album: String,
    pub date: String,
}

/// La discographie d'un artiste : `{collectionId: date de parution du DISQUE}`.
pub type Disques = HashMap<i64, String>;

/// Garde les résultats en concordance franche, puis les classe.
///
/// Ordre de préférence, critère par critère (spécification, § 3.4) :
///  0. la version diffusée (même remixeur d'abord, puis celle que le disque
///     lui-même porte) ;
///  1. aucune marque de variante non diffusée ;
///  2. pas une compilation ;
///  3. un album (7 pistes ou plus) ;
///  4. la date du disque la plus ancienne ;
///  5. à date égale, l'édition d'origine plutôt qu'une réédition ;
///  6. puis le disque le plus court.
///
/// Une même image n'apparaît qu'une fois, sous son meilleur rang.
pub fn classer(
    resultats: &[Value],
    artiste: &str,
    titre: &str,
    disques: Option<&Disques>,
) -> Vec<Pochette> {
    let complete = disques.is_some_and(|d| d.len() < 199);
    let voulues = version_diffusee(titre);
    let rmx = remixeurs(&voulues);
    let artiste_source = sans_article(&norme(artiste));

    let artiste_ok = |x: &Value| {
        let nom = texte(x, "artistName");
        if concorde_artiste(artiste, &nom) {
            return true;
        }
        // Un remix est souvent crédité AU REMIXEUR : on l'accepte si
        // l'artiste diffusé figure dans le titre ou parmi les crédités.
        let tout = norme_pleine(&format!("{} {}", texte(x, "trackName"), nom));
        rmx.iter().any(|r| concorde(r, &nom)) && tout.contains(artiste_source.as_str())
    };

    type Cle = (u8, u8, bool, bool, bool, String, bool, i64);
    let mut cand: Vec<(Cle, Pochette)> = Vec::new();
    for x in resultats {
        let piste = texte(x, "trackName");
        if !(artiste_ok(x) && concorde(titre, &piste)) {
            continue;
        }
        let Some(url) = grande(&texte(x, "artworkUrl100"), "600x600bb") else {
            continue;
        };
        let album = texte(x, "collectionName");
        let artiste_disque = texte(x, "collectionArtistName");
        let mut mq = marques(&piste, titre);
        mq.extend(marques(&album, titre));
        let mut compil = COMPIL.is_match(&artiste_disque)
            || (!artiste_disque.is_empty() && !concorde(artiste, &artiste_disque))
            || (BEST_OF.is_match(&album) && !BEST_OF.is_match(titre));
        let pistes = x.get("trackCount").and_then(Value::as_i64).unwrap_or(0);
        let mut date: String = texte(x, "releaseDate").chars().take(10).collect();
        if let Some(d) = disques {
            let cid = x.get("collectionId").and_then(Value::as_i64);
            match cid.and_then(|c| d.get(&c)) {
                Some(du_disque) => {
                    if !du_disque.is_empty() {
                        date = du_disque.clone();
                    }
                }
                None if complete => compil = true,
                None => {}
            }
        }
        let rv = rang_version(&voulues, &format!("{piste} {album}"));
        let rv_disque = if voulues.is_empty() || rang_version(&voulues, &album) < 2 {
            0
        } else {
            1
        };
        let reedition = REEDITION.is_match(&album);
        let cle = (
            rv,
            rv_disque,
            !mq.is_empty(),
            compil,
            pistes < 7,
            if date.is_empty() {
                "9999".to_string()
            } else {
                date.clone()
            },
            reedition,
            pistes,
        );
        cand.push((cle, Pochette { url, album, date }));
    }
    cand.sort_by(|a, b| a.0.cmp(&b.0));
    let mut vues = HashSet::new();
    cand.into_iter()
        .filter(|(_, p)| vues.insert(p.url.clone()))
        .map(|(_, p)| p)
        .collect()
}

/// `{collectionId: date}` depuis `lookup?id=<artistId>&entity=album&limit=200`.
pub fn discographie(albums: &[Value]) -> Disques {
    albums
        .iter()
        .filter(|a| a.get("wrapperType").and_then(Value::as_str) == Some("collection"))
        .filter_map(|a| {
            let id = a.get("collectionId").and_then(Value::as_i64)?;
            Some((id, texte(a, "releaseDate").chars().take(10).collect()))
        })
        .collect()
}
