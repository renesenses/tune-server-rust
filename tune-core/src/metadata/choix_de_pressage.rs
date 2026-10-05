//! Choisir le pressage MusicBrainz d'un album **sans se tromper**, ou ne pas
//! choisir (#4805, étape D).
//!
//! D'après MetaRust, de Xavier Joly (code offert à Tune le 05/10/2026) :
//! la règle de confiance (`pick_confident_search_hit`, `choose_release`), le
//! départage par titre compact puis par statut `Official`, la lecture des
//! identifiants collés (`parse_release_mbid`, `is_mbid`) et le rapport de
//! complétude (`completeness_against_release`). L'ordre de la cascade
//! (`resolve_release`) est une idée de MetaRust, réécrite pour la base de Tune.
//!
//! # Le défaut
//!
//! Le pilote `identify-all` prenait le **premier** candidat de la recherche,
//! sans exiger qu'il soit devant les autres. MusicBrainz note pourtant 100 des
//! albums différents : `Buddha-Bar` rend `Buddha‐Bar XXIV`, `Buddha-Bar:
//! Ocean` et `Buddha-Bar: Perception`, tous à 100 ; `Symphony No. 9` /
//! Karajan rend Beethoven et Dvořák. Le premier venu était écrit, et
//! l'écriture REMPLACE les clés d'identification.
//!
//! # La règle
//!
//! Un album n'est retenu que s'il est seul, ou nettement devant
//! ([`score_nettement_devant`] : ≥ 90 avec 10 points d'avance, ou ≥ 95 face à
//! un second sous 90), ou s'il est seul à passer un départage : il doit porter
//! le titre (comparé en compact), puis suffixe d'édition, compositeur, nombre
//! de pistes, statut `Official`. Sinon
//! il est **ambigu** : rien n'est écrit, et le pilote le compte.
//!
//! 🔴 La règle juge des **albums**, pas des pressages. MusicBrainz note 100
//! les sept pressages de `Kind of Blue` : appliquée pressage par pressage,
//! elle déclarerait ambigu presque tout album connu (le banc le mesure, colonne
//! « MetaRust strict »). Les pressages sont donc regroupés par groupe de
//! sortie ; la règle choisit le groupe, puis [`choisir_dans_le_groupe`] le
//! pressage (nombre de pistes, édition, `Official`). La garde de complétude
//! ([`Completude`]) vérifie ensuite, sur la liste de pistes, que ce pressage
//! colle aux fichiers : pas de piste en double, pas de piste en trop, au moins
//! la moitié présente.
//!
//! # La cascade ([`identifier_le_pressage`])
//!
//! Les identifiants que les balises portent déjà passent avant la recherche
//! texte : MBID de release majoritaire, puis MBID d'enregistrement vers ses
//! releases, puis code-barres. Le scan les a rangés en base
//! (`track_metadata` : `mb_release_id`, `mb_track_id`, `barcode`). Une étape
//! ne coûte une requête que si la balise existe.

use serde_json::Value;
use tracing::debug;

use super::musicbrainz_release::{
    self as mb, MBReleaseDetail, MBReleaseMatch, MBTrack, RefusMusicBrainz, artist_credit,
    normalize, normalize_compact, normalize_sans_diacritiques, parse_release_detail,
    parse_search_results, plausible, str_field,
};
use super::reidentify::LocalTrack;

/// Score à partir duquel un album peut être retenu sur son seul score.
const SCORE_SUR: i32 = 90;
/// Avance minimale sur le second, pour un album retenu sur son score.
const AVANCE_MIN: i32 = 10;
/// Score qui suffit face à un second sous [`SCORE_SUR`].
const SCORE_TRES_SUR: i32 = 95;

/// Ce que la recherche demande à MusicBrainz : 15, comme le bouton
/// « Ré-identifier » (5 candidats × 3). Même requête, même coût.
const FETCH_RECHERCHE: usize = 15;

/// La lecture d'une release pour l'identifier : pistes, crédits d'artiste,
/// labels — ce que lit `lookup_release_detail` — et le groupe de sortie, que
/// la pose écrit aussi.
pub const INC_DETAIL: &str = "recordings+artist-credits+labels+release-groups";

/// La lecture d'un enregistrement vers ses releases (MetaRust :
/// `get_recording_releases`).
pub const INC_ENREGISTREMENT: &str = "releases";

/// La règle de confiance de MetaRust, sur deux scores : le premier est-il
/// **nettement devant** le second ?
pub fn score_nettement_devant(premier: i32, second: i32) -> bool {
    (premier >= SCORE_SUR && premier - second >= AVANCE_MIN)
        || (premier >= SCORE_TRES_SUR && second < SCORE_SUR)
}

/// Le verdict de [`choisir_le_pressage`] sur une liste de candidats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choix {
    /// L'indice du pressage retenu dans la liste.
    Retenu(usize),
    /// Plusieurs albums se valent et rien ne les départage : ne rien écrire.
    Ambigu,
    /// Aucun candidat.
    Aucun,
}

/// Ce que le titre local dit de l'album, pour départager.
struct Indices {
    /// Formes compactes acceptables du titre : tel quel, sans ses suffixes
    /// d'édition, tel qu'il a été interrogé.
    titres: Vec<String>,
    /// Les suffixes d'édition, compacts (`30thanniversary`).
    editions: Vec<String>,
    /// Le compositeur en tête du titre, sans diacritiques.
    compositeur: Option<String>,
    pistes: Option<u32>,
}

impl Indices {
    fn depuis(titre: &str, artiste: &str, pistes: Option<u32>) -> Self {
        let mut titres = vec![normalize_compact(titre)];
        let mut base = titre.trim().to_string();
        let editions: Vec<String> = mb::editions_du_titre(titre)
            .iter()
            .map(|e| {
                // `editions_du_titre` rend les suffixes du dernier au premier :
                // retirer chacun donne la base.
                if let Some(pos) = base.rfind(e.as_str()) {
                    base = base[..pos]
                        .trim_end()
                        .trim_end_matches(['(', '['])
                        .trim_end()
                        .to_string();
                }
                normalize_compact(e)
            })
            .filter(|e| !e.is_empty())
            .collect();
        titres.push(normalize_compact(&base));
        if let Some(interroge) = mb::titre_de_requete_pour(titre, artiste) {
            titres.push(normalize_compact(&interroge));
        }
        titres.retain(|t| !t.is_empty());
        titres.dedup();
        Indices {
            titres,
            editions,
            compositeur: mb::compositeur_en_prefixe(titre),
            pistes,
        }
    }
}

type Filtre = fn(&MBReleaseMatch, &Indices) -> bool;

fn titre_ok(c: &MBReleaseMatch, i: &Indices) -> bool {
    let t = normalize_compact(&c.title);
    i.titres.contains(&t)
}

fn edition_ok(c: &MBReleaseMatch, i: &Indices) -> bool {
    let desamb = normalize_compact(c.disambiguation.as_deref().unwrap_or(""));
    let titre = normalize_compact(&c.title);
    i.editions
        .iter()
        .any(|e| desamb.contains(e.as_str()) || titre.contains(e.as_str()))
}

fn compositeur_ok(c: &MBReleaseMatch, i: &Indices) -> bool {
    let Some(k) = i.compositeur.as_deref() else {
        return false;
    };
    normalize_sans_diacritiques(&c.artist)
        .split_whitespace()
        .any(|mot| mot == k)
}

fn pistes_ok(c: &MBReleaseMatch, i: &Indices) -> bool {
    i.pistes.is_some() && c.track_count == i.pistes
}

fn officiel(c: &MBReleaseMatch, _: &Indices) -> bool {
    c.status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("official"))
}

/// Le départage des ALBUMS, dans l'ordre, parmi ceux qui portent le titre
/// ([`titre_ok`], obligatoire). Chaque filtre ne s'applique que s'il garde au
/// moins un album ; un album seul à le passer est retenu.
const DEPARTAGE_DES_ALBUMS: [Filtre; 4] = [edition_ok, compositeur_ok, pistes_ok, officiel];

/// Le choix du PRESSAGE dans un album retenu.
const DEPARTAGE_DES_PRESSAGES: [Filtre; 3] = [edition_ok, pistes_ok, officiel];

/// Les pressages d'un même album : même groupe de sortie, à défaut même titre
/// compact et même artiste.
struct Groupe {
    score: i32,
    membres: Vec<usize>,
}

fn cle_de_groupe(c: &MBReleaseMatch) -> String {
    match c.release_group_id.as_deref() {
        Some(g) if !g.is_empty() => format!("rg:{g}"),
        _ => format!(
            "titre:{}\u{1f}{}",
            normalize_compact(&c.title),
            normalize(&c.artist)
        ),
    }
}

fn grouper(candidats: &[MBReleaseMatch]) -> Vec<Groupe> {
    let mut cles: Vec<String> = Vec::new();
    let mut groupes: Vec<Groupe> = Vec::new();
    for (i, c) in candidats.iter().enumerate() {
        let cle = cle_de_groupe(c);
        match cles.iter().position(|k| *k == cle) {
            Some(g) => {
                groupes[g].membres.push(i);
                groupes[g].score = groupes[g].score.max(c.score);
            }
            None => {
                cles.push(cle);
                groupes.push(Groupe {
                    score: c.score,
                    membres: vec![i],
                });
            }
        }
    }
    // Tri stable : à score égal, l'ordre de la liste (celui de
    // `rank_candidates`) est gardé.
    groupes.sort_by_key(|g| std::cmp::Reverse(g.score));
    groupes
}

/// Le pressage à retenir dans la liste `candidats` (classée, meilleur
/// d'abord), ou [`Choix::Ambigu`] quand plusieurs albums se valent.
///
/// `titre` et `artiste` sont ceux de la bibliothèque ; `pistes` le nombre de
/// pistes locales. Voir l'en-tête du module pour la règle.
pub fn choisir_le_pressage(
    candidats: &[MBReleaseMatch],
    titre: &str,
    artiste: &str,
    pistes: Option<u32>,
) -> Choix {
    match candidats.len() {
        0 => return Choix::Aucun,
        1 => return Choix::Retenu(0),
        _ => {}
    }
    let indices = Indices::depuis(titre, artiste, pistes);
    let groupes = grouper(candidats);

    let gagnant =
        if groupes.len() == 1 || score_nettement_devant(groupes[0].score, groupes[1].score) {
            Some(0)
        } else {
            departager_les_albums(&groupes, candidats, &indices)
        };
    match gagnant {
        Some(g) => Choix::Retenu(choisir_dans_le_groupe(
            &groupes[g].membres,
            candidats,
            &indices,
        )),
        None => Choix::Ambigu,
    }
}

fn departager_les_albums(
    groupes: &[Groupe],
    candidats: &[MBReleaseMatch],
    indices: &Indices,
) -> Option<usize> {
    // Le peloton : les albums que le score n'a pas su séparer du premier. Un
    // album à plus de dix points ne le dispute plus.
    let tete = groupes[0].score;
    let mut peloton: Vec<usize> = (0..groupes.len())
        .filter(|g| groupes[*g].score > tete - AVANCE_MIN)
        .collect();
    let passe = |g: usize, f: Filtre| {
        groupes[g]
            .membres
            .iter()
            .any(|m| f(&candidats[*m], indices))
    };

    // 🔴 Le titre est OBLIGATOIRE pour gagner un départage. Sans cette
    //    exigence, le nombre de pistes départageait seul des albums
    //    différents : `Buddha-Bar` (26 fichiers) retenait `Buddha-Bar Best
    //    Collection`, qui a aussi 26 pistes (banc du choix, 05/10/2026). Un
    //    album seul dans son peloton, sous le score sûr, passe la même porte.
    peloton.retain(|g| passe(*g, titre_ok));
    match peloton.len() {
        0 => return None,
        1 => return Some(peloton[0]),
        _ => {}
    }
    for f in DEPARTAGE_DES_ALBUMS {
        let gardes: Vec<usize> = peloton.iter().copied().filter(|g| passe(*g, f)).collect();
        if gardes.len() == 1 {
            return Some(gardes[0]);
        }
        if !gardes.is_empty() {
            peloton = gardes;
        }
    }
    None
}

/// Le pressage d'un album retenu : l'édition nommée par le titre, puis le
/// nombre de pistes, puis `Official`, puis le mieux classé. Plusieurs
/// pressages qui passent tout se valent pour les fichiers ; la garde de
/// complétude jugera celui-ci sur sa liste de pistes.
fn choisir_dans_le_groupe(
    membres: &[usize],
    candidats: &[MBReleaseMatch],
    indices: &Indices,
) -> usize {
    let mut pool: Vec<usize> = membres.to_vec();
    for f in DEPARTAGE_DES_PRESSAGES {
        let gardes: Vec<usize> = pool
            .iter()
            .copied()
            .filter(|m| f(&candidats[*m], indices))
            .collect();
        if !gardes.is_empty() {
            pool = gardes;
        }
    }
    pool[0]
}

// -- Garde de complétude --

/// Les fichiers d'un album confrontés à la liste de pistes d'un pressage.
/// Repris de MetaRust (`completeness_against_release`), aux types de Tune.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completude {
    /// Pistes du pressage.
    pub attendues: usize,
    /// Pistes du pressage qu'un fichier occupe, à son (disque, rang).
    pub presentes: usize,
    /// Pistes du pressage qu'aucun fichier n'occupe.
    pub manquantes: usize,
    /// Fichiers de trop sur un (disque, rang) déjà pris.
    pub en_double: usize,
    /// Fichiers numérotés dont le (disque, rang) n'existe pas dans le pressage.
    pub en_trop: usize,
    /// Fichiers sans numéro de piste : ils ne se placent pas.
    pub sans_numero: usize,
    /// Fichiers en tout.
    pub fichiers: usize,
}

impl Completude {
    /// 🔴 Le seuil de garde : le pressage colle-t-il assez aux fichiers pour
    /// qu'on écrive ses identifiants ?
    ///
    /// - aucun fichier en double ni en trop : un fichier n° 15 face à un
    ///   pressage de 14 pistes, c'est une autre édition ;
    /// - pas plus de fichiers que de pistes ;
    /// - si `exiger_la_moitie`, au moins la moitié des pistes présentes : un
    ///   album de 12 fichiers face au coffret de 40 pistes n'est pas ce
    ///   coffret. Les fichiers sans numéro comptent pour des présents
    ///   possibles.
    pub fn acceptable(&self, exiger_la_moitie: bool) -> bool {
        if self.attendues == 0 || self.en_double > 0 || self.en_trop > 0 {
            return false;
        }
        if self.fichiers > self.attendues {
            return false;
        }
        if !exiger_la_moitie {
            return true;
        }
        let presents_possibles = self.presentes + self.sans_numero.min(self.manquantes);
        presents_possibles * 2 >= self.attendues
    }
}

/// Le rapport de complétude des fichiers `locales` face aux pistes `mb`.
///
/// Un disque `0` (balise absente) vaut le disque 1, comme chez MetaRust.
pub fn completude(locales: &[LocalTrack], mb: &[MBTrack]) -> Completude {
    use std::collections::{BTreeMap, BTreeSet};
    let du_pressage: BTreeSet<(u32, u32)> =
        mb.iter().map(|t| (t.disc.max(1), t.position)).collect();
    let mut occupees: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    let mut sans_numero = 0;
    for l in locales {
        if l.position <= 0 {
            sans_numero += 1;
            continue;
        }
        let cle = (l.disc.max(1) as u32, l.position as u32);
        *occupees.entry(cle).or_default() += 1;
    }
    let en_double = occupees.values().map(|n| n - 1).sum();
    let en_trop = occupees.keys().filter(|k| !du_pressage.contains(k)).count();
    let presentes = du_pressage
        .iter()
        .filter(|k| occupees.contains_key(k))
        .count();
    Completude {
        attendues: du_pressage.len(),
        presentes,
        manquantes: du_pressage.len() - presentes,
        en_double,
        en_trop,
        sans_numero,
        fichiers: locales.len(),
    }
}

// -- Identifiants des balises --

/// `true` pour un MBID : 36 caractères, tirets aux places 8, 13, 18, 23,
/// hexadécimal ailleurs. Repris de MetaRust (`is_mbid`).
pub fn est_un_mbid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Le MBID contenu dans `brut` : un UUID nu, une URL MusicBrainz, ou un texte
/// qui en contient un. Rendu en minuscules. Repris de MetaRust
/// (`parse_release_mbid`).
pub fn normaliser_mbid(brut: &str) -> Option<String> {
    let ligne = brut
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    let candidat = ligne
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| c == '"' || c == '\'');
    if est_un_mbid(candidat) {
        return Some(candidat.to_ascii_lowercase());
    }
    for part in candidat.split(['/', '?', '#']) {
        if est_un_mbid(part) {
            return Some(part.to_ascii_lowercase());
        }
    }
    // Un UUID collé dans un texte : on le cherche par fenêtres de 36 octets.
    // `get` et non l'indexation : une fenêtre peut couper un caractère.
    let n = candidat.len();
    (0..n.saturating_sub(35))
        .filter_map(|i| candidat.get(i..i + 36))
        .find(|s| est_un_mbid(s))
        .map(str::to_ascii_lowercase)
}

/// Le MBID de release que porte **la majorité** des pistes d'un album : au
/// moins la moitié de ses `pistes`, et strictement plus que tout autre. Un
/// dossier qui mélange deux releases à parts égales n'en désigne aucune.
pub fn release_majoritaire(balises: &[String], pistes: usize) -> Option<String> {
    use std::collections::BTreeMap;
    if pistes == 0 {
        return None;
    }
    let mut comptes: BTreeMap<String, usize> = BTreeMap::new();
    for b in balises {
        if let Some(id) = normaliser_mbid(b) {
            *comptes.entry(id).or_default() += 1;
        }
    }
    let mut tries: Vec<(String, usize)> = comptes.into_iter().collect();
    tries.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let (id, n) = tries.first()?.clone();
    let second = tries.get(1).map_or(0, |(_, n)| *n);
    (n * 2 >= pistes && n > second).then_some(id)
}

/// Un code-barres EAN/UPC lisible : chiffres seuls (espaces et tirets
/// retirés), 8 à 14 chiffres.
pub fn normaliser_code_barres(brut: &str) -> Option<String> {
    let chiffres: String = brut.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    ((8..=14).contains(&chiffres.len()) && chiffres.chars().all(|c| c.is_ascii_digit()))
        .then_some(chiffres)
}

/// Le code-barres le plus fréquent parmi les balises, s'il y en a un lisible.
fn code_barres_de_l_album(balises: &[String]) -> Option<String> {
    use std::collections::BTreeMap;
    let mut comptes: BTreeMap<String, usize> = BTreeMap::new();
    for b in balises {
        if let Some(c) = normaliser_code_barres(b) {
            *comptes.entry(c).or_default() += 1;
        }
    }
    comptes.into_iter().max_by_key(|(_, n)| *n).map(|(c, _)| c)
}

/// Les releases d'une réponse `/recording/{id}?inc=releases`, plausibles pour
/// cet album. Repris de MetaRust (`parse_recording_releases`), aux types de
/// Tune : chaque release vaut 100, la réponse ne note pas.
pub fn releases_de_l_enregistrement(
    data: &Value,
    titre: &str,
    artiste: &str,
) -> Vec<MBReleaseMatch> {
    let Some(releases) = data.get("releases").and_then(|r| r.as_array()) else {
        return Vec::new();
    };
    let interroge = mb::titre_de_requete_pour(titre, artiste);
    releases
        .iter()
        .filter_map(|rel| {
            let release_id = str_field(rel, "id")?;
            let title = str_field(rel, "title").unwrap_or_default();
            let artist = artist_credit(rel);
            let ok = plausible(&title, &artist, titre, artiste)
                || interroge
                    .as_deref()
                    .is_some_and(|q| plausible(&title, &artist, q, artiste));
            if !ok {
                return None;
            }
            let date = str_field(rel, "date");
            Some(MBReleaseMatch {
                release_id,
                release_group_id: rel.get("release-group").and_then(|g| str_field(g, "id")),
                title,
                artist,
                score: 100,
                year: date.as_deref().and_then(|d| d.get(0..4)?.parse().ok()),
                date,
                country: str_field(rel, "country"),
                track_count: rel
                    .get("track-count")
                    .and_then(|t| t.as_u64())
                    .map(|n| n as u32),
                disambiguation: str_field(rel, "disambiguation"),
                status: str_field(rel, "status"),
                ..Default::default()
            })
        })
        .collect()
}

// -- La cascade --

/// Ce que le pilote sait d'un album, lu en base.
#[derive(Debug, Clone, Copy)]
pub struct EntreeDIdentification<'a> {
    /// Titre de l'album, tel que la bibliothèque le porte.
    pub titre: &'a str,
    /// Artiste à interroger (`artiste_de_requete`).
    pub artiste: &'a str,
    /// Les fichiers de l'album.
    pub pistes: &'a [LocalTrack],
    /// `track_metadata.mb_release_id` des pistes (balise
    /// `MUSICBRAINZ_ALBUMID`).
    pub releases_des_balises: &'a [String],
    /// `track_metadata.mb_track_id` des pistes (balise `MUSICBRAINZ_TRACKID`),
    /// dans l'ordre des pistes.
    pub enregistrements_des_balises: &'a [String],
    /// `track_metadata.barcode` des pistes, puis `albums.barcode`.
    pub codes_barres: &'a [String],
}

/// D'où vient le pressage retenu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceDuPressage {
    /// Le MBID de release majoritaire des balises.
    BaliseRelease,
    /// Un MBID d'enregistrement des balises, vers ses releases.
    BaliseEnregistrement,
    /// Le code-barres des balises.
    CodeBarres,
    /// La recherche par titre et artiste.
    Recherche,
    /// L'édition choisie par l'utilisateur (`?release_id=`), après une
    /// réponse `ambiguous` du bouton « Ré-identifier ».
    ChoixUtilisateur,
}

impl SourceDuPressage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BaliseRelease => "balise_release",
            Self::BaliseEnregistrement => "balise_enregistrement",
            Self::CodeBarres => "code_barres",
            Self::Recherche => "recherche",
            Self::ChoixUtilisateur => "choix_utilisateur",
        }
    }
}

/// Pourquoi un album est ambigu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaisonAmbigu {
    /// Plusieurs albums se valent, et rien ne les départage.
    AlbumsConcurrents,
    /// Le pressage retenu ne colle pas aux fichiers ([`Completude`]).
    PistesIncompatibles,
    /// La liste de pistes du pressage retenu est introuvable.
    DetailIndisponible,
}

impl RaisonAmbigu {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlbumsConcurrents => "albums_concurrents",
            Self::PistesIncompatibles => "pistes_incompatibles",
            Self::DetailIndisponible => "detail_indisponible",
        }
    }
}

/// L'issue de la cascade pour un album.
#[derive(Debug, Clone)]
pub enum IssueDuChoix {
    /// Un pressage sûr, sa liste de pistes, et ce qu'en dit la garde.
    Retenu {
        pressage: MBReleaseMatch,
        detail: MBReleaseDetail,
        source: SourceDuPressage,
        completude: Completude,
    },
    /// MusicBrainz a répondu, mais rien de sûr : ne rien écrire.
    Ambigu {
        raison: RaisonAmbigu,
        source: SourceDuPressage,
        /// Les candidats que rien n'a départagés (ou le pressage refusé par
        /// la garde), classés : ce que l'utilisateur peut choisir.
        candidats: Vec<MBReleaseMatch>,
    },
    /// MusicBrainz a répondu, il n'a rien.
    Introuvable,
    /// MusicBrainz n'a pas répondu (#4991).
    Refus(RefusMusicBrainz),
}

/// La dernière ambiguïté vue par la cascade : sa raison, son étape, ses
/// candidats.
type Ambiguite = Option<(RaisonAmbigu, SourceDuPressage, Vec<MBReleaseMatch>)>;

/// L'édition que l'utilisateur a CHOISIE (`?release_id=` du bouton
/// « Ré-identifier », après une réponse `ambiguous`) : une lecture, et la pose
/// sans la garde de complétude — c'est son choix, la complétude est rendue
/// pour information. `None` : `release_id` n'est pas un MBID.
pub async fn lire_le_pressage_choisi<L, FutL>(
    release_id: &str,
    pistes: &[LocalTrack],
    mut lire: L,
) -> Option<IssueDuChoix>
where
    L: FnMut(String, &'static str) -> FutL,
    FutL: std::future::Future<Output = Result<Option<Value>, RefusMusicBrainz>>,
{
    let id = normaliser_mbid(release_id)?;
    Some(match lire(format!("release/{id}"), INC_DETAIL).await {
        Err(refus) => IssueDuChoix::Refus(refus),
        Ok(None) => IssueDuChoix::Introuvable,
        Ok(Some(data)) => match parse_release_detail(&data) {
            None => IssueDuChoix::Introuvable,
            Some(detail) => IssueDuChoix::Retenu {
                pressage: pressage_du_detail(&data, &detail),
                completude: completude(pistes, &detail.tracks),
                detail,
                source: SourceDuPressage::ChoixUtilisateur,
            },
        },
    })
}

/// Une requête MusicBrainz sur deux au moins attend son créneau : la
/// première est couverte par le délai que le pilote place entre deux albums.
#[derive(Default)]
struct Cadence {
    faites: usize,
}

impl Cadence {
    async fn avant_une_requete(&mut self) {
        if self.faites > 0 {
            mb::rate_limit_delay().await;
        }
        self.faites += 1;
    }
}

/// Le pressage reconstitué d'une réponse `/release/{id}`, quand il ne vient
/// pas d'une recherche (MBID des balises).
fn pressage_du_detail(data: &Value, detail: &MBReleaseDetail) -> MBReleaseMatch {
    MBReleaseMatch {
        release_id: detail.release_id.clone(),
        release_group_id: data.get("release-group").and_then(|g| str_field(g, "id")),
        title: detail.title.clone(),
        artist: detail.artist.clone(),
        score: 100,
        date: detail.date.clone(),
        year: detail.year,
        country: detail.country.clone(),
        label: detail.label.clone(),
        catalog_number: detail.catalog_number.clone(),
        track_count: Some(detail.tracks.len() as u32),
        disc_count: Some(detail.disc_count),
        media_format: None,
        disambiguation: str_field(data, "disambiguation"),
        status: str_field(data, "status"),
    }
}

/// Juge un pressage sur sa liste de pistes : `Ok` pour l'écrire.
fn conclure(
    data: &Value,
    depuis_recherche: Option<MBReleaseMatch>,
    source: SourceDuPressage,
    pistes: &[LocalTrack],
    exiger_la_moitie: bool,
) -> Result<IssueDuChoix, RaisonAmbigu> {
    let Some(detail) = parse_release_detail(data) else {
        return Err(RaisonAmbigu::DetailIndisponible);
    };
    let mut pressage = depuis_recherche.unwrap_or_else(|| pressage_du_detail(data, &detail));
    if pressage.release_group_id.is_none() {
        pressage.release_group_id = data.get("release-group").and_then(|g| str_field(g, "id"));
    }
    let completude = completude(pistes, &detail.tracks);
    if !completude.acceptable(exiger_la_moitie) {
        debug!(
            release_id = %pressage.release_id,
            source = source.as_str(),
            ?completude,
            "choix_pressage_pistes_incompatibles"
        );
        return Err(RaisonAmbigu::PistesIncompatibles);
    }
    Ok(IssueDuChoix::Retenu {
        pressage,
        detail,
        source,
        completude,
    })
}

/// 🔴 La cascade d'identification d'un album, **sans le transport** (#4805).
///
/// 1. MBID de release majoritaire des balises → sa liste de pistes. Les
///    balises nomment la release : si elle ne colle pas aux fichiers, l'album
///    est ambigu, la recherche texte ne la contredit pas.
/// 2. MBID d'enregistrement → ses releases → [`choisir_le_pressage`].
/// 3. Code-barres → recherche `barcode:` → [`choisir_le_pressage`].
/// 4. Recherche par titre et artiste → [`choisir_le_pressage`].
///
/// Une étape 2 ou 3 ambiguë laisse sa chance à la suivante. Un MBID inconnu de
/// MusicBrainz (`404`) aussi. Un refus arrête tout et se rend tel quel : le
/// disjoncteur du pilote en dépend.
///
/// `rechercher(requete, fetch)` fait une recherche `/release?query=` ;
/// `lire(chemin, inc)` une lecture, `Ok(None)` pour un identifiant inconnu.
/// En production : [`mb::rechercher_sur_musicbrainz`] et
/// [`mb::lire_sur_musicbrainz`].
pub async fn identifier_le_pressage<R, FutR, L, FutL>(
    entree: EntreeDIdentification<'_>,
    mut rechercher: R,
    mut lire: L,
) -> IssueDuChoix
where
    R: FnMut(String, usize) -> FutR,
    FutR: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
    L: FnMut(String, &'static str) -> FutL,
    FutL: std::future::Future<Output = Result<Option<Value>, RefusMusicBrainz>>,
{
    let titre = entree.titre;
    let artiste = entree.artiste;
    let n = entree.pistes.len() as u32;
    let pistes = (n > 0).then_some(n);
    let mut cadence = Cadence::default();
    let mut ambiguite: Ambiguite = None;

    // 1. Le MBID de release des balises.
    if let Some(id) = release_majoritaire(entree.releases_des_balises, entree.pistes.len()) {
        cadence.avant_une_requete().await;
        match lire(format!("release/{id}"), INC_DETAIL).await {
            Err(refus) => return IssueDuChoix::Refus(refus),
            Ok(None) => debug!(release_id = %id, "choix_pressage_mbid_balise_inconnu"),
            Ok(Some(data)) => {
                // Pas d'exigence de moitié : un album partiel bien balisé reste
                // cet album.
                return conclure(
                    &data,
                    None,
                    SourceDuPressage::BaliseRelease,
                    entree.pistes,
                    false,
                )
                .unwrap_or_else(|raison| IssueDuChoix::Ambigu {
                    raison,
                    source: SourceDuPressage::BaliseRelease,
                    candidats: parse_release_detail(&data)
                        .map(|d| vec![pressage_du_detail(&data, &d)])
                        .unwrap_or_default(),
                });
            }
        }
    }

    // Les étapes 2 à 4 : une liste de candidats, un choix, une liste de pistes.
    enum Etape {
        Conclue(IssueDuChoix),
        Suivante,
    }
    async fn juger<L, FutL>(
        candidats: &[MBReleaseMatch],
        source: SourceDuPressage,
        entree: &EntreeDIdentification<'_>,
        pistes: Option<u32>,
        cadence: &mut Cadence,
        lire: &mut L,
        ambiguite: &mut Ambiguite,
    ) -> Etape
    where
        L: FnMut(String, &'static str) -> FutL,
        FutL: std::future::Future<Output = Result<Option<Value>, RefusMusicBrainz>>,
    {
        match choisir_le_pressage(candidats, entree.titre, entree.artiste, pistes) {
            Choix::Aucun => Etape::Suivante,
            Choix::Ambigu => {
                *ambiguite = Some((RaisonAmbigu::AlbumsConcurrents, source, candidats.to_vec()));
                Etape::Suivante
            }
            Choix::Retenu(i) => {
                let retenu = candidats[i].clone();
                cadence.avant_une_requete().await;
                match lire(format!("release/{}", retenu.release_id), INC_DETAIL).await {
                    Err(refus) => Etape::Conclue(IssueDuChoix::Refus(refus)),
                    Ok(None) => {
                        *ambiguite = Some((RaisonAmbigu::DetailIndisponible, source, vec![retenu]));
                        Etape::Suivante
                    }
                    Ok(Some(data)) => {
                        match conclure(&data, Some(retenu.clone()), source, entree.pistes, true) {
                            Ok(issue) => Etape::Conclue(issue),
                            Err(raison) => {
                                *ambiguite = Some((raison, source, vec![retenu]));
                                Etape::Suivante
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Un MBID d'enregistrement des balises.
    if let Some(rid) = entree
        .enregistrements_des_balises
        .iter()
        .find_map(|b| normaliser_mbid(b))
    {
        cadence.avant_une_requete().await;
        match lire(format!("recording/{rid}"), INC_ENREGISTREMENT).await {
            Err(refus) => return IssueDuChoix::Refus(refus),
            Ok(None) => debug!(recording_id = %rid, "choix_pressage_enregistrement_inconnu"),
            Ok(Some(data)) => {
                let candidats = releases_de_l_enregistrement(&data, titre, artiste);
                if let Etape::Conclue(issue) = juger(
                    &candidats,
                    SourceDuPressage::BaliseEnregistrement,
                    &entree,
                    pistes,
                    &mut cadence,
                    &mut lire,
                    &mut ambiguite,
                )
                .await
                {
                    return issue;
                }
            }
        }
    }

    // 3. Le code-barres des balises.
    if let Some(code) = code_barres_de_l_album(entree.codes_barres) {
        cadence.avant_une_requete().await;
        match rechercher(format!("barcode:{code}"), FETCH_RECHERCHE).await {
            Err(refus) => return IssueDuChoix::Refus(refus),
            Ok(data) => {
                let mut candidats = parse_search_results(&data, titre, artiste);
                if candidats.is_empty()
                    && let Some(q) = mb::titre_de_requete_pour(titre, artiste)
                {
                    candidats = parse_search_results(&data, &q, artiste);
                }
                if let Etape::Conclue(issue) = juger(
                    &candidats,
                    SourceDuPressage::CodeBarres,
                    &entree,
                    pistes,
                    &mut cadence,
                    &mut lire,
                    &mut ambiguite,
                )
                .await
                {
                    return issue;
                }
            }
        }
    }

    // 4. La recherche texte.
    cadence.avant_une_requete().await;
    let recherche = mb::recherche_de_pressages_complete(
        titre,
        artiste,
        pistes,
        FETCH_RECHERCHE,
        &mut rechercher,
    )
    .await;
    if let Some(refus) = recherche.refus {
        return IssueDuChoix::Refus(refus);
    }
    if let Etape::Conclue(issue) = juger(
        &recherche.candidats,
        SourceDuPressage::Recherche,
        &entree,
        pistes,
        &mut cadence,
        &mut lire,
        &mut ambiguite,
    )
    .await
    {
        return issue;
    }
    match ambiguite {
        Some((raison, source, candidats)) => IssueDuChoix::Ambigu {
            raison,
            source,
            candidats,
        },
        None => IssueDuChoix::Introuvable,
    }
}

#[cfg(test)]
#[path = "choix_de_pressage_tests.rs"]
mod tests;
