//! Cover+ (#8452) — pré-traiter la métadonnée ICY avant de chercher.
//!
//! Portage fidèle de `pretraitement.py` (radio-metadata.fr, 11/11/2026 ;
//! spécification, § 2). Le StreamTitle brut devient :
//! 1. une décision « chercher / ne rien chercher » ;
//! 2. une à trois requêtes iTunes, qui ne gardent que ce qu'iTunes a dans ses
//!    données (il exige TOUS les mots) ;
//! 3. des hypothèses (artiste, titre) pour JUGER la réponse, qui gardent tout
//!    (versions remix ou live, remixeur).

use std::sync::LazyLock;

use regex::Regex;

use super::choix::{norme, remixeurs, version_diffusee};

fn re(motif: &str) -> Regex {
    Regex::new(motif).expect("motif Cover+ valide")
}

// --- 1. réparer le texte ----------------------------------------------------

/// UTF-8 lu comme du Latin-1 : un A tilde ou un A circonflexe suivi d'un
/// caractère de la plage 0x20-0xBF trahit deux octets UTF-8 décodés un à un.
static MOJIBAKE: LazyLock<Regex> = LazyLock::new(|| re("[\u{C3}\u{C2}][\u{20}-\u{BF}]"));
static ESPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));

/// Les entités HTML qu'on rencontre dans l'ICY : les cinq de XML, l'espace
/// insécable et les références numériques.
fn decoder_entites(t: &str) -> String {
    if !t.contains('&') {
        return t.to_string();
    }
    let mut out = String::with_capacity(t.len());
    let mut reste = t;
    while let Some(i) = reste.find('&') {
        out.push_str(&reste[..i]);
        let apres = &reste[i..];
        let fin = apres.find(';').filter(|&f| f <= 10);
        let remplacement = fin.and_then(|f| {
            let nom = &apres[1..f];
            let c = match nom {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => {
                    let n = if let Some(h) = nom.strip_prefix("#x").or(nom.strip_prefix("#X")) {
                        u32::from_str_radix(h, 16).ok()
                    } else if let Some(d) = nom.strip_prefix('#') {
                        d.parse::<u32>().ok()
                    } else {
                        None
                    };
                    n.and_then(char::from_u32)
                }
            };
            c.map(|c| (c, f))
        });
        match remplacement {
            Some((c, f)) => {
                out.push(c);
                reste = &apres[f + 1..];
            }
            None => {
                out.push('&');
                reste = &apres[1..];
            }
        }
    }
    out.push_str(reste);
    out
}

/// Étape 1 : réparer le texte.
pub fn reparer(t: &str) -> String {
    let mut t = t.to_string();
    if MOJIBAKE.is_match(&t) && t.chars().all(|c| (c as u32) <= 0xFF) {
        let octets: Vec<u8> = t.chars().map(|c| c as u32 as u8).collect();
        if let Ok(repare) = String::from_utf8(octets) {
            t = repare;
        }
    }
    for _ in 0..2 {
        t = decoder_entites(&t);
    }
    // Accent grave ou aigu utilisé comme apostrophe : « Barry Can`t Swim ».
    let t: String = t
        .chars()
        .map(|c| match c {
            '`' | '\u{B4}' | '\u{2018}' | '\u{2019}' => '\'',
            c => c,
        })
        .collect();
    ESPACES.replace_all(&t, " ").trim().to_string()
}

// --- 2. format clé=valeur des radios iHeart ---------------------------------

static IHEART_TEXT: LazyLock<Regex> = LazyLock::new(|| re(r#"\btext="([^"]*)""#));
static IHEART_SPOT: LazyLock<Regex> = LazyLock::new(|| re(r#"\bsong_spot="(\w)""#));
static IHEART_ART: LazyLock<Regex> = LazyLock::new(|| re(r#"\bamgArtworkURL="([^"]*)""#));

struct Iheart {
    artiste: String,
    titre: String,
    musique: bool,
    pochette_fournie: Option<String>,
}

/// La pochette qu'un flux iHeart annonce n'est gardée que si elle vient
/// d'iHeart, en `https://` : `*.iheart.com` (admis par le relais des
/// pochettes, `artwork_proxy::HOTES_AUTORISES`). Toute autre adresse serait
/// refusée par le relais et s'afficherait cassée, label « Cover+ » compris.
pub fn pochette_iheart_admise(adresse: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(adresse.trim()) else {
        return false;
    };
    let hote = url.host_str().unwrap_or_default().to_ascii_lowercase();
    url.scheme() == "https" && (hote == "iheart.com" || hote.ends_with(".iheart.com"))
}

fn iheart(t: &str) -> Option<Iheart> {
    let m = IHEART_TEXT.captures(t)?;
    let tout = m.get(0)?;
    let artiste = t[..tout.start()]
        .trim()
        .trim_end_matches('-')
        .trim()
        .to_string();
    let spot = IHEART_SPOT
        .captures(t)
        .and_then(|c| c.get(1))
        .map(|s| s.as_str().to_string());
    let art = IHEART_ART
        .captures(t)
        .and_then(|c| c.get(1))
        .map(|s| s.as_str().to_string())
        .filter(|a| pochette_iheart_admise(a));
    Some(Iheart {
        artiste,
        titre: m.get(1).map(|s| s.as_str().trim().to_string())?,
        musique: spot.as_deref() == Some("M"),
        pochette_fournie: art,
    })
}

// --- 3. habillages connus en tête --------------------------------------------

static PREFIXES: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)^\s*(?:now playing|now on air|currently playing|playing now|np|en cours|jetzt läuft|nu speelt|musikplayer|on air)\s*(?::+|-|–)\s*",
    )
});
static HABILLAGE_STATION: LazyLock<Regex> = LazyLock::new(|| re(r"^(.{2,40}?)\s*::?\s+(.+ - .+)$"));

// --- 4. ce qu'il ne faut PAS chercher ----------------------------------------

static NON_MUSIQUE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)^(?:stop )?ad ?break\b|^ad \d+$|\bspot block\b|\bstation (?:id|break)\b|\bpromo\b|now playing info goes here|^advert|^werbung|^publicit[eé]|^commercial|^jingle\b|\bartista desconocido\b|^unknown$|^inconnu$|^untitled$|^n/?a$",
    )
});
/// Codes internes de la régie : `2GB_PROMO_01`, `6PR10011-0660`.
static CODE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[\w.]*_[\w.]*$|^[A-Z0-9]{2,}[-_]?\d{3,}[-_\d]*$"));
static ADRESSE: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:https?://|www\.)\S+$"));

/// Une partie de la chaîne qui est le nom de la station, ou son début (6
/// caractères au moins, pour ne pas prendre « Queen » sur Queen FM).
fn est_la_station(partie: &str, nom_station: &str) -> bool {
    let ns = norme(nom_station).replace(' ', "");
    let xs = norme(partie).replace(' ', "");
    !ns.is_empty() && !xs.is_empty() && (xs == ns || (xs.len() >= 6 && ns.starts_with(&xs)))
}

/// Retire de « A - B - C » les parties qui sont la station. `None` s'il ne
/// reste pas au moins deux parties : c'est un slogan.
fn retirer_station(t: &str, nom_station: &str) -> Option<String> {
    if !t.contains(" - ") {
        return Some(t.to_string());
    }
    let parts: Vec<&str> = t.split(" - ").collect();
    let garde: Vec<&str> = parts
        .iter()
        .copied()
        .filter(|x| !est_la_station(x, nom_station))
        .collect();
    if garde.len() == parts.len() {
        return Some(t.to_string());
    }
    (garde.len() >= 2).then(|| garde.join(" - "))
}

fn a_ignorer(t: &str, nom_station: &str) -> Option<&'static str> {
    if t.is_empty() || !t.chars().any(char::is_alphabetic) {
        return Some("vide ou sans lettres");
    }
    if NON_MUSIQUE.is_match(t) {
        return Some("publicité, jingle ou texte de remplissage");
    }
    if CODE.is_match(&t.replace(' ', "")) {
        return Some("code interne de la régie");
    }
    let (ns, nt) = (norme(nom_station), norme(t));
    if !ns.is_empty() && (nt == ns || (ns.len() > 3 && nt.starts_with(&ns) && !t.contains(" - "))) {
        return Some("nom ou slogan de la station");
    }
    if ADRESSE.is_match(t) {
        return Some("adresse web");
    }
    None
}

// --- 5. découpage --------------------------------------------------------------

const SEPARATEURS: [&str; 7] = [" - ", " – ", " — ", " :: ", " | ", " ~ ", " / "];
static PAR_BY: LazyLock<Regex> =
    LazyLock::new(|| re(r#"^"?(.+?)"? by (.+?)(?: from ".*")?(?: on \S.*)?$"#));
static TYPE_SORTIE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)^(?:single|ep|album|lp|maxi|remix|radio edit|edit|.* (?:remix|mix|edit|version))$")
});
static TYPE_VERSION: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)remix|mix|edit|version|live"));
static NUMERO: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^\d{1,3}\.?$|^cd ?\d+$"));
static FEAT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\s+(?:feat\.?|ft\.?|featuring|\[\+\]|vs\.?|x)\s+.*$"));
static PAREN: LazyLock<Regex> = LazyLock::new(|| re(r"\([^)]*\)|\[[^\]]*\]"));
static GUILLEMETS: LazyLock<Regex> = LazyLock::new(|| re("[\"\u{201C}\u{201D}\u{AB}\u{BB}]"));

/// Les parties de la chaîne, et le séparateur reconnu (`None` : aucun).
fn decouper(t: &str) -> (Vec<String>, Option<&'static str>) {
    // « Titre by Artiste [on Station] [from "Album"] », « by » en minuscules.
    if !t.contains(" - ")
        && let Some(m) = PAR_BY.captures(t)
    {
        let titre = m.get(1).map(|s| s.as_str().trim()).unwrap_or_default();
        let artiste = m.get(2).map(|s| s.as_str().trim()).unwrap_or_default();
        return (vec![artiste.to_string(), titre.to_string()], Some("by"));
    }
    // « Artiste - Titre | Label | Réf | Année | Pays » : le premier champ.
    let mut t = t;
    if let Some((premier, _)) = t.split_once(" | ")
        && premier.contains(" - ")
    {
        t = premier;
    }
    for sep in SEPARATEURS {
        if !t.contains(sep) {
            continue;
        }
        let mut out: Vec<String> = Vec::new();
        for p in t.split(sep).map(str::trim) {
            if p.is_empty() || NUMERO.is_match(p) {
                continue;
            }
            if !out.is_empty() && TYPE_SORTIE.is_match(p) {
                if TYPE_VERSION.is_match(p)
                    && let Some(der) = out.last_mut()
                {
                    *der = format!("{der} ({p})");
                }
                continue;
            }
            out.push(p.to_string());
        }
        if out.len() >= 2 {
            out.truncate(4);
            return (out, Some(sep.trim()));
        }
    }
    (vec![t.to_string()], None)
}

fn nettoyer_pour_requete(p: &str) -> String {
    let p = PAREN.replace_all(p, " ");
    let p = FEAT.replace(&p, "");
    let p = GUILLEMETS.replace_all(&p, " ");
    ESPACES
        .replace_all(&p, " ")
        .trim_matches(|c: char| c == ' ' || c == '-')
        .to_string()
}

/// Ce qu'il faut faire d'une métadonnée ICY.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preparation {
    /// `Some(raison)` : rien à chercher (publicité, slogan, vide…).
    pub ignorer: Option<&'static str>,
    /// Les requêtes iTunes, dans l'ordre où les essayer.
    pub requetes: Vec<String>,
    /// Les hypothèses (artiste, titre) pour juger, dans l'ordre. Vide : la
    /// chaîne n'a pas de séparateur, et seule une égalité exacte compte.
    pub hypotheses: Vec<(String, String)>,
    /// La 1re requête porte le nom du remixeur (§ 2.2, étape 6) : elle ne
    /// part que dans la boutique de la station (§ 3.2, étape 1).
    pub avec_remixeur: bool,
    /// La pochette que le flux iHeart fournit lui-même (repli).
    pub pochette_fournie: Option<String>,
    /// Le texte réparé, sans habillage ni nom de station. (Le code de
    /// référence garde ici le texte réparé AVANT ce retrait ; il ne sert qu'à
    /// la concordance « sans séparateur », que l'habillage ferait échouer.)
    pub texte: String,
}

/// `preparer(brut, nom_station)` du code de référence.
pub fn preparer(brut: &str, nom_station: &str) -> Preparation {
    let mut t = reparer(brut);
    let mut r = Preparation {
        texte: t.clone(),
        ..Default::default()
    };
    let parts: Vec<String>;
    if let Some(ih) = iheart(&t) {
        if !ih.musique {
            r.ignorer = Some("iHeart : pas un morceau");
            return r;
        }
        r.pochette_fournie = ih.pochette_fournie;
        parts = vec![ih.artiste, ih.titre];
    } else {
        t = PREFIXES.replace(&t, "").into_owned();
        // Habillage qui contient le nom de la station : « Nu op Novo3 : Smokie - … ».
        let ns = norme(nom_station);
        let sans_habillage = HABILLAGE_STATION
            .captures(&t)
            .filter(|m| {
                !ns.is_empty()
                    && norme(m.get(1).map(|g| g.as_str()).unwrap_or_default()).contains(&ns)
            })
            .and_then(|m| m.get(2).map(|g| g.as_str().to_string()));
        if let Some(reste) = sans_habillage {
            t = reste;
        }
        let Some(reste) = retirer_station(&t, nom_station) else {
            r.ignorer = Some("nom ou slogan de la station");
            return r;
        };
        t = reste;
        r.texte = t.clone();
        r.ignorer = a_ignorer(&t, nom_station);
        if r.ignorer.is_some() {
            return r;
        }
        parts = decouper(&t).0;
    }
    let propres: Vec<String> = parts.iter().map(|p| nettoyer_pour_requete(p)).collect();
    if parts.len() == 1 {
        r.requetes = propres;
        return r;
    }
    // L'ordre usuel d'abord, puis l'inverse (« TITRE - ARTISTE » existe : il
    // est systématique sur Oüi FM), puis les autres paires.
    let mut hyp = vec![
        (parts[0].clone(), parts[1].clone()),
        (parts[1].clone(), parts[0].clone()),
    ];
    for i in 0..parts.len() {
        for j in 0..parts.len() {
            let paire = (parts[i].clone(), parts[j].clone());
            if i != j && !hyp.contains(&paire) {
                hyp.push(paire);
            }
        }
    }
    r.hypotheses = hyp;
    let base = propres.join(" ");
    // Remix : d'abord avec le nom du remixeur, sans quoi la recherche ne
    // ramène pas le remix.
    let rmx: Vec<String> = parts
        .iter()
        .flat_map(|p| remixeurs(&version_diffusee(p)))
        .collect();
    if !rmx.is_empty() {
        r.requetes.push(format!("{base} {}", rmx.join(" ")));
        r.avec_remixeur = true;
    }
    // Plus de deux parties : d'abord les deux premières seules.
    if parts.len() > 2 {
        r.requetes.push(propres[..2].join(" "));
    }
    r.requetes.push(base);
    r
}
