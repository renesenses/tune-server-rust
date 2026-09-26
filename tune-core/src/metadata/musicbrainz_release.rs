//! MusicBrainz release lookup.
//!
//! Two shapes of question, because they need opposite queries:
//!
//! * "which release is this, exactly?" — [`lookup_release`] narrows the search
//!   with the track count and year and returns only a high-confidence hit.
//! * "which releases could this be?" — [`lookup_release_candidates`] deliberately
//!   does *not* constrain on track count, because that is precisely what tells a
//!   deluxe edition apart from the standard one, and returns the list with the
//!   details a human needs to choose (year, country, label, track count, format).
//!
//! Response parsing is split into pure functions so the field extraction — the
//! part that actually breaks when MusicBrainz reshapes its JSON — is testable
//! without a network call.
//!
//! # 🔴 Refus ≠ absence (#4991)
//!
//! [`lookup_release_candidates`] rendait `Vec::new()` **aussi bien** quand
//! MusicBrainz n'avait aucun pressage que lorsqu'il avait refusé la requête
//! (`503`, coupure, délai dépassé) : le `None` de [`mb_get`] était avalé par un
//! `else { return Vec::new() }`. Les deux cas arrivaient donc à l'appelant sous
//! la même forme, et **aucun compteur en aval ne pouvait être juste**.
//!
//! Mesuré le 25/09/2026 sur le .18 : douze albums de musique classique
//! contigus par identifiant — introuvables pour une raison structurelle, le
//! compositeur n'étant pas dans la requête — arrêtaient la passe
//! `POST /library/identify-all` en annonçant « MusicBrainz injoignable », alors
//! que MusicBrainz répondait `200` en 0,15 s.
//!
//! La recherche rend désormais [`RechercheDePressages`], qui porte la liste
//! **et** le [`RefusMusicBrainz`] éventuel. Une liste vide sans refus veut dire
//! ce qu'elle dit : MusicBrainz a répondu, il n'a pas ce pressage.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

const MB_API: &str = "https://musicbrainz.org/ws/2";

/// La base effectivement interrogée. [`MB_API`] en service ; une doublure
/// locale dans les tests (#4836), posée par [`remplacer_la_base_musicbrainz`].
static BASE_REMPLACEE: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

fn base_musicbrainz() -> String {
    BASE_REMPLACEE
        .read()
        .ok()
        .and_then(|b| b.clone())
        .unwrap_or_else(|| MB_API.to_string())
}

/// Remplace la base MusicBrainz par une doublure locale — **tests seulement**
/// (#4836). Aucun chemin de production ne l'appelle : sans elle, toutes les
/// requêtes de ce module partent vers [`MB_API`], avec le même User-Agent et
/// le même limiteur de débit.
#[doc(hidden)]
pub fn remplacer_la_base_musicbrainz(base: Option<String>) {
    if let Ok(mut b) = BASE_REMPLACEE.write() {
        *b = base;
    }
}

/// Le MBID du pseudo-label « [no label] » de MusicBrainz : la release dit
/// explicitement qu'elle n'a PAS de label. L'écrire comme label d'album serait
/// poser un faux nom (#4836).
pub const MBID_SANS_LABEL: &str = "157afde4-4bf5-4039-8ad2-5a15acc85176";

/// Le label et le numéro de catalogue à retenir d'un tableau `label-info`
/// (#4836).
///
/// Règle : le **premier** label dont le nom est non vide et qui n'est pas le
/// pseudo-label « [no label] » (reconnu par son MBID ou par son nom, casse
/// ignorée). Le numéro de catalogue est celui de CETTE entrée ; à défaut, le
/// premier numéro non vide du tableau — un numéro seul reste utile, et
/// « [none] » (la convention MusicBrainz pour « pas de numéro ») est écarté.
pub fn choisir_label(label_info: Option<&Value>) -> (Option<String>, Option<String>) {
    let Some(infos) = label_info.and_then(|l| l.as_array()) else {
        return (None, None);
    };
    let catalogue =
        |i: &Value| str_field(i, "catalog-number").filter(|c| !c.eq_ignore_ascii_case("[none]"));
    let retenue = infos.iter().find_map(|i| {
        let label = i.get("label")?;
        let nom = str_field(label, "name")?;
        let pseudo = str_field(label, "id").as_deref() == Some(MBID_SANS_LABEL)
            || nom.eq_ignore_ascii_case("[no label]");
        if pseudo {
            None
        } else {
            Some((nom, catalogue(i)))
        }
    });
    match retenue {
        Some((nom, Some(cat))) => (Some(nom), Some(cat)),
        Some((nom, None)) => (Some(nom), infos.iter().find_map(catalogue)),
        None => (None, infos.iter().find_map(catalogue)),
    }
}
/// Public depuis #4863 : le greffon de lecture de CD consulte `/ws/2/discid`
/// sous la même identité. Son DÉBIT, lui, passe par [`rate_limit_delay`] (#4767).
pub const MB_UA: &str = "TuneServer/1.0 (contact@mozaiklabs.fr)";

/// Below this score a search hit is noise rather than a match.
const MIN_CONFIDENT_SCORE: i32 = 80;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MBReleaseMatch {
    pub release_id: String,
    pub release_group_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub score: i32,
    // -- Edition details --
    //
    // What lets someone pick between the seven releases MusicBrainz holds for a
    // popular album. All optional: MusicBrainz is a wiki and any of them can be
    // absent. `#[serde(default)]` keeps older stored payloads deserializable.
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub catalog_number: Option<String>,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub disc_count: Option<u32>,
    /// `CD`, `Digital Media`, `12" Vinyl`, …
    #[serde(default)]
    pub media_format: Option<String>,
    /// MusicBrainz's own edition note, e.g. "deluxe edition", "reissue".
    #[serde(default)]
    pub disambiguation: Option<String>,
    /// `Official`, `Promotion`, `Bootleg`, …
    #[serde(default)]
    pub status: Option<String>,
}

/// One track of a chosen release.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MBTrack {
    /// Position within its disc, 1-based.
    pub position: u32,
    /// Disc (medium) number, 1-based.
    pub disc: u32,
    /// Printed number when it differs from the position (vinyl sides: `A1`).
    pub number: Option<String>,
    pub title: String,
    pub length_ms: Option<u64>,
    pub recording_id: Option<String>,
    /// Set only when the track credits someone other than the release artist —
    /// the useful case being a compilation.
    pub artist: Option<String>,
}

/// A release with its track listing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MBReleaseDetail {
    pub release_id: String,
    pub title: String,
    pub artist: String,
    pub date: Option<String>,
    pub year: Option<u32>,
    pub country: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub disc_count: u32,
    pub tracks: Vec<MBTrack>,
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn year_from_date(date: Option<&str>) -> Option<u32> {
    date?.get(0..4)?.parse().ok()
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Join an `artist-credit` array into a display string, honouring the
/// join phrases so "Queen & David Bowie" does not come out as "Queen David
/// Bowie".
fn artist_credit(v: &Value) -> String {
    let Some(credits) = v.get("artist-credit").and_then(|c| c.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for credit in credits {
        let name = credit
            .get("name")
            .and_then(|n| n.as_str())
            .or_else(|| {
                credit
                    .get("artist")
                    .and_then(|a| a.get("name"))
                    .and_then(|n| n.as_str())
            })
            .unwrap_or("");
        out.push_str(name);
        if let Some(join) = credit.get("joinphrase").and_then(|j| j.as_str()) {
            out.push_str(join);
        }
    }
    out.trim().to_string()
}

/// Does this search hit plausibly refer to what we asked for?
///
/// MusicBrainz happily returns loosely-related releases; without this a search
/// for one album offers up the artist's whole discography as "candidates".
fn plausible(rel_title: &str, rel_artist: &str, want_title: &str, want_artist: &str) -> bool {
    let norm_title = normalize(want_title);
    let norm_rel_title = normalize(rel_title);
    if norm_rel_title != norm_title
        && !norm_title.contains(&norm_rel_title)
        && !norm_rel_title.contains(&norm_title)
    {
        return false;
    }

    let norm_artist = normalize(want_artist);
    let norm_rel_artist = normalize(rel_artist);
    if !norm_artist.is_empty()
        && !norm_rel_artist.is_empty()
        && !norm_artist.contains(&norm_rel_artist)
        && !norm_rel_artist.contains(&norm_artist)
    {
        return false;
    }

    true
}

/// Pull every plausible release out of a `/release` search response.
///
/// Pure: this is the part that breaks when the API reshapes, so it is tested
/// against captured payloads instead of the live service.
pub fn parse_search_results(
    data: &Value,
    want_title: &str,
    want_artist: &str,
) -> Vec<MBReleaseMatch> {
    let Some(releases) = data.get("releases").and_then(|r| r.as_array()) else {
        return Vec::new();
    };

    let mut out: Vec<MBReleaseMatch> = Vec::new();
    for rel in releases {
        let Some(release_id) = str_field(rel, "id") else {
            continue;
        };
        let rel_title = str_field(rel, "title").unwrap_or_default();
        let rel_artist = artist_credit(rel);

        if !plausible(&rel_title, &rel_artist, want_title, want_artist) {
            continue;
        }

        // Media: a release can span several discs, possibly of mixed formats.
        let media = rel.get("media").and_then(|m| m.as_array());
        let disc_count = media.map(|m| m.len() as u32).filter(|n| *n > 0);
        let media_format = media.and_then(|m| {
            let mut formats: Vec<String> =
                m.iter().filter_map(|x| str_field(x, "format")).collect();
            formats.dedup();
            if formats.is_empty() {
                None
            } else {
                Some(formats.join(" + "))
            }
        });
        // `track-count` at the top level covers the whole release; fall back to
        // summing the media when the search index omits it.
        let track_count = rel
            .get("track-count")
            .and_then(|t| t.as_u64())
            .map(|n| n as u32)
            .or_else(|| {
                media.map(|m| {
                    m.iter()
                        .filter_map(|x| x.get("track-count").and_then(|t| t.as_u64()))
                        .sum::<u64>() as u32
                })
            })
            .filter(|n| *n > 0);

        let (label, catalog_number) = choisir_label(rel.get("label-info"));

        let date = str_field(rel, "date");
        let country = str_field(rel, "country").or_else(|| {
            rel.get("release-events")
                .and_then(|e| e.as_array())
                .and_then(|events| {
                    events.iter().find_map(|e| {
                        e.get("area")
                            .and_then(|a| str_field(a, "iso-3166-1-codes"))
                            .or_else(|| e.get("area").and_then(|a| str_field(a, "name")))
                    })
                })
        });

        out.push(MBReleaseMatch {
            release_id,
            release_group_id: rel.get("release-group").and_then(|g| str_field(g, "id")),
            title: rel_title,
            artist: rel_artist,
            score: rel.get("score").and_then(|s| s.as_i64()).unwrap_or(0) as i32,
            year: year_from_date(date.as_deref()),
            date,
            country,
            label,
            catalog_number,
            track_count,
            disc_count,
            media_format,
            disambiguation: str_field(rel, "disambiguation"),
            status: str_field(rel, "status"),
        });
    }

    // The same release can surface twice across paginated indexes.
    out.dedup_by(|a, b| a.release_id == b.release_id);
    out
}

/// Order candidates for a human: best score first, and among equal scores the
/// one whose track count matches what is on disk.
fn rank_candidates(
    mut candidates: Vec<MBReleaseMatch>,
    track_hint: Option<u32>,
) -> Vec<MBReleaseMatch> {
    candidates.sort_by_key(|c| {
        let delta = match (track_hint, c.track_count) {
            (Some(hint), Some(tc)) => (tc as i64 - hint as i64).abs(),
            // Unknown track count sorts after a known mismatch of one track:
            // an edition we cannot compare is a weaker suggestion.
            (Some(_), None) => 2,
            _ => 0,
        };
        (std::cmp::Reverse(c.score), delta)
    });
    candidates
}

/// Parse a `/release/{id}?inc=recordings` response into a track listing.
pub fn parse_release_detail(data: &Value) -> Option<MBReleaseDetail> {
    let release_id = str_field(data, "id")?;
    let date = str_field(data, "date");

    let (label, catalog_number) = choisir_label(data.get("label-info"));
    let media = data.get("media").and_then(|m| m.as_array());

    let mut tracks: Vec<MBTrack> = Vec::new();
    if let Some(media) = media {
        for (idx, medium) in media.iter().enumerate() {
            // Trust the declared position; fall back to the array order, since
            // some releases omit it on single-disc media.
            let disc = medium
                .get("position")
                .and_then(|p| p.as_u64())
                .map(|n| n as u32)
                .unwrap_or(idx as u32 + 1);

            let Some(list) = medium.get("tracks").and_then(|t| t.as_array()) else {
                continue;
            };
            for (tidx, track) in list.iter().enumerate() {
                let recording = track.get("recording");
                let title = str_field(track, "title")
                    .or_else(|| recording.and_then(|r| str_field(r, "title")))
                    .unwrap_or_default();
                if title.is_empty() {
                    continue;
                }
                let length_ms = track.get("length").and_then(|l| l.as_u64()).or_else(|| {
                    recording
                        .and_then(|r| r.get("length"))
                        .and_then(|l| l.as_u64())
                });
                let credited = artist_credit(track);

                tracks.push(MBTrack {
                    position: track
                        .get("position")
                        .and_then(|p| p.as_u64())
                        .map(|n| n as u32)
                        .unwrap_or(tidx as u32 + 1),
                    disc,
                    number: str_field(track, "number"),
                    title,
                    length_ms,
                    recording_id: recording.and_then(|r| str_field(r, "id")),
                    artist: if credited.is_empty() {
                        None
                    } else {
                        Some(credited)
                    },
                });
            }
        }
    }

    Some(MBReleaseDetail {
        release_id,
        title: str_field(data, "title").unwrap_or_default(),
        artist: artist_credit(data),
        year: year_from_date(date.as_deref()),
        date,
        country: str_field(data, "country"),
        label,
        catalog_number,
        disc_count: media.map(|m| m.len() as u32).unwrap_or(0),
        tracks,
    })
}

// -- Network --

/// Pourquoi MusicBrainz n'a **pas répondu** (#4991).
///
/// 🔴 À ne jamais confondre avec « MusicBrainz n'a pas ce pressage ». Le second
/// est un résultat, sur lequel l'appelant peut conclure ; le premier ne dit
/// rien de l'album et tout du service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusMusicBrainz {
    /// La requête n'est pas partie, ou la réponse n'est jamais arrivée :
    /// coupure réseau, DNS, délai de 15 s dépassé.
    Transport,
    /// MusicBrainz a répondu autre chose qu'un `2xx` — `503` quand la cadence
    /// par IP est dépassée, `500`, `502`…
    Statut(u16),
    /// Réponse reçue, corps illisible. Ce n'est pas MusicBrainz qui dit
    /// « rien » : c'est nous qui n'avons pas compris.
    CorpsIllisible,
}

impl std::fmt::Display for RefusMusicBrainz {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport => write!(f, "transport"),
            Self::Statut(code) => write!(f, "statut_{code}"),
            Self::CorpsIllisible => write!(f, "corps_illisible"),
        }
    }
}

async fn mb_get(path: &str, params: &[(&str, String)]) -> Result<Value, RefusMusicBrainz> {
    let client = crate::http::client::shared();
    let resp = client
        .get(format!("{}/{path}", base_musicbrainz()))
        .query(params)
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| {
            debug!(path = path, error = %e, "mb_request_transport_error");
            RefusMusicBrainz::Transport
        })?;

    if !resp.status().is_success() {
        debug!(status = %resp.status(), path = path, "mb_request_http_error");
        return Err(RefusMusicBrainz::Statut(resp.status().as_u16()));
    }
    resp.json().await.map_err(|e| {
        debug!(path = path, error = %e, "mb_request_body_error");
        RefusMusicBrainz::CorpsIllisible
    })
}

/// Best-guess identification: narrow the query with everything we know and
/// return a hit only if MusicBrainz is confident.
pub async fn lookup_release(
    title: &str,
    artist: &str,
    track_count: Option<i32>,
    year: Option<i32>,
) -> Option<MBReleaseMatch> {
    let mut query_parts = vec![
        format!("release:\"{title}\""),
        format!("artist:\"{artist}\""),
    ];
    if let Some(tc) = track_count {
        query_parts.push(format!("tracks:{tc}"));
    }
    if let Some(y) = year {
        query_parts.push(format!("date:{y}"));
    }

    let data = mb_get(
        "release",
        &[
            ("query", query_parts.join(" AND ")),
            ("limit", "5".to_string()),
            ("fmt", "json".to_string()),
        ],
    )
    .await
    .ok()?;

    parse_search_results(&data, title, artist)
        .into_iter()
        .max_by_key(|m| m.score)
        .filter(|m| m.score >= MIN_CONFIDENT_SCORE)
}

/// Les marqueurs qui trahissent un suffixe de PRESSAGE, pas un morceau du titre.
/// Comparés en minuscules, par inclusion : `192kHz/24bit` porte `khz`, et
/// `Original 1976 Version` porte `version`.
const MARQUEURS_DE_SUFFIXE: &[&str] = &[
    "khz",
    "bit",
    "remaster",
    "deluxe",
    "edition",
    "version",
    "anniversary",
    "expanded",
    "bonus",
    "live",
    "mono",
    "stereo",
    "reissue",
    "dsd",
];

/// Les mots qui introduisent un numéro de disque en fin de titre.
const MOTS_DE_DISQUE: &[&str] = &["disc", "disque", "cd"];

fn contient_un_marqueur(dedans: &str) -> bool {
    let bas = dedans.to_lowercase();
    MARQUEURS_DE_SUFFIXE.iter().any(|m| bas.contains(m))
}

/// Retire UN suffixe de fin. Rend `None` quand il n'y a rien à retirer, ce qui
/// sert de condition d'arrêt à la boucle de [`titre_de_requete`].
fn retire_un_suffixe(titre: &str) -> Option<String> {
    let t = titre.trim_end();

    // 1. Parenthèse ou crochet FINAL dont le contenu porte un marqueur de
    //    pressage. `(Live at Montreux)` s'en va ; `(Part 2)` reste.
    for (ouvre, ferme) in [('(', ')'), ('[', ']')] {
        if !t.ends_with(ferme) {
            continue;
        }
        let Some(pos) = t.rfind(ouvre) else { continue };
        let contenu = &t[pos + ouvre.len_utf8()..t.len() - ferme.len_utf8()];
        if !contient_un_marqueur(contenu) {
            continue;
        }
        let reste = t[..pos].trim_end();
        if !reste.is_empty() {
            return Some(reste.to_string());
        }
    }

    // 2. `, Disc 1` / ` - CD 2` / `, Disque 3` en fin de titre. Le découpage
    //    par disque est une propriété de NOTRE arborescence, pas du pressage :
    //    MusicBrainz décrit le coffret entier sous un seul titre.
    let bas = t.to_lowercase();
    for mot in MOTS_DE_DISQUE {
        // Chercher la dernière occurrence du mot, puis vérifier que tout ce qui
        // suit est un nombre, et que ce qui précède est un séparateur.
        let Some(pos) = bas.rfind(mot) else { continue };
        let apres = t[pos + mot.len()..].trim();
        if apres.is_empty() || !apres.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let avant = t[..pos].trim_end();
        let avant = avant.strip_suffix('-').or_else(|| avant.strip_suffix('–'));
        let Some(avant) = avant.map(str::trim_end).or_else(|| {
            // Sans tiret, il faut une virgule : `Singles and More, Disc 1`.
            t[..pos].trim_end().strip_suffix(',')
        }) else {
            continue;
        };
        let avant = avant.trim_end().trim_end_matches(',').trim_end();
        if !avant.is_empty() {
            return Some(avant.to_string());
        }
    }

    None
}

/// Le titre à envoyer à la RECHERCHE MusicBrainz, quand celui de la bibliothèque
/// porte un suffixe qui la fait échouer.
///
/// Mesuré le 23/09/2026 sur 40 albums du .18 tirés au hasard (#4805) : la
/// recherche telle quelle rend un pressage plausible pour **26 albums sur 39**
/// (66,7 %). Les 13 échecs ont une cause unique et visible — le titre porte un
/// suffixe technique (`(192kHz/24bit)`, `(Remastered)`, `(Deluxe)`,
/// `(Original 1976 Version)`, `, Disc 1`) qui noie la recherche Lucene en amont.
/// Rejouée avec le titre nettoyé, elle en retrouve **6 de plus : 32 sur 39
/// (82,1 %)**, soit **+15,4 points** pour une requête supplémentaire sur le seul
/// tiers d'albums en échec.
///
/// Ne touche QUE la requête : la donnée stockée n'est jamais réécrite, et le
/// filtre [`plausible`] continue de juger contre le titre d'origine — ce qui est
/// plus strict, l'inclusion étant mutuelle (`somethin else 192khz24bit` contient
/// bien `somethin else`).
///
/// Rend `None` quand il n'y a rien à retirer : l'appelant sait alors qu'un
/// second essai serait la même requête, et s'épargne 1,1 s.
pub fn titre_de_requete(titre: &str) -> Option<String> {
    let mut courant = titre.trim().to_string();
    // Plusieurs suffixes peuvent s'empiler : `Album (Remastered) (Deluxe)`.
    // Borné pour qu'aucune entrée tordue ne fasse boucler.
    for _ in 0..4 {
        match retire_un_suffixe(&courant) {
            Some(plus_court) => courant = plus_court,
            None => break,
        }
    }
    let courant = courant.trim();
    if courant.is_empty() || courant == titre.trim() {
        return None;
    }
    Some(courant.to_string())
}

/// Every plausible release for an album, for the user to choose from.
///
/// The query is deliberately loose — only title and artist. Constraining on the
/// track count would hide the very editions someone opens this list for: a
/// 15-track deluxe never comes back from a `tracks:14` search. The count is
/// used to *rank* instead, and is reported per candidate so the difference is
/// visible.
pub async fn lookup_release_candidates(
    title: &str,
    artist: &str,
    track_hint: Option<u32>,
    limit: usize,
) -> RechercheDePressages {
    recherche_de_pressages(
        title,
        artist,
        track_hint,
        limit,
        |requete, fetch| async move {
            mb_get(
                "release",
                &[
                    ("query", requete),
                    ("limit", fetch.to_string()),
                    ("fmt", "json".to_string()),
                ],
            )
            .await
        },
    )
    .await
}

/// Ce qu'une recherche de pressages a donné — **refus du service compris**
/// (#4991).
///
/// Une `Vec<MBReleaseMatch>` nue ne pouvait pas porter cette différence, et
/// c'est tout le défaut : `candidats` vide avec `refus: None` veut dire
/// « MusicBrainz a répondu, il n'a pas ce pressage » ; `candidats` vide avec
/// `refus: Some(_)` veut dire « MusicBrainz n'a pas répondu, on ne sait rien de
/// cet album ».
#[derive(Debug, Clone, Default)]
pub struct RechercheDePressages {
    /// Les pressages plausibles, du meilleur au moins bon. Toujours vide sur
    /// refus.
    pub candidats: Vec<MBReleaseMatch>,
    /// `Some` quand MusicBrainz n'a pas répondu. `None` veut dire qu'il a
    /// répondu — **y compris quand il a répondu qu'il n'avait rien**.
    pub refus: Option<RefusMusicBrainz>,
}

impl RechercheDePressages {
    /// MusicBrainz a répondu. La liste peut être vide : c'est alors une
    /// ABSENCE de pressage, donc un résultat.
    pub fn repondue(candidats: Vec<MBReleaseMatch>) -> Self {
        Self {
            candidats,
            refus: None,
        }
    }

    /// MusicBrainz n'a pas répondu. Aucune conclusion possible sur l'album.
    pub fn refusee(refus: RefusMusicBrainz) -> Self {
        Self {
            candidats: Vec::new(),
            refus: Some(refus),
        }
    }

    /// 🔴 Le prédicat du disjoncteur de `identify-all` : c'est **ça** qu'une
    /// passe de lot doit compter, et rien d'autre.
    pub fn service_refuse(&self) -> bool {
        self.refus.is_some()
    }

    /// Le pressage retenu, s'il y en a un.
    pub fn meilleur(self) -> Option<MBReleaseMatch> {
        self.candidats.into_iter().next()
    }
}

/// Le corps de [`lookup_release_candidates`], **sans le transport**.
///
/// La couture existe pour que la distinction refus / absence (#4991) soit
/// prouvable sans réseau : les témoins passent un `interroger` qui rend au
/// choix un `503` ou une réponse vide, et vérifient que les deux ne se lisent
/// pas pareil. Le transport réel, lui, n'a qu'un seul appelant.
async fn recherche_de_pressages<F, Fut>(
    title: &str,
    artist: &str,
    track_hint: Option<u32>,
    limit: usize,
    mut interroger: F,
) -> RechercheDePressages
where
    F: FnMut(String, usize) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    if title.trim().is_empty() {
        // Rien n'a été demandé à MusicBrainz, et ce n'est pas sa faute.
        return RechercheDePressages::repondue(Vec::new());
    }

    // Ask for more than we show: the plausibility filter drops some, and
    // MusicBrainz mixes in loosely-related releases.
    let fetch = (limit * 3).clamp(10, 100);

    // La requête Lucene. `interroge` porte le titre ENVOYÉ à MusicBrainz ; le
    // tri de plausibilité, lui, juge toujours contre `title`, celui de la
    // bibliothèque.
    let requete = |interroge: &str| -> String {
        let mut query_parts = vec![format!("release:\"{interroge}\"")];
        if !artist.trim().is_empty() {
            query_parts.push(format!("artist:\"{artist}\""));
        }
        query_parts.join(" AND ")
    };

    let mut candidates = match interroger(requete(title), fetch).await {
        Ok(data) => rank_candidates(parse_search_results(&data, title, artist), track_hint),
        // 🔴 Refus au premier essai : on s'arrête là et on le DIT. Rejouer le
        //    titre nettoyé contre un service qui vient de refuser coûterait
        //    1,1 s pour le même refus, et rendrait surtout une liste vide
        //    indiscernable d'une absence de pressage.
        Err(refus) => {
            debug!(title = title, refus = %refus, "mb_release_candidates_refus");
            return RechercheDePressages::refusee(refus);
        }
    };

    // Second essai, et seulement sur échec : le titre débarrassé de son suffixe
    // de pressage. Mesuré à +15,4 points sur le .18 (#4805). Le coût — une
    // requête de 1,1 s — n'est payé que par le tiers d'albums qui a échoué, et
    // pas du tout quand il n'y a rien à retirer.
    let second_essai = if candidates.is_empty() {
        titre_de_requete(title)
    } else {
        None
    };
    if let Some(nettoye) = second_essai {
        debug!(
            title = title,
            retry = %nettoye,
            "mb_release_candidates_retry_titre_nettoye"
        );
        rate_limit_delay().await;
        candidates = match interroger(requete(&nettoye), fetch).await {
            Ok(data) => rank_candidates(parse_search_results(&data, title, artist), track_hint),
            Err(refus) => {
                debug!(title = title, refus = %refus, "mb_release_candidates_refus");
                return RechercheDePressages::refusee(refus);
            }
        };
    }

    candidates.truncate(limit);
    debug!(
        count = candidates.len(),
        title = title,
        "mb_release_candidates_found"
    );
    RechercheDePressages::repondue(candidates)
}

/// Fetch a chosen release with its track listing.
pub async fn lookup_release_detail(release_id: &str) -> Option<MBReleaseDetail> {
    if release_id.trim().is_empty() {
        return None;
    }
    let data = mb_get(
        &format!("release/{release_id}"),
        &[
            ("inc", "recordings+artist-credits+labels".to_string()),
            ("fmt", "json".to_string()),
        ],
    )
    .await
    .ok()?;
    parse_release_detail(&data)
}

/// Le TYPE DE SORTIE d'un groupe de sortie MusicBrainz (#4767).
///
/// Une seule requête, sur l'identifiant que la base porte déjà
/// (`albums.musicbrainz_release_group_id`) : pas de recherche par
/// titre+artiste, qui ramènerait un groupe voisin et donc un type FAUX.
///
/// `None` couvre les trois cas qui se valent pour l'appelant — identifiant
/// vide, MusicBrainz muet ou en erreur, groupe de sortie sans type — et veut
/// dire INCONNU. L'appelant laisse alors la colonne nulle : c'est l'état
/// normal, la couverture MBID mesurée étant de 0,9 % sur le .18.
///
/// Le décodage lui-même vit dans [`super::release_type`], pour être testable
/// sans réseau.
pub async fn lookup_release_group_type(
    release_group_id: &str,
) -> Option<super::release_type::TypeDeSortie> {
    if release_group_id.trim().is_empty() {
        return None;
    }
    let data = mb_get(
        &format!("release-group/{release_group_id}"),
        &[("fmt", "json".to_string())],
    )
    .await
    .ok()?;
    super::release_type::depuis_groupe_musicbrainz(&data)
}

/// Clé du créneau MusicBrainz dans le limiteur partagé
/// [`crate::http::fetch::MUSICBRAINZ`]. C'est la MÊME que celle des pochettes
/// et images d'artistes (`library::artwork`) : une clé par SERVICE, pas par
/// passe — MusicBrainz plafonne à une requête par seconde PAR IP, et deux clés
/// distinctes laisseraient deux flux parallèles doubler ce débit (503).
pub const CLE_LIMITEUR_MUSICBRAINZ: &str = "mb";

/// Attend le prochain créneau MusicBrainz.
///
/// 🔴 Ce n'est plus un `sleep` local (#4767). Un `sleep` n'espace que les
/// requêtes d'UNE boucle : la passe des types de sortie, la ré-identification
/// et la passe des crédits tournant en même temps que celle des pochettes
/// frappaient MusicBrainz plusieurs fois dans la même seconde. Toutes passent
/// désormais par le limiteur PARTAGÉ du dépôt, sous la même clé que les
/// pochettes : les créneaux sont réservés un par un, une seconde d'écart,
/// quel que soit le nombre de passes.
pub async fn rate_limit_delay() {
    crate::http::fetch::MUSICBRAINZ
        .acquire(CLE_LIMITEUR_MUSICBRAINZ)
        .await;
}

/// Issue d'une lecture de release pour la passe des crédits (#4767).
///
/// Trois cas, parce que l'appelant ne fait pas la même chose : une réponse
/// se lit ; un identifiant INCONNU de MusicBrainz (404, 400) ne reviendra pas
/// à la prochaine passe et se marque traité ; une panne (503, réseau) se
/// retente plus tard.
#[derive(Debug)]
pub enum LectureRelease {
    Lue(Value),
    Inconnue,
    Panne(String),
}

/// Les relations d'une release demandées en UNE requête : pistes,
/// artistes crédités, relations d'enregistrement (musiciens, chant,
/// production), relations d'œuvre (compositeur, parolier) et relations
/// d'artistes au niveau de la release.
pub const INC_CREDITS_RELEASE: &str =
    "recordings+artist-credits+recording-level-rels+work-rels+work-level-rels+artist-rels";

/// Lit une release avec toutes ses relations de crédits (#4767). N'attend PAS
/// le créneau : l'appelant appelle [`rate_limit_delay`] juste avant.
pub async fn lookup_release_credits(release_id: &str) -> LectureRelease {
    lire_release(release_id, INC_CREDITS_RELEASE, 30).await
}

/// Lit les SEULS labels d'une release connue par son MBID (#4836) : la passe
/// « labels seulement » du pilote d'identification. Une requête, `inc=labels`,
/// sans la liste des pistes qu'elle n'utilise pas. N'attend PAS le créneau :
/// l'appelant appelle [`rate_limit_delay`] juste avant. Le label se tire de la
/// réponse par [`labels_de_release`].
pub async fn lookup_release_labels(release_id: &str) -> LectureRelease {
    lire_release(release_id, "labels", 15).await
}

/// Le label et le numéro de catalogue d'une réponse `/release/{id}` (#4836),
/// selon la règle de [`choisir_label`].
pub fn labels_de_release(data: &Value) -> (Option<String>, Option<String>) {
    choisir_label(data.get("label-info"))
}

/// Une lecture `/release/{id}` qui distingue la panne de l'inconnu.
async fn lire_release(release_id: &str, inc: &str, delai_s: u64) -> LectureRelease {
    let id = release_id.trim();
    if id.is_empty() {
        return LectureRelease::Inconnue;
    }
    let client = crate::http::client::shared();
    let resp = match client
        .get(format!("{}/release/{id}", base_musicbrainz()))
        .query(&[("inc", inc), ("fmt", "json")])
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(delai_s))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return LectureRelease::Panne(e.to_string()),
    };
    let statut = resp.status();
    if statut == reqwest::StatusCode::NOT_FOUND || statut == reqwest::StatusCode::BAD_REQUEST {
        return LectureRelease::Inconnue;
    }
    if !statut.is_success() {
        return LectureRelease::Panne(format!("HTTP {statut}"));
    }
    match resp.json::<Value>().await {
        Ok(v) => LectureRelease::Lue(v),
        Err(e) => LectureRelease::Panne(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_text() {
        assert_eq!(normalize("Kind of Blue"), "kind of blue");
        assert_eq!(normalize("Hello, World!"), "hello world");
        assert_eq!(normalize("  spaces  "), "spaces");
    }

    #[test]
    fn mb_release_match_serde() {
        let m = MBReleaseMatch {
            release_id: "abc-123".into(),
            release_group_id: Some("def-456".into()),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            score: 95,
            ..Default::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MBReleaseMatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.score, 95);
        assert_eq!(back.release_id, "abc-123");
    }

    #[test]
    fn mb_release_match_deserializes_without_edition_fields() {
        // Payload stored before the edition details existed.
        let old =
            r#"{"release_id":"a","release_group_id":null,"title":"T","artist":"A","score":90}"#;
        let back: MBReleaseMatch = serde_json::from_str(old).unwrap();
        assert_eq!(back.score, 90);
        assert!(back.label.is_none());
        assert!(back.track_count.is_none());
    }

    #[test]
    fn normalize_empty() {
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn normalize_unicode() {
        assert_eq!(normalize("Café Résumé"), "café résumé");
    }

    // -- Search parsing --

    /// Shape of a real `/release?query=` hit, trimmed to the fields we read.
    fn search_payload() -> Value {
        json!({
            "releases": [
                {
                    "id": "rel-standard",
                    "score": 100,
                    "title": "Absolution",
                    "status": "Official",
                    "date": "2003-09-29",
                    "country": "GB",
                    "track-count": 14,
                    "disambiguation": "",
                    "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                    "release-group": { "id": "rg-1" },
                    "label-info": [
                        { "catalog-number": "TAS 0002", "label": { "name": "Taste Media" } }
                    ],
                    "media": [{ "format": "CD", "track-count": 14 }]
                },
                {
                    "id": "rel-deluxe",
                    "score": 100,
                    "title": "Absolution",
                    "status": "Official",
                    "date": "2004-03-23",
                    "country": "US",
                    "track-count": 15,
                    "disambiguation": "US edition",
                    "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                    "release-group": { "id": "rg-1" },
                    "label-info": [{ "label": { "name": "Warner Bros." } }],
                    "media": [{ "format": "CD", "track-count": 15 }]
                },
                {
                    "id": "rel-other-album",
                    "score": 72,
                    "title": "Origin of Symmetry",
                    "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                    "media": [{ "format": "CD", "track-count": 12 }]
                }
            ]
        })
    }

    #[test]
    fn parses_edition_details() {
        let out = parse_search_results(&search_payload(), "Absolution", "Muse");
        assert_eq!(out.len(), 2, "the unrelated album must be filtered out");

        let std = &out[0];
        assert_eq!(std.release_id, "rel-standard");
        assert_eq!(std.year, Some(2003));
        assert_eq!(std.date.as_deref(), Some("2003-09-29"));
        assert_eq!(std.country.as_deref(), Some("GB"));
        assert_eq!(std.label.as_deref(), Some("Taste Media"));
        assert_eq!(std.catalog_number.as_deref(), Some("TAS 0002"));
        assert_eq!(std.track_count, Some(14));
        assert_eq!(std.disc_count, Some(1));
        assert_eq!(std.media_format.as_deref(), Some("CD"));
        assert_eq!(std.release_group_id.as_deref(), Some("rg-1"));
        // An empty disambiguation must not become Some("").
        assert!(std.disambiguation.is_none());

        assert_eq!(out[1].disambiguation.as_deref(), Some("US edition"));
    }

    #[test]
    fn filters_out_unrelated_titles() {
        let out = parse_search_results(&search_payload(), "Absolution", "Muse");
        assert!(!out.iter().any(|r| r.release_id == "rel-other-album"));
    }

    #[test]
    fn filters_out_other_artists() {
        let data = json!({
            "releases": [{
                "id": "x", "score": 100, "title": "Absolution",
                "artist-credit": [{ "name": "Some Tribute Band", "joinphrase": "" }]
            }]
        });
        assert!(parse_search_results(&data, "Absolution", "Muse").is_empty());
    }

    #[test]
    fn parse_search_handles_empty_and_malformed() {
        assert!(parse_search_results(&json!({}), "T", "A").is_empty());
        assert!(parse_search_results(&json!({"releases": []}), "T", "A").is_empty());
        // A hit with no id is unusable.
        let no_id = json!({"releases": [{"score": 100, "title": "T"}]});
        assert!(parse_search_results(&no_id, "T", "").is_empty());
    }

    #[test]
    fn sums_track_count_across_discs_when_absent() {
        let data = json!({
            "releases": [{
                "id": "box", "score": 90, "title": "Absolution",
                "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                "media": [
                    { "format": "CD", "track-count": 14 },
                    { "format": "DVD", "track-count": 5 }
                ]
            }]
        });
        let out = parse_search_results(&data, "Absolution", "Muse");
        assert_eq!(out[0].track_count, Some(19));
        assert_eq!(out[0].disc_count, Some(2));
        assert_eq!(out[0].media_format.as_deref(), Some("CD + DVD"));
    }

    #[test]
    fn joins_collaboration_artists_with_their_join_phrase() {
        let data = json!({
            "releases": [{
                "id": "c", "score": 100, "title": "Under Pressure",
                "artist-credit": [
                    { "name": "Queen", "joinphrase": " & " },
                    { "name": "David Bowie", "joinphrase": "" }
                ]
            }]
        });
        let out = parse_search_results(&data, "Under Pressure", "Queen");
        assert_eq!(out[0].artist, "Queen & David Bowie");
    }

    #[test]
    fn reads_country_from_release_events() {
        let data = json!({
            "releases": [{
                "id": "e", "score": 100, "title": "Absolution",
                "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                "release-events": [{ "area": { "iso-3166-1-codes": "JP" } }]
            }]
        });
        let out = parse_search_results(&data, "Absolution", "Muse");
        assert_eq!(out[0].country.as_deref(), Some("JP"));
    }

    // -- Ranking --

    fn candidate(id: &str, score: i32, tracks: Option<u32>) -> MBReleaseMatch {
        MBReleaseMatch {
            release_id: id.into(),
            score,
            track_count: tracks,
            ..Default::default()
        }
    }

    #[test]
    fn ranks_by_score_first() {
        let out = rank_candidates(
            vec![
                candidate("low", 70, Some(14)),
                candidate("high", 100, Some(99)),
            ],
            Some(14),
        );
        assert_eq!(out[0].release_id, "high");
    }

    #[test]
    fn breaks_score_ties_on_track_count() {
        // Both scored 100; the one matching the 15 files on disk comes first.
        let out = rank_candidates(
            vec![
                candidate("std", 100, Some(14)),
                candidate("deluxe", 100, Some(15)),
            ],
            Some(15),
        );
        assert_eq!(out[0].release_id, "deluxe");
        assert_eq!(out[1].release_id, "std");
    }

    #[test]
    fn ranks_unknown_track_count_after_a_near_miss() {
        let out = rank_candidates(
            vec![
                candidate("unknown", 100, None),
                candidate("off-by-one", 100, Some(14)),
            ],
            Some(15),
        );
        assert_eq!(out[0].release_id, "off-by-one");
    }

    #[test]
    fn ranking_without_a_hint_keeps_score_order() {
        let out = rank_candidates(
            vec![candidate("a", 80, Some(14)), candidate("b", 95, None)],
            None,
        );
        assert_eq!(out[0].release_id, "b");
    }

    // -- Release detail parsing --

    fn detail_payload() -> Value {
        json!({
            "id": "rel-standard",
            "title": "Absolution",
            "date": "2003-09-29",
            "country": "GB",
            "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
            "label-info": [{ "catalog-number": "TAS 0002", "label": { "name": "Taste Media" } }],
            "media": [
                {
                    "position": 1,
                    "format": "CD",
                    "tracks": [
                        { "id": "t1", "position": 1, "number": "1", "title": "Intro", "length": 22000,
                          "recording": { "id": "rec-1", "title": "Intro" } },
                        { "id": "t2", "position": 2, "number": "2", "title": "Apocalypse Please", "length": 197000,
                          "recording": { "id": "rec-2", "title": "Apocalypse Please" } }
                    ]
                },
                {
                    "position": 2,
                    "format": "DVD",
                    "tracks": [
                        { "id": "t3", "position": 1, "number": "1", "title": "Live at Glastonbury",
                          "recording": { "id": "rec-3" } }
                    ]
                }
            ]
        })
    }

    #[test]
    fn parses_track_listing_across_discs() {
        let d = parse_release_detail(&detail_payload()).unwrap();
        assert_eq!(d.release_id, "rel-standard");
        assert_eq!(d.artist, "Muse");
        assert_eq!(d.year, Some(2003));
        assert_eq!(d.label.as_deref(), Some("Taste Media"));
        assert_eq!(d.disc_count, 2);
        assert_eq!(d.tracks.len(), 3);

        assert_eq!(d.tracks[0].title, "Intro");
        assert_eq!(d.tracks[0].disc, 1);
        assert_eq!(d.tracks[0].position, 1);
        assert_eq!(d.tracks[0].length_ms, Some(22000));
        assert_eq!(d.tracks[0].recording_id.as_deref(), Some("rec-1"));

        assert_eq!(d.tracks[2].disc, 2);
        assert_eq!(d.tracks[2].title, "Live at Glastonbury");
    }

    #[test]
    fn falls_back_to_recording_title_and_array_order() {
        let data = json!({
            "id": "r",
            "title": "T",
            "media": [{
                "tracks": [
                    { "recording": { "title": "From the recording" } },
                    { "title": "Own title" }
                ]
            }]
        });
        let d = parse_release_detail(&data).unwrap();
        assert_eq!(d.tracks[0].title, "From the recording");
        assert_eq!(d.tracks[0].position, 1);
        assert_eq!(
            d.tracks[0].disc, 1,
            "single medium with no position is disc 1"
        );
        assert_eq!(d.tracks[1].position, 2);
    }

    #[test]
    fn keeps_printed_track_numbers() {
        let data = json!({
            "id": "r", "title": "T",
            "media": [{ "position": 1, "format": "12\" Vinyl", "tracks": [
                { "position": 1, "number": "A1", "title": "Side opener" }
            ]}]
        });
        let d = parse_release_detail(&data).unwrap();
        assert_eq!(d.tracks[0].number.as_deref(), Some("A1"));
    }

    #[test]
    fn detail_without_media_is_still_usable() {
        let data = json!({ "id": "r", "title": "T" });
        let d = parse_release_detail(&data).unwrap();
        assert_eq!(d.disc_count, 0);
        assert!(d.tracks.is_empty());
    }

    #[test]
    fn detail_requires_an_id() {
        assert!(parse_release_detail(&json!({ "title": "T" })).is_none());
    }

    #[test]
    fn skips_untitled_tracks() {
        let data = json!({
            "id": "r", "title": "T",
            "media": [{ "tracks": [{ "position": 1 }, { "position": 2, "title": "Real" }] }]
        });
        let d = parse_release_detail(&data).unwrap();
        assert_eq!(d.tracks.len(), 1);
        assert_eq!(d.tracks[0].title, "Real");
    }

    /// Les SIX titres qui, le 23/09/2026, ne rendaient rien tels quels sur le
    /// .18 et ont rendu un pressage une fois nettoyés (#4805). Ce ne sont pas
    /// des exemples inventés : c'est l'échantillon mesuré.
    #[test]
    fn titre_de_requete_retire_les_suffixes_qui_font_echouer_la_recherche() {
        for (brut, attendu) in [
            ("Somethin' Else (192kHz/24bit)", "Somethin' Else"),
            ("My Favorite Things (96kHz/24bit)", "My Favorite Things"),
            ("Bird And Diz (Remastered)", "Bird And Diz"),
            (
                "West Kirby County Primary (Deluxe)",
                "West Kirby County Primary",
            ),
            (
                "Tales Of Mystery And Imagination (Original 1976 Version)",
                "Tales Of Mystery And Imagination",
            ),
            (
                "Smash the System: Singles and More, Disc 1",
                "Smash the System: Singles and More",
            ),
        ] {
            assert_eq!(
                titre_de_requete(brut).as_deref(),
                Some(attendu),
                "le suffixe de « {brut} » n'a pas ete retire"
            );
        }
    }

    /// `, Disc N` et ` - CD N` en fin de titre : le decoupage par disque est une
    /// propriete de notre arborescence, MusicBrainz decrit le coffret entier.
    #[test]
    fn titre_de_requete_retire_le_numero_de_disque_final() {
        assert_eq!(
            titre_de_requete("Radio Nova - La boite Jaune - 1992, Disc 12").as_deref(),
            Some("Radio Nova - La boite Jaune - 1992")
        );
        assert_eq!(
            titre_de_requete("Anthology - CD 2").as_deref(),
            Some("Anthology")
        );
    }

    /// La contrepartie : ne rien retirer quand il n'y a rien a retirer. `None`
    /// est ce qui epargne a l'appelant une seconde requete de 1,1 s identique.
    #[test]
    fn titre_de_requete_ne_touche_pas_un_titre_propre() {
        for propre in [
            "Cousin Zaka, Vol. 1",
            "Tango macondo",
            "The Best of Miles Davis & John Coltrane: 1955-1961",
            "Back To Mine - Talvin Singh",
            "Kind of Blue",
        ] {
            assert_eq!(
                titre_de_requete(propre),
                None,
                "« {propre} » a ete ampute alors qu'il etait propre"
            );
        }
    }

    /// Un suffixe ne doit jamais devorer le titre entier : mieux vaut ne rien
    /// retirer que d'interroger MusicBrainz avec une chaine vide.
    #[test]
    fn titre_de_requete_ne_vide_jamais_le_titre() {
        assert_eq!(titre_de_requete("(Remastered)"), None);
        assert_eq!(titre_de_requete("[Deluxe Edition]"), None);
        assert_eq!(titre_de_requete(", Disc 1"), None);
        assert_eq!(titre_de_requete(""), None);
    }

    /// Plusieurs suffixes s'empilent en pratique.
    #[test]
    fn titre_de_requete_depile_les_suffixes_empiles() {
        assert_eq!(
            titre_de_requete("Fireball (25th Anniversary Edition) (Remastered)").as_deref(),
            Some("Fireball")
        );
    }

    /// Le filtre `plausible` juge contre le titre D'ORIGINE, pas contre le titre
    /// nettoye — c'est ce qui rend le second essai sur : le pressage court
    /// revenu de MusicBrainz reste inclus dans le titre long de la bibliotheque.
    #[test]
    fn le_pressage_court_reste_plausible_face_au_titre_long() {
        assert!(plausible(
            "Somethin' Else",
            "Cannonball Adderley",
            "Somethin' Else (192kHz/24bit)",
            "Cannonball Adderley"
        ));
        assert!(plausible(
            "Smash the System: Singles and More",
            "Saint Etienne",
            "Smash the System: Singles and More, Disc 1",
            "Saint Etienne"
        ));
    }

    // -- Choix du label (#4836) --

    #[test]
    fn le_label_saute_le_pseudo_label_sans_label_et_garde_son_catalogue() {
        let infos = json!([
            { "catalog-number": "none-1", "label": { "id": MBID_SANS_LABEL, "name": "[no label]" } },
            { "catalog-number": "", "label": { "name": "  " } },
            { "catalog-number": "CL 1355", "label": { "id": "x", "name": "Columbia" } },
            { "catalog-number": "BN 1", "label": { "name": "Blue Note" } },
        ]);
        assert_eq!(
            choisir_label(Some(&infos)),
            (Some("Columbia".to_string()), Some("CL 1355".to_string()))
        );
    }

    #[test]
    fn le_pseudo_label_est_reconnu_par_son_nom_sans_son_mbid() {
        let infos = json!([{ "label": { "name": "[No Label]" } }]);
        assert_eq!(choisir_label(Some(&infos)), (None, None));
    }

    #[test]
    fn un_catalogue_none_n_est_pas_un_numero() {
        let infos = json!([
            { "catalog-number": "[none]", "label": { "name": "ECM" } },
            { "catalog-number": "ECM 1064", "label": null },
        ]);
        assert_eq!(
            choisir_label(Some(&infos)),
            (Some("ECM".to_string()), Some("ECM 1064".to_string()))
        );
    }

    #[test]
    fn sans_label_info_rien() {
        assert_eq!(choisir_label(None), (None, None));
        assert_eq!(choisir_label(Some(&json!([]))), (None, None));
        assert_eq!(labels_de_release(&json!({"id": "r"})), (None, None));
    }

    // -- 🔴 #4991 : un refus de MusicBrainz n'est pas une absence de pressage --
    //
    // Hermétique : aucun appel réseau. La couture `recherche_de_pressages`
    // reçoit un `interroger` de témoin, qui rend au choix un refus ou une
    // réponse. C'est la SEULE façon de prouver que les deux cas ne se lisent
    // plus pareil — le défaut d'origine était précisément qu'ils étaient
    // indiscernables à la sortie de la fonction.

    /// Une réponse de recherche portant un pressage plausible.
    fn reponse_avec_un_pressage(titre: &str, artiste: &str) -> Value {
        json!({
            "releases": [{
                "id": "11111111-2222-3333-4444-555555555555",
                "title": titre,
                "score": 100,
                "artist-credit": [{ "name": artiste }],
            }]
        })
    }

    /// Le cas mesuré sur le .18 le 25/09/2026, côté service en panne : un `503`
    /// ne doit **pas** ressortir comme « aucun pressage ». Sans ce témoin, le
    /// pilote de lot ne peut pas faire la différence, et c'est tout le défaut.
    #[tokio::test(start_paused = true)]
    async fn un_refus_de_musicbrainz_se_lit_comme_un_refus() {
        let recherche = recherche_de_pressages(
            "Goldberg-Variationen",
            "Laszlo Borbely",
            Some(64),
            5,
            |_requete, _fetch| async { Err(RefusMusicBrainz::Statut(503)) },
        )
        .await;

        assert!(
            recherche.service_refuse(),
            "un 503 de MusicBrainz doit se lire comme un REFUS, pas comme une \
             absence de pressage : refus = {:?}",
            recherche.refus
        );
        assert_eq!(recherche.refus, Some(RefusMusicBrainz::Statut(503)));
        assert!(recherche.candidats.is_empty());
    }

    /// L'autre moitié, et la plus importante : MusicBrainz répond, il n'a rien.
    /// C'est un RÉSULTAT. Douze albums de musique classique contigus par
    /// identifiant passent tous par ici — et arrêtaient la passe.
    #[tokio::test(start_paused = true)]
    async fn une_reponse_sans_pressage_nest_pas_un_refus() {
        let recherche = recherche_de_pressages(
            "Goldberg-Variationen",
            "Laszlo Borbely",
            Some(64),
            5,
            |_requete, _fetch| async { Ok(json!({ "releases": [] })) },
        )
        .await;

        assert!(
            !recherche.service_refuse(),
            "MusicBrainz a RÉPONDU qu'il n'avait pas ce pressage : le prendre \
             pour une panne est le défaut #4991 — refus = {:?}",
            recherche.refus
        );
        assert!(recherche.candidats.is_empty());
    }

    /// Une recherche qui aboutit ne porte évidemment aucun refus.
    #[tokio::test(start_paused = true)]
    async fn une_recherche_aboutie_ne_porte_aucun_refus() {
        let recherche = recherche_de_pressages(
            "Kind of Blue",
            "Miles Davis",
            Some(5),
            5,
            |_requete, _fetch| async {
                Ok(reponse_avec_un_pressage("Kind of Blue", "Miles Davis"))
            },
        )
        .await;

        assert!(!recherche.service_refuse());
        assert_eq!(recherche.candidats.len(), 1);
        assert_eq!(
            recherche.meilleur().map(|m| m.release_id),
            Some("11111111-2222-3333-4444-555555555555".to_string())
        );
    }

    /// Un refus au premier essai n'arme pas le second : rejouer le titre
    /// nettoyé contre un service qui vient de refuser coûte 1,1 s pour le même
    /// refus. Le titre porte ici un suffixe, donc un second essai serait
    /// possible — c'est bien le refus qui l'empêche.
    #[tokio::test(start_paused = true)]
    async fn un_refus_au_premier_essai_narme_pas_le_second() {
        assert!(
            titre_de_requete("Somethin' Else (192kHz/24bit)").is_some(),
            "le titre du témoin doit avoir un suffixe à retirer, sinon le \
             témoin ne prouve rien"
        );
        let appels = std::cell::Cell::new(0usize);

        let recherche = recherche_de_pressages(
            "Somethin' Else (192kHz/24bit)",
            "Cannonball Adderley",
            Some(5),
            5,
            |_requete, _fetch| {
                appels.set(appels.get() + 1);
                async { Err(RefusMusicBrainz::Transport) }
            },
        )
        .await;

        assert!(recherche.service_refuse());
        assert_eq!(
            appels.get(),
            1,
            "le second essai a été lancé alors que MusicBrainz venait de refuser"
        );
    }

    /// Et à l'inverse : réponse vide au premier essai, le second essai part
    /// bien — le correctif ne doit pas avoir emporté le rattrapage de #4805.
    #[tokio::test(start_paused = true)]
    async fn une_reponse_vide_arme_toujours_le_second_essai() {
        let appels = std::cell::Cell::new(0usize);

        let recherche = recherche_de_pressages(
            "Somethin' Else (192kHz/24bit)",
            "Cannonball Adderley",
            Some(5),
            5,
            |_requete, _fetch| {
                appels.set(appels.get() + 1);
                let premier = appels.get() == 1;
                async move {
                    if premier {
                        Ok(json!({ "releases": [] }))
                    } else {
                        Ok(reponse_avec_un_pressage(
                            "Somethin' Else",
                            "Cannonball Adderley",
                        ))
                    }
                }
            },
        )
        .await;

        assert_eq!(appels.get(), 2, "le second essai n'a pas eu lieu");
        assert!(!recherche.service_refuse());
        assert_eq!(recherche.candidats.len(), 1);
    }

    /// Un refus au SECOND essai est un refus tout court : la passe ne doit pas
    /// conclure « rien trouvé » parce que le premier essai, lui, avait répondu.
    #[tokio::test(start_paused = true)]
    async fn un_refus_au_second_essai_reste_un_refus() {
        let appels = std::cell::Cell::new(0usize);

        let recherche = recherche_de_pressages(
            "Somethin' Else (192kHz/24bit)",
            "Cannonball Adderley",
            Some(5),
            5,
            |_requete, _fetch| {
                appels.set(appels.get() + 1);
                let premier = appels.get() == 1;
                async move {
                    if premier {
                        Ok(json!({ "releases": [] }))
                    } else {
                        Err(RefusMusicBrainz::Statut(503))
                    }
                }
            },
        )
        .await;

        assert_eq!(appels.get(), 2);
        assert!(
            recherche.service_refuse(),
            "le refus du second essai a été avalé : {:?}",
            recherche.refus
        );
    }

    /// Un titre vide n'interroge personne — et n'accuse donc personne.
    #[tokio::test(start_paused = true)]
    async fn un_titre_vide_nest_pas_un_refus_de_musicbrainz() {
        let appels = std::cell::Cell::new(0usize);
        let recherche = recherche_de_pressages("   ", "Miles Davis", None, 5, |_r, _f| {
            appels.set(appels.get() + 1);
            async { Ok(Value::Null) }
        })
        .await;

        assert_eq!(
            appels.get(),
            0,
            "aucune requête ne doit partir pour un titre vide"
        );
        assert!(!recherche.service_refuse());
        assert!(recherche.candidats.is_empty());
    }
}
