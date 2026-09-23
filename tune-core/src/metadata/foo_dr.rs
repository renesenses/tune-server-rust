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
//!
//! # Les autres mesureurs (#4352) : c'est la DÉCOUVERTE qui était étroite
//!
//! L'analyseur ci-dessus n'exige aucun en-tête « foobar2000 » : il lit une
//! mise en page, celle du TT DR. Jusqu'à #4352, [`rapport_voisin`] ne
//! demandait pourtant qu'UN seul nom, `foo_dr.txt`, exact et sensible à la
//! casse. Un rapport écrit par un autre outil — la même mise en page, un
//! autre nom — n'était jamais ouvert.
//!
//! Ce que l'on sait des noms réellement écrits, et d'où on le tient :
//!
//! - **DR Meter de foobar2000** → `foo_dr.txt`. Établi par la pièce jointe du
//!   fil 1800 (#4186), lue octet par octet.
//! - **`dr14_t.meter`** (simon-r) → `dr14.txt` jusqu'au commit `30d571f4`
//!   (05/10/2020), puis `dr14-DR<n>.txt` (le DR d'album est DANS le nom) ;
//!   aussi `dr14_bbcode.txt` et `dr14_mediawiki.txt`, qui sont du BALISAGE et
//!   non cette mise en page. Établi sur la source : `dr14tmeter/dr14_utils.py`,
//!   table `tables_list` de `write_results`, écrite dans le dossier analysé.
//! - **`dr_meter` de DeaDBeeF** (`dakeryas/deadbeef-dr-meter`) → **aucun nom
//!   fixe**. `dr_plugin_gui/src/save_button.c` ouvre un `GtkFileChooser` en
//!   mode `SAVE` **sans nom par défaut** : c'est l'utilisateur qui nomme le
//!   fichier. Sa mise en page, elle, est exactement celle du TT DR :
//!   `dr_meter/src/dr_log_printer.c` écrit
//!   `DR         Peak         RMS     Duration Track` puis
//!   `Official DR value: DRn`, et les mesures au format
//!   `DR%-2.0f %10.2f dB %8.2f dB` (`DEFAULT_DR_FORMAT`,
//!   `dr_plugin/src/dr_meter_plugin.c`).
//! - **MAAT DROffline MkII** → **aucun nom fixe non plus**, et surtout **une
//!   AUTRE mise en page** : voir la section suivante. Son manuel
//!   (`DROfflineMkII_UM.pdf`, sections « Settings » et « Global Tab ») décrit
//!   une case *Create Log File*, un dossier de sortie au choix (dossier
//!   source, *Analysis Folder* ou *Alternate Folder*) et un format « plain
//!   ASCII text, where commas are used to create structure » ou TSV. **Les
//!   deux rapports réels démentent cette description** : ni virgule ni
//!   tabulation, un tableau à barres verticales.
//!
//! # DROffline MkII (#4352) : là, c'est l'ANALYSEUR qui refusait
//!
//! Pour DeaDBeeF, le blocage était la découverte, et l'élargissement des noms
//! l'a levé. Pour DROffline MkII, il est ailleurs. Mesuré le 20/09/2026 sur
//! les deux pièces jointes de Patatorz (fil 1781, réponse 6568), passées au
//! module livré en v0.9.158 : `lignes=0`, `dr_album=None`, `entete=false` —
//! **et la même chose après les avoir renommées `foo_dr.txt`**. Le nom n'y
//! était pour rien.
//!
//! Le plus court des deux, intégral :
//!
//! ```text
//! Folder Path:   /Volumes/music-1/00_music/studio_masters/GoGo Penguin/Live At Abbey Road EP
//!
//!                   File Name | Format |  SR | Word Length | Max. TPL |  LUFSi | DR (PMF) |
//!
//!  01 - Branches Break (Live) |  .flac | 48k |          24 |    -0.37 | -12.01 |        7 |
//!       02 - GBFISYSIH (Live) |  .flac | 48k |          24 |    -0.37 | -16.21 |       11 |
//!        03 - Initiate (Live) |  .flac | 48k |          24 |    -0.39 | -10.25 |        7 |
//! 04 - Ocean In A Drop (Live) |  .flac | 48k |          24 |    -0.32 | -11.49 |        7 |
//!
//! Number of EP/Album Files: 4
//! Official EP/Album DR: 8
//! ```
//!
//! Rien de ce que le lecteur TT DR cherche n'y est : la valeur DR est en
//! **dernière** colonne et en **entier nu**, pas en tête et pas préfixée
//! `DR` ; il n'y a ni `Peak` ni `RMS` mais un true peak (`Max. TPL`) et une
//! loudness intégrée (`LUFSi`) ; et le total s'écrit `Official EP/Album DR:`.
//! D'où un **second analyseur** ([`analyser_droffline`]), et non un nom de
//! plus dans la liste : [`analyser`] aiguille sur la présence de l'en-tête à
//! barres verticales, qu'un rapport TT DR ne peut pas porter.
//!
//! Le nom réel des deux fichiers est `<dernier segment du Folder Path>_log.txt`
//! (`Live At Abbey Road EP_log.txt`). Ce n'est **pas** ajouté aux noms
//! établis : deux exemplaires font une régularité, pas une règle, et la porte
//! des candidats suffit — l'en-tête DROffline signe le rapport
//! ([`RapportDr::est_signe`]).
//!
//! # Comment on élargit sans lire n'importe quoi
//!
//! Un nom de plus ne suffit pas : DeaDBeeF n'en a pas. Mais accepter tout
//! `.txt` d'un dossier d'album prendrait le livret pour un rapport. D'où
//! **deux régimes**, et un seul juge — l'analyseur :
//!
//! 1. **Nom établi** (`foo_dr.txt`, `dr14.txt`, `dr14-dr*.txt`, la casse ne
//!    comptant plus) : c'est une déclaration d'intention. Il suffit au
//!    fichier de porter une mesure, exactement comme avant #4352.
//! 2. **Tout autre `.txt`** : il doit FAIRE SES PREUVES, c'est-à-dire porter
//!    au moins une mesure **et** une marque de rapport — la ligne d'en-tête
//!    `DR … Peak … RMS`, ou la ligne `Official DR value:`
//!    ([`RapportDr::est_signe`]). Une fiche de release où quelqu'un a recopié
//!    `DR12  -0.5 dB  -12.4 dB   01-So What` a beau donner une ligne
//!    analysable, qui désigne la piste 1 sans ambiguïté : sans en-tête ni
//!    total, elle n'est pas retenue.
//!
//! Ce module ne fait que **lire**. Il n'ouvre aucun fichier audio et n'écrit
//! aucun tag : le piège de #4238 (une écriture de métadonnées qui efface les
//! champs Vorbis qu'elle ne connaît pas) ne le concerne pas.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
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
    /// Vrai quand le texte portait une ligne d'en-tête de colonnes — celle du
    /// TT DR (`DR … Peak … RMS`) ou celle de DROffline MkII
    /// (`File Name | … | DR (PMF) |`). C'est, avec `dr_album`, ce qui
    /// distingue un RAPPORT d'un texte où des lignes ressemblent à des
    /// mesures — voir [`RapportDr::est_signe`].
    pub entete: bool,
    /// Le dossier que le mesureur dit avoir analysé, quand il l'écrit —
    /// `Folder Path:` chez DROffline MkII (#4352). Le TT DR ne l'écrit pas,
    /// et le rapport est alors `None`.
    ///
    /// Lu et conservé mais PAS encore exploité : [`rapport_voisin`] ne
    /// cherche que dans le dossier de la piste, donc le chemin y est toujours
    /// le bon. Il devient la clé d'appariement le jour où l'on indexera les
    /// rapports d'un *Analysis Folder* commun à plusieurs albums — ce que le
    /// manuel MAAT autorise et que Tune ne sait pas faire. Le jeter
    /// maintenant obligerait à réécrire l'analyseur à ce moment-là.
    pub dossier_source: Option<String>,
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
///
/// Deux mises en page, deux analyseurs (#4352) : celle du TT DR — colonnes à
/// largeur fixe, sans délimiteur, DR en TÊTE de ligne — et celle de MAAT
/// DROffline MkII — colonnes séparées par des barres verticales, DR en
/// DERNIÈRE position et en entier nu. Le choix se fait sur la présence d'un
/// en-tête DROffline, qui ne peut pas apparaître dans un rapport TT DR (le
/// second ne contient aucune barre verticale).
pub fn analyser(texte: &str) -> RapportDr {
    match colonnes_droffline(texte) {
        Some(colonnes) => analyser_droffline(texte, &colonnes),
        None => analyser_tt_dr(texte),
    }
}

/// La mise en page du TT DR Offline Meter, celle que reprennent le DR Meter
/// de foobar2000, `dr14_t.meter` et le `dr_meter` de DeaDBeeF.
fn analyser_tt_dr(texte: &str) -> RapportDr {
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
            // La marque d'un rapport : c'est cette ligne, ou `Official DR
            // value:`, qui autorise à ouvrir un fichier au nom inconnu.
            rapport.entete = true;
            continue;
        }
        if let Some(l) = analyser_ligne(ligne, entete_multicanal) {
            brutes.push(l);
        }
    }
    rapport.lignes = assembler(brutes);
    rapport
}

/// Découpe la colonne « piste » de chaque ligne brute en numéro de disque,
/// numéro de piste et titre. Commun aux deux analyseurs : ce découpage dépend
/// de l'ENSEMBLE des lignes (coffrets), il ne peut pas se faire ligne à ligne.
fn assembler(brutes: Vec<LigneBrute>) -> Vec<LigneDr> {
    // Coffrets : `1-01 Titre`. La forme disque-piste n'est retenue que si
    // TOUTES les lignes la portent — sur une seule, `01-12 Bars Blues` serait
    // lu « disque 1, piste 12 » et apparié à la mauvaise piste.
    let en_disque_piste =
        !brutes.is_empty() && brutes.iter().all(|l| DISQUE_ET_NUMERO.is_match(&l.piste));
    brutes
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
        .collect()
}

/// Le nom des colonnes de l'en-tête DROffline MkII, s'il y en a un dans le
/// texte. C'est le seul aiguillage entre les deux analyseurs.
///
/// Mesuré sur les deux pièces jointes de Patatorz (fil 1781, réponse 6568) :
///
/// ```text
///                   File Name | Format |  SR | Word Length | Max. TPL |  LUFSi | DR (PMF) |
/// ```
///
/// Reconnu par sa STRUCTURE, pas par la liste exacte des colonnes : une
/// colonne `File Name` et une colonne dont le nom commence par `DR`. Le
/// manuel MAAT annonce trois formats d'export (virgules, TSV, et celui-ci) et
/// ne documente aucune de ces colonnes ; on ne fige donc que ce qui a été
/// mesuré, et un réglage qui ajoute ou retire une colonne du milieu ne casse
/// rien — c'est l'en-tête qui donne les positions.
fn colonnes_droffline(texte: &str) -> Option<Vec<String>> {
    texte.lines().find_map(|ligne| {
        if !ligne.contains('|') {
            return None;
        }
        let colonnes = decouper_aux_barres(ligne);
        let a_le_nom = colonnes.iter().any(|c| c.eq_ignore_ascii_case("File Name"));
        let a_le_dr = colonnes.iter().any(|c| est_colonne_dr(c));
        (a_le_nom && a_le_dr).then_some(colonnes)
    })
}

/// Découpe une ligne aux barres verticales. La ligne finit par `| ` : le
/// champ vide de queue n'est pas une colonne.
fn decouper_aux_barres(ligne: &str) -> Vec<String> {
    let mut champs: Vec<String> = ligne
        .trim_end_matches('\r')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect();
    while champs.last().is_some_and(String::is_empty) {
        champs.pop();
    }
    champs
}

/// `DR (PMF)`, `DR`, `DR(PMF)` — la colonne qui porte la plage dynamique.
/// `Word Length` ou `LUFSi` ne commencent pas par `DR`, et la colonne
/// `Duration` d'un éventuel autre réglage non plus.
fn est_colonne_dr(nom: &str) -> bool {
    let n = nom.trim();
    if !n.get(..2).is_some_and(|d| d.eq_ignore_ascii_case("dr")) {
        return false;
    }
    matches!(n.as_bytes().get(2).copied(), None | Some(b' ') | Some(b'('))
}

/// La mise en page de MAAT DROffline MkII (#4352), telle que les deux
/// rapports réels de Patatorz l'écrivent.
///
/// Quatre différences avec le TT DR, et chacune est une raison pour laquelle
/// l'analyseur ci-dessus rendait ZÉRO ligne sur ces fichiers, même renommés
/// `foo_dr.txt` (mesuré le 20/09/2026) :
///
/// 1. les colonnes sont séparées par des **barres verticales**, pas par des
///    largeurs fixes ;
/// 2. la valeur DR est en **dernière** position et en **entier nu** (`7`),
///    quand `analyser_ligne` exige un premier jeton `DR<n>` ;
/// 3. il n'y a ni `Peak` ni `RMS` : DROffline donne un true peak
///    (`Max. TPL`) et une loudness intégrée (`LUFSi`) ;
/// 4. le total s'écrit `Official EP/Album DR:` et non `Official DR value:`.
///
/// ⚠️ **L'échelle de `DR (PMF)` n'est pas établie** comme identique à celle du
/// TT DR : `PMF` n'est explicité nulle part dans le fichier, et aucune mesure
/// croisée n'existe (JeromeQ en a une entre foobar2000 et DeaDBeeF, « ±1 dB »,
/// pas pour DROffline). Le seul recoupement possible ici est interne : sur les
/// deux rapports, `Official EP/Album DR` vaut la **moyenne tronquée** des DR
/// de piste — 8 pour `(7+11+7+7)/4 = 8,0` et 8 pour `144/17 = 8,47` — soit la
/// même convention d'agrégation que le TT DR. C'est une cohérence interne, pas
/// une équivalence d'échelle.
fn analyser_droffline(texte: &str, colonnes: &[String]) -> RapportDr {
    let mut rapport = RapportDr {
        entete: true,
        ..RapportDr::default()
    };
    let (Some(i_nom), Some(i_dr)) = (
        colonnes
            .iter()
            .position(|c| c.eq_ignore_ascii_case("File Name")),
        colonnes.iter().position(|c| est_colonne_dr(c)),
    ) else {
        return rapport;
    };
    let mut brutes: Vec<LigneBrute> = Vec::new();
    for ligne in texte.lines() {
        let propre = ligne.trim_end_matches('\r').trim();
        if propre.is_empty() {
            continue;
        }
        if let Some(reste) = propre.strip_prefix("Folder Path:") {
            let chemin = reste.trim();
            if !chemin.is_empty() {
                rapport.dossier_source = Some(chemin.to_string());
            }
            continue;
        }
        // `Official EP/Album DR: 8` chez DROffline, `Official DR value: DR9`
        // chez le TT DR — les deux sont acceptés, le second ne coûte rien.
        if let Some(reste) = propre
            .strip_prefix("Official EP/Album DR:")
            .or_else(|| propre.strip_prefix("Official DR value:"))
        {
            rapport.dr_album = valeur_dr(reste.trim());
            continue;
        }
        if !propre.contains('|') {
            continue;
        }
        let champs = decouper_aux_barres(ligne);
        // Le nombre de colonnes est celui de l'en-tête : une ligne plus
        // courte ou plus longue n'est pas une mesure de ce tableau.
        if champs.len() != colonnes.len() {
            continue;
        }
        // L'en-tête lui-même repasse ici : sa colonne DR vaut `DR (PMF)`,
        // que `valeur_dr` refuse. Rien à filtrer de plus.
        let Some(dr) = valeur_dr(&champs[i_dr]) else {
            continue;
        };
        let piste = champs[i_nom].trim().to_string();
        if piste.is_empty() {
            continue;
        }
        brutes.push(LigneBrute {
            dr,
            piste,
            // DROffline écrit le titre en entier et n'a pas de colonnes par
            // canal : ni troncature à reconnaître, ni couche à départager.
            tronque: false,
            multicanal: false,
        });
    }
    rapport.lignes = assembler(brutes);
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
    /// Vrai quand le texte porte les marques d'un **rapport** DR, et pas
    /// seulement des lignes qui ressemblent à des mesures.
    ///
    /// C'est le juge de la découverte élargie (#4352) : un fichier au nom
    /// inconnu n'est retenu que s'il a au moins une mesure **et** une
    /// signature — la ligne d'en-tête `DR … Peak … RMS`, ou le total
    /// `Official DR value:`. Les deux mesureurs dont la sortie est établie
    /// (`dr14_t.meter`, `dr_meter` de DeaDBeeF) écrivent les DEUX ; un texte
    /// de notes qui aligne `DR12  -0.5 dB  -12.4 dB` n'en écrit aucune.
    pub fn est_signe(&self) -> bool {
        !self.lignes.is_empty() && (self.entete || self.dr_album.is_some())
    }

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

/// Au-delà, ce n'est plus un rapport d'album : celui du fil 1800, 22 pistes
/// et deux couches, pèse 6 232 octets. La borne écarte le livret, les paroles
/// ou le texte d'un coffret sans avoir à les lire.
const TAILLE_MAX: u64 = 1024 * 1024;

/// Combien de `.txt` au nom NON établi on accepte d'ouvrir dans un dossier.
/// Un dossier d'album en porte un ou deux ; au-delà c'est un dossier de
/// documents, et on ne va pas tout relire à chaque piste.
const CANDIDATS_MAX: usize = 8;

/// Le rang d'un nom de rapport **établi**, ou `None` si le nom ne dit rien.
///
/// `nom_minuscule` est le nom de fichier en minuscules, extension comprise ;
/// c'est ce qui rend la recherche insensible à la casse — un `FOO_DR.TXT`
/// recopié depuis un partage Windows était invisible avant #4352.
///
/// Le rang ordonne : `foo_dr.txt` d'abord, c'est le nom du fil 1800 et celui
/// que Tune documente. Les sources de ces noms sont en tête de module.
fn rang_du_nom_etabli(nom_minuscule: &str) -> Option<u8> {
    if nom_minuscule == NOM_DU_RAPPORT {
        return Some(0);
    }
    // `dr14_t.meter` : `dr14.txt` avant le 05/10/2020, `dr14-DR<n>.txt`
    // depuis. `dr14_bbcode.txt` et `dr14_mediawiki.txt` sont du BALISAGE et
    // ne sont pas des noms établis pour CETTE mise en page : s'ils sont là,
    // ils passeront par la porte des candidats et seront refusés.
    if nom_minuscule == "dr14.txt" || nom_minuscule.starts_with("dr14-dr") {
        return Some(1);
    }
    None
}

/// Le rapport posé à côté d'un fichier audio, s'il y en a un.
///
/// Cherche dans le dossier du fichier, parmi les seuls `.txt` (la casse ne
/// compte pas) d'au plus [`TAILLE_MAX`] :
///
/// 1. les **noms établis** d'abord, dans l'ordre de [`rang_du_nom_etabli`] —
///    il leur suffit de porter une mesure ;
/// 2. puis, au plus [`CANDIDATS_MAX`] autres `.txt` par ordre alphabétique,
///    retenus seulement s'ils sont [`RapportDr::est_signe`] — c'est le cas de
///    DeaDBeeF, dont le journal n'a pas de nom fixe (voir le module).
///
/// `None` quand rien ne correspond : le cas de l'immense majorité des
/// bibliothèques. Il en coûte alors un `read_dir` par piste — le scan vient
/// d'ouvrir et de décoder les tags du fichier audio, c'est sans commune
/// mesure — et aucune lecture de contenu s'il n'y a pas de `.txt`.
pub fn rapport_voisin(fichier_audio: &Path) -> Option<RapportDr> {
    let dossier = fichier_audio.parent()?;
    let mut etablis: Vec<(u8, PathBuf)> = Vec::new();
    let mut candidats: Vec<PathBuf> = Vec::new();
    for entree in std::fs::read_dir(dossier).ok()?.flatten() {
        let brut = entree.file_name();
        let Some(nom) = brut.to_str() else { continue };
        let minuscule = nom.to_ascii_lowercase();
        if !minuscule.ends_with(".txt") {
            continue;
        }
        match entree.metadata() {
            Ok(m) if m.is_file() && m.len() <= TAILLE_MAX => {}
            _ => continue,
        }
        match rang_du_nom_etabli(&minuscule) {
            Some(rang) => etablis.push((rang, entree.path())),
            None => candidats.push(entree.path()),
        }
    }
    // `read_dir` ne promet aucun ordre : on le fixe, sinon deux scans du même
    // dossier pourraient retenir deux fichiers différents.
    etablis.sort();
    candidats.sort();

    for (_, chemin) in &etablis {
        if let Some(rapport) = lire_le_rapport(chemin)
            && !rapport.lignes.is_empty()
        {
            return Some(rapport);
        }
    }
    for chemin in candidats.iter().take(CANDIDATS_MAX) {
        if let Some(rapport) = lire_le_rapport(chemin)
            && rapport.est_signe()
        {
            return Some(rapport);
        }
    }
    None
}

/// Lit et analyse un fichier. `None` si la lecture échoue — un fichier
/// illisible n'est pas une erreur du scan, c'est une absence de rapport.
fn lire_le_rapport(chemin: &Path) -> Option<RapportDr> {
    Some(analyser_octets(&std::fs::read(chemin).ok()?))
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

    // ------------------------------------------------------------------
    // #4352 — les autres mesureurs. Sources des mises en page : en tête de
    // module, avec le fichier et la ligne de chaque outil.
    // ------------------------------------------------------------------

    /// Le journal du greffon `dr_meter` de DeaDBeeF, mis en page par
    /// `dr_log_printer.c`.
    const DEADBEEF: &[u8] = include_bytes!("../../tests/fixtures/dr_deadbeef_4352.txt");
    /// La table texte de `dr14_t.meter` (cellules séparées par des
    /// tabulations, `TextTable` de `dr14tmeter/table.py`).
    const DR14: &[u8] = include_bytes!("../../tests/fixtures/dr14_tmeter_4352.txt");
    /// Une fiche de release où quelqu'un a RECOPIÉ une ligne de rapport,
    /// sans en-tête de colonnes ni total : le faux positif réaliste.
    const NOTES: &[u8] = include_bytes!("../../tests/fixtures/pas_un_rapport_dr_4352.txt");

    /// Un dossier d'album jetable, avec un faux fichier audio.
    fn dossier_avec_audio(etiquette: &str) -> (crate::test_scratch::ScratchDir, PathBuf) {
        let dossier = crate::test_scratch::scratch_dir(etiquette);
        let audio = dossier.join("01 - So What.flac");
        std::fs::write(&audio, b"pas un vrai flac").unwrap();
        (dossier, audio)
    }

    /// L'analyseur lisait DÉJÀ ces deux mises en page — c'est la découverte
    /// qui ne les trouvait pas. On le montre avant tout le reste.
    #[test]
    fn l_analyseur_lisait_deja_les_deux_autres_mesureurs_4352() {
        let d = analyser_octets(DEADBEEF);
        assert_eq!(d.lignes.len(), 3, "DeaDBeeF : trois pistes. Relevé : {d:?}");
        assert_eq!(d.dr_album, Some(13));
        assert_eq!(d.lignes[0].dr, 13);
        assert_eq!(d.lignes[0].numero, Some(1));
        assert_eq!(d.lignes[0].titre, "So What");
        assert!(d.entete, "l'en-tête de colonnes de DeaDBeeF est reconnu");

        let t = analyser_octets(DR14);
        assert_eq!(
            t.lignes.len(),
            2,
            "dr14_t.meter : deux pistes. Relevé : {t:?}"
        );
        assert_eq!(t.dr_album, Some(11));
        assert_eq!(t.lignes[1].dr, 11);
        assert_eq!(t.lignes[1].numero, Some(2));
        assert_eq!(t.lignes[1].titre, "No Reply At All.flac");
    }

    /// LE CAS DE JeromeQ — le greffon de DeaDBeeF n'a AUCUN nom par défaut
    /// (`save_button.c` ouvre un sélecteur de fichier vide) : c'est
    /// l'utilisateur qui nomme. Seule la découverte par CONTENU le trouve.
    #[test]
    fn un_journal_deadbeef_sans_nom_fixe_est_trouve_et_lu_4352() {
        let (dossier, audio) = dossier_avec_audio("foo-dr-4352-deadbeef");
        std::fs::write(dossier.join("mes mesures DR.txt"), DEADBEEF).unwrap();
        let r = rapport_voisin(&audio).expect("#4352 — un nom libre reste un rapport");
        assert_eq!(r.dr_album, Some(13));
        assert_eq!(r.lignes.len(), 3);
    }

    /// LE FAUX POSITIF — le vrai risque. Des lignes analysables, mais ni
    /// en-tête de colonnes ni `Official DR value:` : ce n'est pas un rapport,
    /// et on ne colle pas DR12 sur la piste 1 de l'album.
    #[test]
    fn un_texte_de_notes_n_est_pas_pris_pour_un_rapport_4352() {
        // L'ANALYSEUR, lui, en tire bien trois lignes : sans la signature,
        // élargir la découverte à tout `.txt` aurait lu ce fichier.
        let brut = analyser_octets(NOTES);
        assert_eq!(
            brut.lignes.len(),
            1,
            "le piège est réel : cette ligne S'ANALYSE, et elle désigne la \
             piste 1 sans ambiguïté. Relevé : {brut:?}"
        );
        assert_eq!(brut.lignes[0].dr, 12);
        assert_eq!(brut.lignes[0].numero, Some(1));
        assert!(
            !brut.est_signe(),
            "ni en-tête `DR … Peak … RMS` ni `Official DR value:` : pas un rapport"
        );

        let (dossier, audio) = dossier_avec_audio("foo-dr-4352-notes");
        std::fs::write(dossier.join("notes.txt"), NOTES).unwrap();
        assert!(
            rapport_voisin(&audio).is_none(),
            "#4352 — un texte posé dans le dossier n'est pas une source de DR"
        );
    }

    /// La CASSE ne cache plus rien : `join("foo_dr.txt")` était une
    /// correspondance exacte, et un `FOO_DR.TXT` venu d'un partage Windows
    /// restait invisible. Ce cas ne rougit que sur un système de fichiers
    /// SENSIBLE à la casse (Linux, la CI) : sous macOS, APFS répond déjà à
    /// `foo_dr.txt` pour un fichier nommé `FOO_DR.TXT`.
    #[test]
    fn la_casse_du_nom_ne_cache_plus_le_rapport_4352() {
        let (dossier, audio) = dossier_avec_audio("foo-dr-4352-casse");
        std::fs::write(dossier.join("FOO_DR.TXT"), STEREO).unwrap();
        let r = rapport_voisin(&audio).expect("#4352 — la casse du nom ne compte plus");
        assert_eq!(r.lignes.len(), 3);
    }

    /// Le nom de `dr14_t.meter` depuis 2020 porte le DR dans le nom :
    /// `dr14-DR11.txt`. Un nom établi n'a pas à porter de signature.
    #[test]
    fn le_nom_de_dr14_tmeter_est_un_nom_etabli_4352() {
        let (dossier, audio) = dossier_avec_audio("foo-dr-4352-dr14");
        std::fs::write(dossier.join("dr14-DR11.txt"), DR14).unwrap();
        let r = rapport_voisin(&audio).expect("#4352 — `dr14-DR<n>.txt` est un nom établi");
        assert_eq!(r.dr_album, Some(11));
        assert_eq!(r.lignes.len(), 2);
    }

    /// Deux rapports dans le même dossier : le nom ÉTABLI passe devant, quel
    /// que soit l'ordre que rend `read_dir`.
    #[test]
    fn le_nom_etabli_prime_sur_un_candidat_4352() {
        let (dossier, audio) = dossier_avec_audio("foo-dr-4352-priorite");
        std::fs::write(dossier.join("aaa mesures.txt"), DEADBEEF).unwrap();
        std::fs::write(dossier.join(NOM_DU_RAPPORT), STEREO).unwrap();
        let r = rapport_voisin(&audio).expect("le rapport se lit");
        assert_eq!(
            r.dr_album,
            Some(11),
            "`foo_dr.txt` (Autechre, DR11) a été retenu, pas le journal \
             DeaDBeeF (Miles Davis, DR13). Relevé : {r:?}"
        );
        assert_eq!(r.lignes[0].titre, "Foil");
    }

    const DROFFLINE_ABBEY: &str = r#"
Folder Path:   /Volumes/music-1/00_music/studio_masters/GoGo Penguin/Live At Abbey Road EP

                  File Name | Format |  SR | Word Length | Max. TPL |  LUFSi | DR (PMF) | 

 01 - Branches Break (Live) |  .flac | 48k |          24 |    -0.37 | -12.01 |        7 | 
      02 - GBFISYSIH (Live) |  .flac | 48k |          24 |    -0.37 | -16.21 |       11 | 
       03 - Initiate (Live) |  .flac | 48k |          24 |    -0.39 | -10.25 |        7 | 
04 - Ocean In A Drop (Live) |  .flac | 48k |          24 |    -0.32 | -11.49 |        7 | 

Number of EP/Album Files: 4
Official EP/Album DR: 8"#;

    const DROFFLINE_HOPE: &str = r#"
Folder Path:   /Users/ludovicaudoin/Downloads/Qobuz Download/Ezra Collective/Here Because of Hope

             File Name | Format |  SR | Word Length | Max. TPL |  LUFSi | DR (PMF) | 

           01 - Part 1 |   .aif | 48k |          24 |    -4.01 | -18.66 |       15 | 
02 - Blow Your Trumpet |   .aif | 48k |          24 |    -0.11 | -10.68 |        6 | 
       03 - Sweet Echo |   .aif | 48k |          24 |    -0.11 |  -9.68 |        7 | 
      04 - Don't Worry |   .aif | 48k |          24 |    -0.11 |  -9.44 |        7 | 
        05 - Only Love |   .aif | 48k |          24 |    -0.11 |  -9.87 |        6 | 
          06 - Someday |   .aif | 48k |          24 |    -0.11 |  -9.68 |        7 | 
           07 - Part 2 |   .aif | 48k |          24 |    -4.02 | -18.29 |       13 | 
     08 - Birdie Sings |   .aif | 48k |          24 |    -0.11 |  -9.76 |        8 | 
   09 - The Last Stand |   .aif | 48k |          24 |    -0.11 |  -8.89 |        7 | 
   10 - Well Organised |   .aif | 48k |          24 |    -0.11 | -10.32 |        7 | 
       11 - El Corazón |   .aif | 48k |          24 |    -0.11 |  -9.63 |        8 | 
12 - Bunny on the Rise |   .aif | 48k |          24 |    -0.11 |  -8.95 |        7 | 
           13 - Part 3 |   .aif | 48k |          24 |    -4.01 | -18.55 |       15 | 
       14 - All I Need |   .aif | 48k |          24 |    -0.11 | -11.94 |        9 | 
  15 - Jubilee Feeling |   .aif | 48k |          24 |    -0.11 | -10.41 |        7 | 
       16 - Black Flag |   .aif | 48k |          24 |    -0.11 | -10.00 |        7 | 
        17 - Most High |   .aif | 48k |          24 |    -0.11 | -11.15 |        8 | 

Number of EP/Album Files: 17
Official EP/Album DR: 8"#;

    /// Un tableau à barres verticales qui n'est PAS un rapport DR : même
    /// forme, aucune colonne DR. Il ne doit devenir ni un rapport, ni un
    /// candidat retenu par la découverte.
    const TABLEAU_SANS_DR: &str = "\
                  File Name | Format |  SR | Word Length |\n\
 01 - Branches Break (Live) |  .flac | 48k |          24 |\n\
      02 - GBFISYSIH (Live) |  .flac | 48k |          24 |\n";

    /// Le rapport DROffline MkII de Patatorz, **mot pour mot** (réponse 6568
    /// du fil 1781, `Live At Abbey Road EP_log.txt`, 601 octets, LF, aucune
    /// tabulation et aucune virgule). Mesuré le 20/09 sur le module livré :
    /// `lignes=0 dr_album=None entete=false` — même renommé `foo_dr.txt`.
    #[test]
    fn lit_le_rapport_droffline_mkii_de_patatorz_4352() {
        let r = analyser(DROFFLINE_ABBEY);
        assert_eq!(
            r.lignes.len(),
            4,
            "les quatre pistes de l'EP. Relevé : {r:#?}"
        );
        assert_eq!(r.dr_album, Some(8), "`Official EP/Album DR: 8`");
        assert!(
            r.entete,
            "l'en-tête `File Name | … | DR (PMF) |` signe le rapport"
        );
        assert!(r.est_signe(), "sans signature, un nom inconnu reste fermé");
        assert_eq!(
            r.dossier_source.as_deref(),
            Some("/Volumes/music-1/00_music/studio_masters/GoGo Penguin/Live At Abbey Road EP"),
            "`Folder Path:` est la clé d'appariement d'un rapport posé ailleurs"
        );
        let numeros: Vec<Option<u32>> = r.lignes.iter().map(|l| l.numero).collect();
        assert_eq!(
            numeros,
            vec![Some(1), Some(2), Some(3), Some(4)],
            "la colonne `File Name` porte `NN - Titre`"
        );
        let drs: Vec<u8> = r.lignes.iter().map(|l| l.dr).collect();
        assert_eq!(
            drs,
            vec![7, 11, 7, 7],
            "la valeur DR est en DERNIÈRE colonne, en entier nu, sans préfixe `DR`"
        );
        assert_eq!(r.lignes[0].titre, "Branches Break (Live)");
        assert_eq!(r.lignes[3].titre, "Ocean In A Drop (Live)");
        assert!(
            r.lignes.iter().all(|l| !l.tronque && !l.multicanal),
            "DROffline ne tronque pas les titres et n'écrit pas de colonnes par canal"
        );
    }

    /// Le second rapport de la même réponse : 17 pistes `.aif`, un titre
    /// accentué (`El Corazón`) qui prouve le décodage UTF-8, et une colonne
    /// `File Name` dont le rembourrage est DIFFÉRENT du premier fichier
    /// (13 espaces contre 18) — la largeur dépend du plus long titre et ne
    /// peut donc pas être codée en dur.
    #[test]
    fn lit_le_second_rapport_droffline_et_ses_dix_sept_pistes_4352() {
        let r = analyser(DROFFLINE_HOPE);
        assert_eq!(r.lignes.len(), 17, "Relevé : {r:#?}");
        assert_eq!(r.dr_album, Some(8));
        assert_eq!(r.lignes[0].titre, "Part 1");
        assert_eq!(r.lignes[0].dr, 15);
        assert_eq!(r.lignes[10].numero, Some(11));
        assert_eq!(r.lignes[10].titre, "El Corazón");
        assert_eq!(r.lignes[10].dr, 8);
        assert_eq!(r.lignes[16].numero, Some(17));
        assert_eq!(r.lignes[16].dr, 8);
        assert_eq!(
            r.dossier_source.as_deref(),
            Some(
                "/Users/ludovicaudoin/Downloads/Qobuz Download/Ezra Collective/Here Because of Hope"
            )
        );
    }

    /// L'appariement d'une piste, une fois le rapport lu : par numéro, et par
    /// titre quand le fichier n'a pas de numéro de piste.
    #[test]
    fn apparie_une_piste_du_rapport_droffline_4352() {
        let r = analyser(DROFFLINE_ABBEY);
        assert_eq!(r.dr_pour_la_piste(Some(2), None, None, Some(2)), Some(11));
        assert_eq!(
            r.dr_pour_la_piste(None, None, Some("Ocean In A Drop (Live)"), Some(2)),
            Some(7)
        );
        assert_eq!(
            r.dr_pour_la_piste(Some(9), None, Some("Pas dans le rapport"), Some(2)),
            None,
            "une piste absente du rapport ne prend pas le DR d'une autre"
        );
    }

    /// La découverte : le fichier porte le nom que DROffline lui a donné —
    /// `<nom du dossier d'album>_log.txt` — qui n'est PAS un nom établi. Il
    /// passe par la porte des candidats, et c'est sa SIGNATURE qui l'ouvre.
    #[test]
    fn trouve_le_rapport_droffline_sous_son_vrai_nom_4352() {
        let (dossier, audio) = dossier_avec_audio("foo-dr-4352-droffline");
        std::fs::write(
            dossier.join("Live At Abbey Road EP_log.txt"),
            DROFFLINE_ABBEY,
        )
        .unwrap();
        let r = rapport_voisin(&audio)
            .expect("#4352 — un rapport DROffline MkII signé est retenu sous n'importe quel nom");
        assert_eq!(r.lignes.len(), 4);
        assert_eq!(r.dr_album, Some(8));
    }

    /// Le garde-fou : un tableau à barres verticales SANS colonne DR n'est ni
    /// lu ni retenu. Sans lui, n'importe quel tableau texte d'un dossier
    /// d'album deviendrait une source de plage dynamique.
    #[test]
    fn un_tableau_a_barres_sans_colonne_dr_n_est_pas_un_rapport_4352() {
        let r = analyser(TABLEAU_SANS_DR);
        assert!(r.lignes.is_empty(), "Relevé : {r:#?}");
        assert!(!r.est_signe());

        let (dossier, audio) = dossier_avec_audio("foo-dr-4352-tableau");
        std::fs::write(dossier.join("liste des pistes.txt"), TABLEAU_SANS_DR).unwrap();
        assert!(rapport_voisin(&audio).is_none());
    }
}
