//! Le rapport `foo_dr.txt` du DR Meter de foobar2000, lu comme TROISIÈME
//! source de plage dynamique (#4186).
//!
//! Tades (fil 1800) : « Quand ils ont été mesurés on les trouve généralement
//! dans un fichier `foo_dr.txt` (pas dans les tags) ». Le composant DR Meter
//! de foobar2000 — comme le TT DR Offline Meter dont il reprend la mise en
//! page — écrit son rapport dans le DOSSIER de l'album, et n'écrit rien dans
//! les fichiers. Tune ne connaissait que deux producteurs de `dr_track` : le
//! tag `DYNAMIC RANGE` lu au scan (`read_extended_metadata`) et sa propre
//! passe d'analyse (`audio::replaygain`). Une mesure déjà faite, posée à côté
//! des fichiers, lui restait invisible.
//!
//! # Ce module est PUR
//!
//! Il lit du texte et rend des lignes ; il n'ouvre ni base ni fichier audio.
//! Le seul point d'entrée qui touche le disque est [`rapport_voisin`], qui
//! cherche `foo_dr.txt` à côté d'un fichier audio. Le branchement dans le scan
//! est dans `read_extended_metadata`, la précédence y est écrite : le tag du
//! fichier prime, ce rapport ne comble que le vide, et la passe d'analyse ne
//! vient qu'après (`replaygain::peut_ecrire_le_dr`).
//!
//! # Le format, tel qu'il est vraiment
//!
//! Pièce jointe du fil 1800 (DR Meter v1.0.8, CRLF, UTF-8) :
//!
//! ```text
//! foobar2000 v2.25.10 / DR Meter v1.0.8
//! log date: 2026-09-14 07:00:23
//!
//! --------------------------------------------------------------------------------
//! Analyzed: Stokowski LSO / Mahler Symphony 2
//! --------------------------------------------------------------------------------
//!
//! DR         Peak           RMS       Duration Track                                    DR (FL)      DR (FR) …
//! --------------------------------------------------------------------------------
//! DR11      -6.69 dBFS   -24.47 dBFS     23:15 01-Mahler Sym No 2  1st Mov Alle(…)     11.05 dB     11.89 dB …
//! --------------------------------------------------------------------------------
//!
//! Number of tracks:  22
//! Official DR value: DR9
//! ```
//!
//! Quatre traits qui ne s'inventent pas, tous relevés sur cette pièce :
//!
//! 1. **Colonnes à largeur fixe**, sans délimiteur. Les quatre premières
//!    (`DR`, `Peak`, `RMS`, `Duration`) se lisent par jetons ; la colonne
//!    `Track` prend le reste de la ligne — un titre contient des espaces.
//! 2. **Titres tronqués** à largeur fixe et suffixés `(…)` :
//!    `01-Mahler Sym No 2  1st Mov Alle(…)`. Un appariement par titre EXACT est
//!    donc impossible ; le numéro de piste est le préfixe `01-` de la chaîne.
//! 3. **Un SACD y a ses deux couches** : les mêmes 11 pistes en stéréo, puis
//!    en multicanal, avec des DR différents (DR11 contre DR7 sur la première).
//!    Rien ne dit laquelle est dans Tune — sauf les colonnes `DR (FC)`,
//!    `DR (LFE)`, `DR (BL)`, `DR (BR)`, vides sur les lignes stéréo. C'est
//!    [`LigneDr::multicanal`], et c'est le nombre de canaux du fichier audio
//!    qui départage à l'appariement.
//! 4. **`Official DR value: DR9`** est un DR d'ALBUM — qui, ici, agrège les
//!    deux couches. Il est lu ([`RapportDr::dr_album`]) mais le scan ne
//!    l'écrit PAS dans `dr_album` : voir la note dans `read_extended_metadata`.
//!
//! Le rapport n'est jamais rejeté pour son encodage : BOM UTF-8 retiré, UTF-8
//! sinon, Windows-1252 en repli — l'outil tourne sous Windows, et un accent
//! dans un titre ne doit pas faire disparaître les 22 mesures de l'album.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// Le nom que le composant DR Meter donne à son rapport, dans le dossier de
/// l'album. Cherché tel quel : c'est le nom que foobar2000 écrit, et c'est
/// aussi celui du fil 1800.
pub const NOM_DU_RAPPORT: &str = "foo_dr.txt";

/// Une ligne de mesure du rapport : une piste, telle que le mesureur l'a vue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LigneDr {
    /// La plage dynamique, en entier — `DR11` → 11.
    pub dr: u8,
    /// Le numéro de piste, lu en tête de la colonne `Track` (`01-…`, `01. …`,
    /// `01 …`). `None` quand la colonne ne commence pas par un nombre.
    pub numero: Option<u32>,
    /// Le numéro de disque, quand TOUTES les lignes du rapport sont de la
    /// forme `D-NN …` (voir [`analyser`]). Sinon `None`.
    pub disque: Option<u32>,
    /// Le titre, après le numéro et sans le suffixe `(…)` de troncature.
    pub titre: String,
    /// Vrai quand le mesureur a coupé le titre (`(…)` en fin de colonne) :
    /// il ne peut alors servir qu'en PRÉFIXE du vrai titre.
    pub tronque: bool,
    /// Vrai quand la ligne porte des mesures au-delà des deux canaux avant —
    /// la couche multicanal d'un SACD, ou un fichier 5.1.
    pub multicanal: bool,
}

/// Un rapport lu : ses lignes de piste et, s'il l'annonce, son DR d'album.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RapportDr {
    /// `Official DR value: DR9`. Lu, pas écrit : voir le module.
    pub dr_album: Option<u8>,
    pub lignes: Vec<LigneDr>,
}

/// `01-Titre`, `01. Titre`, `01 - Titre`, `01 Titre`, `01_Titre`.
///
/// Un à trois chiffres, puis un séparateur ou une espace : `2001 A Space
/// Odyssey` ne commence PAS par un numéro de piste (quatre chiffres collés),
/// et `12 Bars Blues` n'est un numéro que si la ligne est reconnue comme
/// telle — voir le repli par titre dans [`RapportDr::dr_pour_la_piste`].
static NUMERO_EN_TETE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,3})(?:\s*[-–—._)]\s*|\s+)").expect("regex figée"));

/// `1-01 Titre`, `2.03 Titre` — disque puis piste, forme des coffrets.
static DISQUE_ET_NUMERO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{1,2})[-.](\d{2})(?:\s*[-–—._)]\s*|\s+)").expect("regex figée")
});

/// `23:15`, `1:02:03`.
static DUREE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{1,3}:\d{2}(?::\d{2})?$").expect("regex figée"));

/// Une extension audio en queue de titre : `dr14_tmeter` et certaines
/// versions du mesureur écrivent le NOM DE FICHIER dans la colonne `Track`.
static EXTENSION_AUDIO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\.(flac|mp3|m4a|aac|alac|ogg|oga|opus|wav|wv|ape|aif|aiff|dsf|dff|dsd|mpc|tta|wma)$",
    )
    .expect("regex figée")
});

/// Décode le rapport quel que soit son encodage, puis l'analyse.
///
/// Même règle que `scanner::cue::parse_cue_bytes` : permissif, ne rejette
/// jamais. BOM UTF-8 retiré ; UTF-8 valide pris tel quel ; sinon Windows-1252
/// — l'encodage « ANSI » de l'outil sous Windows, où `é` est l'octet `0xE9`
/// et `’` l'octet `0x92`.
pub fn analyser_octets(octets: &[u8]) -> RapportDr {
    let corps = octets.strip_prefix(b"\xef\xbb\xbf").unwrap_or(octets);
    match std::str::from_utf8(corps) {
        Ok(texte) => analyser(texte),
        Err(_) => analyser(&decoder_windows_1252(corps)),
    }
}

/// Windows-1252 : Latin-1 plus la plage `0x80..=0x9F`, où vivent les
/// guillemets typographiques, les points de suspension et le `€` qu'un titre
/// tapé sous Windows contient couramment. Les cinq positions non assignées
/// rendent U+FFFD, jamais une erreur.
fn decoder_windows_1252(octets: &[u8]) -> String {
    const HAUT: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    octets
        .iter()
        .map(|&o| match o {
            0x80..=0x9F => HAUT[(o - 0x80) as usize],
            _ => o as char,
        })
        .collect()
}

/// Analyse le texte d'un rapport. Ne rend jamais d'erreur : une ligne qui
/// n'est pas une mesure est ignorée, et un rapport sans aucune mesure rend
/// un [`RapportDr`] vide.
pub fn analyser(texte: &str) -> RapportDr {
    let mut rapport = RapportDr::default();
    // Le mesureur n'ajoute les colonnes par canal que quand il en a. Ce
    // n'est qu'à cette condition que des `dB` en queue de ligne sont des
    // colonnes et non la fin d'un titre.
    let mut entete_multicanal = false;
    let mut brutes: Vec<LigneBrute> = Vec::new();
    for brute in texte.lines() {
        let ligne = brute.trim_end_matches('\r');
        let propre = ligne.trim();
        if propre.is_empty() {
            continue;
        }
        if let Some(reste) = propre.strip_prefix("Official DR value:") {
            rapport.dr_album = valeur_dr(reste.trim());
            continue;
        }
        if propre.starts_with("DR") && propre.contains("Peak") && propre.contains("RMS") {
            entete_multicanal = propre.contains("(FL)") || propre.contains("(FR)");
            continue;
        }
        if let Some(l) = analyser_ligne(ligne, entete_multicanal) {
            brutes.push(l);
        }
    }
    // Coffrets : `1-01 Titre`. La forme disque-piste n'est retenue que si
    // TOUTES les lignes la portent — sur une seule, `01-12 Bars Blues` serait
    // lu « disque 1, piste 12 » et apparié à la mauvaise piste.
    let en_disque_piste =
        !brutes.is_empty() && brutes.iter().all(|l| DISQUE_ET_NUMERO.is_match(&l.piste));
    rapport.lignes = brutes
        .into_iter()
        .map(|l| {
            let (disque, numero, titre) = match (en_disque_piste, NUMERO_EN_TETE.captures(&l.piste))
            {
                (true, _) => {
                    let c = DISQUE_ET_NUMERO
                        .captures(&l.piste)
                        .expect("la forme a été vérifiée sur toutes les lignes");
                    (
                        c[1].parse().ok(),
                        c[2].parse().ok(),
                        l.piste[c[0].len()..].to_string(),
                    )
                }
                (false, Some(c)) => (None, c[1].parse().ok(), l.piste[c[0].len()..].to_string()),
                (false, None) => (None, None, l.piste.clone()),
            };
            LigneDr {
                dr: l.dr,
                numero,
                disque,
                titre: titre.trim().to_string(),
                tronque: l.tronque,
                multicanal: l.multicanal,
            }
        })
        .collect();
    rapport
}

/// Une ligne lue, avant que la colonne `Track` soit découpée en numéro et
/// titre — ce découpage dépend de l'ENSEMBLE des lignes (coffrets).
struct LigneBrute {
    dr: u8,
    piste: String,
    tronque: bool,
    multicanal: bool,
}

/// `DR11` → 11. Tolère `DR 11`, `dr11`, `11`.
fn valeur_dr(jeton: &str) -> Option<u8> {
    let t = jeton.trim();
    let corps = t
        .strip_prefix("DR")
        .or_else(|| t.strip_prefix("dr"))
        .or_else(|| t.strip_prefix("Dr"))
        .unwrap_or(t)
        .trim();
    if corps.is_empty() || !corps.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    corps.parse().ok()
}

/// Un curseur de jetons qui garde la POSITION : la colonne `Track` est le
/// reste de la ligne à partir d'un octet, pas une suite de mots.
struct Jetons<'a> {
    ligne: &'a str,
    pos: usize,
}

impl<'a> Jetons<'a> {
    fn suivant(&mut self) -> Option<&'a str> {
        let reste = &self.ligne[self.pos..];
        let debut = reste.find(|c: char| !c.is_whitespace())?;
        let apres = reste[debut..]
            .find(char::is_whitespace)
            .map_or(reste.len(), |i| debut + i);
        self.pos += apres;
        Some(&reste[debut..apres])
    }

    fn regarder(&self) -> Option<&'a str> {
        let mut copie = Jetons {
            ligne: self.ligne,
            pos: self.pos,
        };
        copie.suivant()
    }

    fn reste(&self) -> &'a str {
        &self.ligne[self.pos..]
    }
}

fn est_un_nombre(jeton: &str) -> bool {
    let corps = jeton
        .strip_prefix('-')
        .or_else(|| jeton.strip_prefix('+'))
        .unwrap_or(jeton);
    !corps.is_empty()
        && corps.chars().all(|c| c.is_ascii_digit() || c == '.')
        && corps.chars().any(|c| c.is_ascii_digit())
}

fn est_une_unite(jeton: &str) -> bool {
    matches!(jeton, "dB" | "dBFS" | "dBTP" | "db" | "dbfs")
}

/// Une mesure : `-6.69 dBFS`, ou `-6.69dBFS` collé, ou un nombre nu.
fn consommer_mesure(jetons: &mut Jetons<'_>) -> bool {
    let Some(j) = jetons.regarder() else {
        return false;
    };
    if est_un_nombre(j) {
        jetons.suivant();
        if jetons.regarder().is_some_and(est_une_unite) {
            jetons.suivant();
        }
        return true;
    }
    // `-6.69dBFS` en un seul jeton.
    let sans_unite = j
        .strip_suffix("dBFS")
        .or_else(|| j.strip_suffix("dB"))
        .unwrap_or("");
    if est_un_nombre(sans_unite) {
        jetons.suivant();
        return true;
    }
    false
}

fn analyser_ligne(ligne: &str, entete_multicanal: bool) -> Option<LigneBrute> {
    let mut jetons = Jetons { ligne, pos: 0 };
    let premier = jetons.suivant()?;
    // `DR` nu est l'en-tête, `DR11` une mesure. `----` et `Analyzed:` ne
    // commencent pas par DR.
    if !(premier.starts_with("DR") || premier.starts_with("dr")) || premier.len() < 3 {
        return None;
    }
    let dr = valeur_dr(premier)?;
    // Peak puis RMS, obligatoires : sans eux ce n'est pas une ligne de mesure.
    if !consommer_mesure(&mut jetons) || !consommer_mesure(&mut jetons) {
        return None;
    }
    // Duration, facultative (absente de certains rapports).
    if jetons.regarder().is_some_and(|j| DUREE.is_match(j)) {
        jetons.suivant();
    }
    let mut piste = jetons.reste().trim().to_string();
    if piste.is_empty() {
        return None;
    }

    // Les colonnes par canal, en QUEUE de ligne — `11.05 dB     11.89 dB
    // … -24.50 dBFS  -24.45 dBFS`. Comptées pour savoir si la ligne dépasse
    // les deux canaux avant, puis retirées du titre.
    let mut colonnes_dr = 0usize;
    if entete_multicanal {
        while let Some((reste, unite)) = detacher_mesure_en_queue(&piste) {
            if unite == "dB" {
                colonnes_dr += 1;
            }
            piste = reste;
        }
    }
    let multicanal = colonnes_dr > 2;

    let mut tronque = false;
    for suffixe in ["(…)", "(...)", "…"] {
        if let Some(sans) = piste.strip_suffix(suffixe) {
            piste = sans.to_string();
            tronque = true;
            break;
        }
    }
    Some(LigneBrute {
        dr,
        piste: piste.trim().to_string(),
        tronque,
        multicanal,
    })
}

/// Retire une mesure `<nombre> <unité>` en fin de chaîne et rend (le reste,
/// l'unité). `None` si la queue n'en est pas une.
fn detacher_mesure_en_queue(s: &str) -> Option<(String, &'static str)> {
    let t = s.trim_end();
    let (avant_unite, unite) = if let Some(r) = t.strip_suffix("dBFS") {
        (r, "dBFS")
    } else {
        (t.strip_suffix("dB")?, "dB")
    };
    let avant_unite = avant_unite.trim_end();
    let debut_nombre = avant_unite.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let nombre = &avant_unite[debut_nombre..];
    if !est_un_nombre(nombre) {
        return None;
    }
    // Une mesure est une COLONNE : il faut une espace avant, sinon c'est la
    // fin d'un titre (`Song2.0dB`).
    if debut_nombre == 0 {
        return None;
    }
    Some((avant_unite[..debut_nombre].trim_end().to_string(), unite))
}

/// La forme sous laquelle deux titres se comparent : minuscules, extension
/// audio retirée, toute ponctuation et toute suite d'espaces ramenées à une
/// espace. Même esprit que `cue_album::index_du_dossier`, qui compare en
/// minuscules parce qu'un même arbre recopié sur un NAS change de casse.
pub fn normaliser_titre(titre: &str) -> String {
    let sans_extension = EXTENSION_AUDIO.replace(titre.trim(), "");
    let mut sortie = String::with_capacity(sans_extension.len());
    let mut espace_en_attente = false;
    for c in sans_extension.chars() {
        if c.is_alphanumeric() {
            if espace_en_attente && !sortie.is_empty() {
                sortie.push(' ');
            }
            espace_en_attente = false;
            sortie.extend(c.to_lowercase());
        } else {
            espace_en_attente = true;
        }
    }
    sortie
}

/// Le numéro de piste en tête d'un nom de fichier (`03 - Titre.flac`) —
/// le repli quand le fichier audio n'a pas de tag de numéro.
pub fn numero_dans_le_nom(nom_sans_extension: &str) -> Option<u32> {
    NUMERO_EN_TETE
        .captures(nom_sans_extension.trim())
        .and_then(|c| c[1].parse().ok())
}

impl RapportDr {
    /// Le DR d'UNE piste de l'album, ou `None` si le rapport ne permet pas
    /// de la désigner sans ambiguïté.
    ///
    /// L'appariement se fait dans cet ordre, et chaque étape ne RESSERRE que
    /// si elle laisse au moins une ligne :
    ///
    /// 1. par **numéro de piste** (et de disque, si le rapport en porte) ;
    ///    à défaut de numéro, ou si aucune ligne ne le porte, par **titre**
    ///    normalisé — égal, ou en préfixe quand le mesureur a tronqué ;
    /// 2. quand plusieurs lignes restent, par **titre** ;
    /// 3. quand plusieurs lignes restent encore, par **couche** : les lignes
    ///    multicanal pour un fichier de plus de deux canaux, les autres pour
    ///    un fichier stéréo — c'est le SACD à deux couches du fil 1800 ;
    /// 4. les lignes restantes doivent porter UN SEUL DR. Deux valeurs
    ///    différentes, c'est une ambiguïté, et on ne tire pas au sort : un
    ///    DR7 posé sur une piste qui vaut DR11 serait pire qu'aucun.
    pub fn dr_pour_la_piste(
        &self,
        numero: Option<u32>,
        disque: Option<u32>,
        titre: Option<&str>,
        canaux: Option<u16>,
    ) -> Option<u8> {
        let titre_norme = titre.map(normaliser_titre).filter(|t| !t.is_empty());
        let correspond_au_titre = |l: &LigneDr| -> bool {
            let Some(voulu) = titre_norme.as_deref() else {
                return false;
            };
            let sien = normaliser_titre(&l.titre);
            if sien.is_empty() {
                return false;
            }
            if l.tronque {
                voulu.starts_with(&sien)
            } else {
                voulu == sien
            }
        };

        let rapport_a_des_disques = self.lignes.iter().any(|l| l.disque.is_some());
        let mut candidates: Vec<&LigneDr> = match numero {
            Some(n) => self
                .lignes
                .iter()
                .filter(|l| l.numero == Some(n))
                .filter(|l| !rapport_a_des_disques || disque.is_none() || l.disque == disque)
                .collect(),
            None => Vec::new(),
        };
        if candidates.is_empty() {
            candidates = self
                .lignes
                .iter()
                .filter(|l| correspond_au_titre(l))
                .collect();
        }
        if candidates.len() > 1 {
            let par_titre: Vec<&LigneDr> = candidates
                .iter()
                .copied()
                .filter(|l| correspond_au_titre(l))
                .collect();
            if !par_titre.is_empty() {
                candidates = par_titre;
            }
        }
        if candidates.len() > 1
            && let Some(c) = canaux
        {
            let voulu_multicanal = c > 2;
            let par_couche: Vec<&LigneDr> = candidates
                .iter()
                .copied()
                .filter(|l| l.multicanal == voulu_multicanal)
                .collect();
            if !par_couche.is_empty() {
                candidates = par_couche;
            }
        }
        let valeurs: HashSet<u8> = candidates.iter().map(|l| l.dr).collect();
        if valeurs.len() == 1 {
            valeurs.into_iter().next()
        } else {
            None
        }
    }
}

/// Le rapport posé à côté d'un fichier audio, s'il y en a un.
///
/// `None` quand il n'y a pas de `foo_dr.txt` dans le dossier — le cas de
/// l'immense majorité des bibliothèques, et il ne coûte qu'un `stat`. `None`
/// aussi quand le fichier existe mais ne contient aucune ligne de mesure :
/// un rapport vide n'est pas une source.
pub fn rapport_voisin(fichier_audio: &Path) -> Option<RapportDr> {
    let chemin = fichier_audio.parent()?.join(NOM_DU_RAPPORT);
    let octets = std::fs::read(&chemin).ok()?;
    let rapport = analyser_octets(&octets);
    if rapport.lignes.is_empty() {
        return None;
    }
    Some(rapport)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La pièce jointe du fil 1800, mot pour mot (CRLF compris), ramenée à
    /// trois pistes par couche pour rester lisible. Les largeurs de colonnes
    /// et les `(…)` sont ceux du fichier.
    const TADES: &str = "foobar2000 v2.25.10 / DR Meter v1.0.8\r\n\
log date: 2026-09-14 07:00:23\r\n\
\r\n\
--------------------------------------------------------------------------------\r\n\
Analyzed: Stokowski LSO / Mahler Symphony 2 \r\n\
--------------------------------------------------------------------------------\r\n\
\r\n\
DR         Peak           RMS       Duration Track                                    DR (FL)      DR (FR)      DR (FC)     DR (LFE)      DR (BL)      DR (BR)     RMS (FL)     RMS (FR)     RMS (FC)    RMS (LFE)     RMS (BL)     RMS (BR)\r\n\
--------------------------------------------------------------------------------\r\n\
DR11      -6.69 dBFS   -24.47 dBFS     23:15 01-Mahler Sym No 2  1st Mov Alle(…)     11.05 dB     11.89 dB                                                      -24.50 dBFS  -24.45 dBFS                                                    \r\n\
DR12      -9.52 dBFS   -28.03 dBFS     10:18 02-Mahler Sym No 2  2nd Mov Anda(…)     11.31 dB     12.30 dB                                                      -28.11 dBFS  -27.95 dBFS                                                    \r\n\
DR10      -6.45 dBFS   -20.60 dBFS      8:48 11-Mahler Sym No 2  5th Mov Etwa(…)      9.71 dB      9.60 dB                                                      -20.47 dBFS  -20.73 dBFS                                                    \r\n\
DR7       -8.86 dBFS   -27.42 dBFS     23:15 01-Mahler Sym No 2  1st Mov Alle(…)     10.73 dB     10.02 dB      0.00 dB      0.00 dB     11.03 dB     10.74 dB  -25.61 dBFS  -25.84 dBFS -139.55 dBFS -139.55 dBFS  -26.44 dBFS  -24.89 dBFS\r\n\
DR8       -9.81 dBFS   -30.65 dBFS     10:18 02-Mahler Sym No 2  2nd Mov Anda(…)     12.13 dB     12.40 dB      0.00 dB      0.00 dB     11.95 dB     11.05 dB  -29.69 dBFS  -30.36 dBFS -139.55 dBFS -139.55 dBFS  -29.27 dBFS  -27.03 dBFS\r\n\
DR18      -8.19 dBFS   -25.42 dBFS      8:47 11-Mahler Sym No 2  5th Mov Etwa(…)      9.25 dB      9.14 dB     34.80 dB     34.80 dB     10.79 dB      9.93 dB  -22.69 dBFS  -22.87 dBFS -133.39 dBFS -133.39 dBFS  -26.21 dBFS  -23.70 dBFS\r\n\
--------------------------------------------------------------------------------\r\n\
\r\n\
Number of tracks:  22\r\n\
Official DR value: DR9\r\n\
\r\n\
Samplerate:        88200 Hz\r\n\
Channels:          2, 6\r\n\
Bits per sample:   1\r\n\
Bitrate:           11294 kbps\r\n\
Codec:             DST64\r\n\
================================================================================\r\n";

    /// Un rapport stéréo ordinaire, tel que le DR Meter l'écrit pour un
    /// album CD : ni colonnes par canal, ni troncature, unité `dB`.
    const STEREO: &str = "foobar2000 v1.6.2 / Dynamic Range Meter 1.1.1\n\
log date: 2021-03-03 20:11:02\n\
\n\
--------------------------------------------------------------------------------\n\
Analyzed: Autechre / Amber\n\
--------------------------------------------------------------------------------\n\
\n\
DR         Peak         RMS     Duration Track\n\
--------------------------------------------------------------------------------\n\
DR11      -0.20 dB   -14.53 dB      4:12 01-Foil\n\
DR12      -0.10 dB   -15.02 dB      6:00 02-Montreal\n\
DR9       -0.30 dB   -12.75 dB      5:25 03-Silverside\n\
--------------------------------------------------------------------------------\n\
\n\
Number of tracks:  3\n\
Official DR value: DR11\n\
\n\
Samplerate:        44100 Hz\n\
Channels:          2\n\
Bits per sample:   16\n\
Bitrate:           951 kbps\n\
Codec:             FLAC\n\
================================================================================\n";

    #[test]
    fn lit_la_piece_jointe_du_fil_1800_avec_ses_deux_couches() {
        let r = analyser(TADES);
        assert_eq!(r.dr_album, Some(9), "Official DR value: DR9");
        assert_eq!(r.lignes.len(), 6, "trois pistes par couche : {r:#?}");
        let premiere = &r.lignes[0];
        assert_eq!(premiere.dr, 11);
        assert_eq!(premiere.numero, Some(1));
        assert_eq!(premiere.titre, "Mahler Sym No 2  1st Mov Alle");
        assert!(premiere.tronque, "le `(…)` marque un titre coupé");
        assert!(!premiere.multicanal, "deux colonnes DR seulement : stéréo");
        let couche_2 = &r.lignes[3];
        assert_eq!(couche_2.dr, 7);
        assert_eq!(couche_2.numero, Some(1));
        assert!(couche_2.multicanal, "six colonnes DR : multicanal");
        // La ligne aberrante (DR18, canaux FC/LFE silencieux à 34,80 dB) est
        // LUE telle quelle : on rapporte ce que le mesureur a écrit.
        assert_eq!(r.lignes[5].dr, 18);
        assert!(r.lignes[5].multicanal);
    }

    /// La pièce jointe ENTIÈRE, telle que téléchargée (6 232 octets, CRLF),
    /// posée en fixture : 22 lignes, deux couches, et la valeur aberrante.
    #[test]
    fn la_piece_jointe_entiere_donne_ses_vingt_deux_lignes() {
        let octets = include_bytes!("../../tests/fixtures/foo_dr_tades_1800.txt");
        assert!(octets.contains(&b'\r'), "la fixture doit garder ses CRLF");
        let r = analyser_octets(octets);
        assert_eq!(r.lignes.len(), 22, "Number of tracks: 22");
        assert_eq!(r.dr_album, Some(9));
        let stereo: Vec<&LigneDr> = r.lignes.iter().filter(|l| !l.multicanal).collect();
        let multi: Vec<&LigneDr> = r.lignes.iter().filter(|l| l.multicanal).collect();
        assert_eq!(stereo.len(), 11);
        assert_eq!(multi.len(), 11);
        assert_eq!(
            stereo.iter().map(|l| l.numero).collect::<Vec<_>>(),
            (1..=11).map(Some).collect::<Vec<_>>()
        );
        assert_eq!(
            stereo.iter().map(|l| l.dr).collect::<Vec<_>>(),
            [11, 12, 12, 12, 10, 9, 10, 10, 12, 11, 10]
        );
        assert_eq!(
            multi.iter().map(|l| l.dr).collect::<Vec<_>>(),
            [7, 8, 8, 8, 6, 6, 7, 6, 7, 8, 18]
        );
        assert!(
            r.lignes.iter().all(|l| l.tronque),
            "tous les titres sont coupés"
        );
        // Chaque piste stéréo se retrouve par son numéro et ses deux canaux.
        for (i, attendu) in [11, 12, 12, 12, 10, 9, 10, 10, 12, 11, 10]
            .iter()
            .enumerate()
        {
            assert_eq!(
                r.dr_pour_la_piste(Some(i as u32 + 1), None, None, Some(2)),
                Some(*attendu),
                "piste {}",
                i + 1
            );
        }
    }

    #[test]
    fn un_fichier_stereo_prend_la_couche_stereo_et_un_multicanal_l_autre() {
        let r = analyser(TADES);
        // Piste 1, fichier stéréo : DR11, pas DR7.
        assert_eq!(r.dr_pour_la_piste(Some(1), None, None, Some(2)), Some(11));
        // Le même morceau en 5.1 : DR7.
        assert_eq!(r.dr_pour_la_piste(Some(1), None, None, Some(6)), Some(7));
        // Piste 11 en stéréo : DR10 — et non le DR18 de la couche multicanal.
        assert_eq!(
            r.dr_pour_la_piste(
                Some(11),
                None,
                Some("Mahler Sym No 2  5th Mov Etwas"),
                Some(2)
            ),
            Some(10)
        );
    }

    #[test]
    fn deux_couches_sans_nombre_de_canaux_est_une_ambiguite_et_ne_tire_pas_au_sort() {
        let r = analyser(TADES);
        assert_eq!(
            r.dr_pour_la_piste(Some(1), None, None, None),
            None,
            "DR11 et DR7 pour la même piste, rien pour départager : aucun DR"
        );
    }

    #[test]
    fn un_rapport_stereo_ordinaire_s_apparie_par_numero() {
        let r = analyser(STEREO);
        assert_eq!(r.dr_album, Some(11));
        assert_eq!(r.lignes.len(), 3);
        assert!(r.lignes.iter().all(|l| !l.tronque && !l.multicanal));
        assert_eq!(r.lignes[1].titre, "Montreal");
        assert_eq!(r.dr_pour_la_piste(Some(2), None, None, Some(2)), Some(12));
        assert_eq!(
            r.dr_pour_la_piste(Some(3), None, Some("Silverside"), Some(2)),
            Some(9)
        );
        assert_eq!(
            r.dr_pour_la_piste(Some(4), None, None, Some(2)),
            None,
            "pas de piste 4"
        );
    }

    #[test]
    fn sans_numero_de_piste_le_titre_apparie_meme_tronque() {
        let r = analyser(TADES);
        // Titre entier contre un titre coupé par le mesureur : préfixe.
        assert_eq!(
            r.dr_pour_la_piste(
                None,
                None,
                Some("Mahler Sym No 2: 2nd Mov Andante moderato"),
                Some(2)
            ),
            Some(12)
        );
        // Un titre qui n'y est pas.
        assert_eq!(
            r.dr_pour_la_piste(None, None, Some("Kindertotenlieder"), Some(2)),
            None
        );
        // Rapport stéréo, titre exact, casse différente.
        let s = analyser(STEREO);
        assert_eq!(
            s.dr_pour_la_piste(None, None, Some("MONTREAL"), None),
            Some(12)
        );
        // Un titre entier ne s'apparie PAS en préfixe sur une ligne non
        // tronquée : `Foil` n'est pas `Foiled Again`.
        assert_eq!(
            s.dr_pour_la_piste(None, None, Some("Foiled Again"), None),
            None
        );
    }

    #[test]
    fn le_numero_de_piste_prime_sur_un_titre_qui_ne_colle_pas() {
        // Le tag TITLE du fichier et la colonne du mesureur divergent (le
        // mesureur a pris le nom de fichier) : le numéro suffit.
        let s = analyser(STEREO);
        assert_eq!(
            s.dr_pour_la_piste(Some(1), None, Some("Foil (2021 remaster)"), Some(2)),
            Some(11)
        );
    }

    #[test]
    fn crlf_bom_et_windows_1252_ne_font_perdre_aucune_ligne() {
        // CRLF : déjà le cas de TADES. BOM UTF-8 devant :
        let mut avec_bom = b"\xef\xbb\xbf".to_vec();
        avec_bom.extend_from_slice(STEREO.as_bytes());
        let r = analyser_octets(&avec_bom);
        assert_eq!(r.lignes.len(), 3, "le BOM ne doit pas masquer l'en-tête");
        assert_eq!(r.dr_album, Some(11));

        // Windows-1252 : `é` = 0xE9, `’` = 0x92, `…` = 0x85.
        let mut cp1252 = Vec::new();
        cp1252.extend_from_slice(b"DR         Peak         RMS     Duration Track\n");
        cp1252.extend_from_slice(b"DR13      -0.50 dB   -16.00 dB      3:30 01-Pr\xe9lude \xe0 l\x92apr\xe8s-midi d\x92un faune\n");
        cp1252.extend_from_slice(b"DR12      -0.40 dB   -15.00 dB      3:31 02-Nocturne(\x85)\n");
        cp1252.extend_from_slice(b"Official DR value: DR12\n");
        let r = analyser_octets(&cp1252);
        assert_eq!(
            r.lignes.len(),
            2,
            "un accent Windows ne perd pas la ligne : {r:#?}"
        );
        assert_eq!(r.lignes[0].titre, "Prélude à l’après-midi d’un faune");
        assert_eq!(r.lignes[1].titre, "Nocturne");
        assert!(
            r.lignes[1].tronque,
            "le `…` de Windows-1252 (0x85) est aussi une troncature"
        );
        assert_eq!(
            r.dr_pour_la_piste(
                None,
                None,
                Some("Prélude à l'après-midi d'un faune"),
                Some(2)
            ),
            Some(13),
            "l'apostrophe droite et la typographique se comparent égales"
        );
    }

    #[test]
    fn un_dr_d_album_seul_sans_ligne_de_piste_ne_donne_aucune_piste() {
        let r = analyser("foobar2000 / DR Meter\nOfficial DR value: DR12\n");
        assert_eq!(r.dr_album, Some(12));
        assert!(r.lignes.is_empty());
        assert_eq!(r.dr_pour_la_piste(Some(1), None, Some("x"), Some(2)), None);
    }

    #[test]
    fn un_coffret_disque_piste_ne_se_lit_ainsi_que_si_toutes_les_lignes_le_sont() {
        let coffret = "DR         Peak         RMS     Duration Track\n\
DR11      -0.20 dB   -14.53 dB      4:12 1-01 Ouverture\n\
DR12      -0.10 dB   -15.02 dB      6:00 1-02 Air\n\
DR9       -0.30 dB   -12.75 dB      5:25 2-01 Gigue\n";
        let r = analyser(coffret);
        assert_eq!(r.lignes[2].disque, Some(2));
        assert_eq!(r.lignes[2].numero, Some(1));
        assert_eq!(r.lignes[2].titre, "Gigue");
        assert_eq!(r.dr_pour_la_piste(Some(1), Some(2), None, Some(2)), Some(9));
        assert_eq!(
            r.dr_pour_la_piste(Some(1), Some(1), None, Some(2)),
            Some(11)
        );
        // Sans numéro de disque côté fichier, deux « piste 1 » : le titre
        // départage, sinon ambiguïté.
        assert_eq!(
            r.dr_pour_la_piste(Some(1), None, Some("Gigue"), Some(2)),
            Some(9)
        );
        assert_eq!(r.dr_pour_la_piste(Some(1), None, None, Some(2)), None);

        // UNE seule ligne de cette forme au milieu d'un rapport ordinaire :
        // `01-12 Bars Blues` reste la piste 1, pas la piste 12 du disque 1.
        let ordinaire = "DR         Peak         RMS     Duration Track\n\
DR11      -0.20 dB   -14.53 dB      4:12 01-12 Bars Blues\n\
DR12      -0.10 dB   -15.02 dB      6:00 02-Slow Train\n";
        let r = analyser(ordinaire);
        assert_eq!(r.lignes[0].numero, Some(1));
        assert_eq!(r.lignes[0].disque, None);
        assert_eq!(r.lignes[0].titre, "12 Bars Blues");
    }

    #[test]
    fn un_nom_de_fichier_dans_la_colonne_track_s_apparie_sans_son_extension() {
        let r = analyser(
            "DR         Peak         RMS     Duration Track\n\
DR14      -0.20 dBFS  -16.53 dBFS    4:12 03 - Silverside.flac\n",
        );
        assert_eq!(r.lignes[0].numero, Some(3));
        assert_eq!(
            r.dr_pour_la_piste(None, None, Some("Silverside"), Some(2)),
            Some(14)
        );
        assert_eq!(numero_dans_le_nom("03 - Silverside"), Some(3));
        assert_eq!(numero_dans_le_nom("2001 A Space Odyssey"), None);
        assert_eq!(numero_dans_le_nom("Silverside"), None);
    }

    #[test]
    fn le_titre_se_normalise_sans_casse_ni_ponctuation() {
        assert_eq!(
            normaliser_titre("  Prélude À l’Après-Midi.FLAC "),
            "prélude à l après midi"
        );
        assert_eq!(
            normaliser_titre("Mahler Sym No 2  1st Mov"),
            "mahler sym no 2 1st mov"
        );
        assert_eq!(normaliser_titre("(…)"), "");
    }

    #[test]
    fn un_texte_sans_mesure_rend_un_rapport_vide() {
        let r = analyser("Ceci n'est pas un rapport\nDR n'est qu'un mot ici\n");
        assert!(r.lignes.is_empty());
        assert_eq!(r.dr_album, None);
    }

    #[test]
    fn rapport_voisin_lit_le_fichier_du_dossier_et_rien_d_autre() {
        let dossier = crate::test_scratch::scratch_dir("foo-dr-4186-voisin");
        let audio = dossier.join("01-Foil.flac");
        std::fs::write(&audio, b"pas un vrai flac").unwrap();
        assert!(rapport_voisin(&audio).is_none(), "pas de rapport : None");
        std::fs::write(dossier.join(NOM_DU_RAPPORT), "rien\n").unwrap();
        assert!(
            rapport_voisin(&audio).is_none(),
            "un rapport sans mesure n'est pas une source"
        );
        std::fs::write(dossier.join(NOM_DU_RAPPORT), STEREO).unwrap();
        let r = rapport_voisin(&audio).expect("le rapport voisin se lit");
        assert_eq!(r.lignes.len(), 3);
    }
}
