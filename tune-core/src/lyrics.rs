//! Synchronized lyrics via LRCLIB.
//!
//! - Parses LRC-format timestamped lyrics into `Vec<LyricLine>` (delegates
//!   to the canonical parser in [`crate::metadata::lyrics`]).
//! - Fetches from <https://lrclib.net/api/get> (no API key required).
//! - Caches results in the `lyrics_cache` DB table (SQLite and Postgres),
//!   including negative results which are retried after 14 days.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Negative cache entries (no lyrics found) are retried after this delay.
pub const NEGATIVE_CACHE_TTL_DAYS: i64 = 14;

/// Process-wide count of LRCLIB replies that told us to slow down (HTTP 429
/// « Too Many Requests » or 503). Incremented by [`fetch_lrclib_raw`], never
/// reset — a caller snapshots it before a run and compares afterwards.
///
/// LRCLIB is a free community service with no API key: a batch pass that kept
/// hammering it after a 429 would get the whole instance banned, and that would
/// also kill the on-demand lookup done while listening. The background pass
/// (`crate::library::lyrics_pass`) watches this counter and stops on the first
/// hit. Same idiom as `ARTWORK_RATE_LIMIT_HITS` in `library::artwork`.
pub static LRCLIB_RATE_LIMIT_HITS: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single time-stamped lyric line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LyricLine {
    /// Milliseconds from track start.
    pub time_ms: i64,
    /// The lyric text for this line.
    pub text: String,
}

/// Full lyrics payload returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lyrics {
    /// Whether time-synced lyrics are available.
    pub synced: bool,
    /// Parsed time-stamped lines (empty when `synced` is false).
    pub lines: Vec<LyricLine>,
    /// Plain (unsynced) lyrics text.
    pub plain_text: Option<String>,
    /// Attribution source.
    pub source: String,
}

// ---------------------------------------------------------------------------
// LRCLIB API response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibResponse {
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

/// Raw LRCLIB result: original `syncedLyrics` (LRC text) and `plainLyrics`.
#[derive(Debug, Clone, Default)]
pub struct LrclibRaw {
    pub synced_lyrics: Option<String>,
    pub plain_lyrics: Option<String>,
}

impl LrclibRaw {
    pub fn is_empty(&self) -> bool {
        self.synced_lyrics
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            && self
                .plain_lyrics
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
    }
}

/// Position de lecture à utiliser pour choisir la ligne de paroles active,
/// une fois appliqué le décalage `zones.lyrics_offset_ms` de la zone.
///
/// **Implémentation de référence unique** du réglage (#2997) : tout code qui
/// surligne une ligne de paroles pour une zone doit passer par ici plutôt que
/// de comparer les horodatages à la position brute.
///
/// `offset_ms` positif = paroles **retardées**. Le serveur apprend le titre
/// avant que l'auditeur ne l'entende (tampon de Tune, puis du renderer), donc
/// les paroles défilent en avance et il faut les retarder : on recule la
/// position d'autant, ce qui revient à avancer d'autant l'instant où chaque
/// ligne devient active. Décalage **par zone**, parce que la profondeur du
/// tampon appartient à l'appareil ; à ne pas confondre avec `sync_delay_ms`,
/// qui décale l'AUDIO pour aligner deux pièces.
///
/// Deux propriétés dont les appelants dépendent :
/// - un décalage de **zéro** rend la position inchangée, donc exactement le
///   comportement d'avant ce réglage ;
/// - le résultat ne descend jamais sous zéro — une position négative n'existe
///   pas, et laisserait le début du morceau sans ligne active.
///
/// C'est le calcul que le client web tient déjà dans `TvView.svelte`
/// (`syncPos = max(0, pos - lyricsOffsetMs)`) : cette fonction est la même
/// règle, côté serveur, pour les surfaces qui connaissent la zone.
pub fn sync_position_ms(position_ms: i64, offset_ms: i32) -> i64 {
    position_ms.saturating_sub(offset_ms as i64).max(0)
}

// ---------------------------------------------------------------------------
// LRC parser (canonical implementation lives in metadata::lyrics)
// ---------------------------------------------------------------------------

/// Parse an LRC-format string into a sorted `Vec<LyricLine>`.
///
/// Accepted format per line: `[MM:SS.xx] text` where `xx` can be 1-3
/// digits (centiseconds or milliseconds). Several timestamps per line are
/// supported and metadata tags (`[ar:..]`, `[ti:..]`…) are ignored.
pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    crate::metadata::lyrics::parse_lrc(lrc)
        .into_iter()
        .map(|l| LyricLine {
            time_ms: l.time_ms as i64,
            text: l.text,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// LRCLIB fetch
// ---------------------------------------------------------------------------

/// Vrai si ce jeton, seul, est une mention de QUALITÉ audio (format, résolution).
///
/// Le vocabulaire est volontairement **court et fermé** : c'est lui qui borne
/// le risque. Un jeton inconnu fait renoncer au nettoyage tout entier
/// ([`album_sans_mention_de_qualite`]), donc élargir cette liste élargit le
/// risque de mutiler un titre légitime — « 24 Carat Black », « 24/7 », « 1999 ».
fn jeton_de_qualite(jeton: &str) -> bool {
    let j = jeton.to_ascii_lowercase();

    // Conteneurs et familles, en toutes lettres.
    if matches!(
        j.as_str(),
        "sacd"
            | "dsd"
            | "dsd64"
            | "dsd128"
            | "dsd256"
            | "dsd512"
            | "dsf"
            | "dff"
            | "mqa"
            | "flac"
            | "wav"
            | "alac"
            | "aiff"
            | "hi-res"
            | "hires"
            | "hi-rez"
            | "16bit"
            | "16bits"
            | "24bit"
            | "24bits"
            | "32bit"
            | "32bits"
    ) {
        return true;
    }

    // « 96kHz », « 44.1 kHz » réduit à un seul jeton par la découpe.
    let sans_unite = j
        .strip_suffix("khz")
        .map(|s| s.trim_end_matches(' '))
        .unwrap_or(j.as_str());
    let unite_vue = sans_unite.len() != j.len();
    if unite_vue && frequence_plausible(sans_unite) {
        return true;
    }

    // « 24/96 », « 24-192 », « 32x384 » : une profondeur ET une fréquence, les
    // deux plausibles. C'est ce couple qui distingue « 24/96 » de « 24/7 ».
    let mut morceaux = sans_unite.splitn(2, |c| c == '/' || c == '-' || c == 'x');
    match (morceaux.next(), morceaux.next()) {
        (Some(profondeur), Some(frequence)) => {
            matches!(profondeur, "16" | "24" | "32") && frequence_plausible(frequence)
        }
        _ => false,
    }
}

/// Vrai si cette chaîne est une fréquence d'échantillonnage courante, en kHz.
///
/// Fermée elle aussi, et pour la même raison : c'est elle qui refuse le « 7 »
/// de « 24/7 ».
fn frequence_plausible(f: &str) -> bool {
    matches!(
        f,
        "44" | "44.1"
            | "48"
            | "88"
            | "88.2"
            | "96"
            | "176"
            | "176.4"
            | "192"
            | "352"
            | "352.8"
            | "384"
    )
}

/// Vrai si ce jeton est une année civile plausible (1900-2099).
fn jeton_d_annee(jeton: &str) -> bool {
    jeton.len() == 4
        && jeton.bytes().all(|b| b.is_ascii_digit())
        && matches!(&jeton[..2], "19" | "20")
}

/// Découpe un segment de fin de titre en jetons.
///
/// On coupe sur les espaces et les parenthèses/crochets/virgules, **jamais**
/// sur `-`, `/` ni `.` : « 24/96 », « 24-96 » et « 44.1 » doivent rester d'un
/// seul tenant, sinon « 96 » seul deviendrait indistinguable d'un nombre
/// quelconque.
fn jetons(segment: &str) -> Vec<&str> {
    segment
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | ',' | ';'))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Classement d'un segment de fin de titre.
enum Segment {
    /// Tous les jetons sont reconnus, et au moins un est une mention de qualité.
    Qualite,
    /// Tous les jetons sont reconnus, mais ce ne sont que des années.
    AnneeSeule,
    /// Au moins un jeton n'est pas reconnu : on ne touche à rien.
    Inconnu,
}

fn classer(segment: &str) -> Segment {
    let jetons = jetons(segment);
    if jetons.is_empty() {
        return Segment::Inconnu;
    }
    let mut qualite_vue = false;
    for jeton in jetons {
        if jeton_de_qualite(jeton) {
            qualite_vue = true;
        } else if !jeton_d_annee(jeton) {
            return Segment::Inconnu;
        }
    }
    if qualite_vue {
        Segment::Qualite
    } else {
        Segment::AnneeSeule
    }
}

/// Détache le dernier segment d'un titre, sous une des trois formes reconnues,
/// et rend `(tête, segment)`.
///
/// Les trois formes, dans cet ordre : un groupe parenthésé ou crocheté final,
/// une queue après un tiret entouré d'espaces, un dernier mot.
fn detacher_le_dernier_segment(titre: &str) -> Option<(&str, &str)> {
    let t = titre.trim_end();
    if t.is_empty() {
        return None;
    }

    // 1. « … (24-96) », « … [SACD] »
    let fermante = t.chars().next_back()?;
    if let Some(ouvrante) = match fermante {
        ')' => Some('('),
        ']' => Some('['),
        _ => None,
    } {
        if let Some(i) = t.rfind(ouvrante) {
            let dedans = &t[i + ouvrante.len_utf8()..t.len() - fermante.len_utf8()];
            return Some((&t[..i], dedans));
        }
        return None;
    }

    // 2. « … - 24/96 » : le tiret DOIT être précédé d'un espace, sinon
    //    « Raconte-moi » y laisserait sa dernière syllabe.
    let mut derniere_coupe = None;
    for (i, c) in t.char_indices() {
        if matches!(c, '-' | '\u{2013}' | '\u{2014}')
            && t[..i].chars().next_back().is_some_and(char::is_whitespace)
        {
            derniere_coupe = Some((i, c));
        }
    }
    if let Some((i, c)) = derniere_coupe {
        return Some((&t[..i], t[i + c.len_utf8()..].trim_start()));
    }

    // 3. « … 24/96 » — sans tiret ni parenthèse, le testeur en signale aussi.
    let i = t.rfind(char::is_whitespace)?;
    Some((&t[..i], t[i..].trim_start()))
}

/// Retire d'un titre d'album les mentions de qualité que certains testeurs
/// ajoutent pour distinguer leurs éditions (« Innuendo - 24/96 »,
/// « Unplugged - SACD(2021) »), afin que LRCLIB puisse retrouver l'album.
///
/// **La règle est conservatrice, et le sens de l'erreur est choisi** : rater un
/// nettoyage est sans conséquence (la recherche échoue comme aujourd'hui),
/// mutiler un titre légitime en aurait une. Donc :
///
/// - on ne retire que des segments de **fin** de titre, jamais du milieu ni du
///   début — « 24 Carat Black » ressort intact ;
/// - un segment n'est retiré que si **tous** ses jetons sont reconnus ; un seul
///   mot inconnu arrête le nettoyage sur-le-champ — « The Wall (Remastered) »
///   ressort intact ;
/// - une **année seule** n'est jamais un motif de nettoyage : elle n'est
///   emportée que si une mention de qualité a été retirée à sa gauche — « 1999 »
///   et « Unplugged (2021) » ressortent intacts ;
/// - si le nettoyage vide le titre, on rend le titre d'origine.
pub fn album_sans_mention_de_qualite(album: &str) -> String {
    let origine = album.trim();
    let mut courant = origine;
    let mut qualite_vue = false;

    while let Some((tete, segment)) = detacher_le_dernier_segment(courant) {
        if tete.trim().is_empty() {
            // Le titre entier est la mention : ce n'est pas un suffixe.
            break;
        }
        match classer(segment) {
            Segment::Inconnu => break,
            Segment::Qualite => {
                qualite_vue = true;
                courant = tete;
            }
            Segment::AnneeSeule => courant = tete,
        }
    }

    if !qualite_vue {
        return origine.to_string();
    }
    let propre = courant
        .trim_end_matches(['-', '\u{2013}', '\u{2014}', ' '])
        .trim();
    if propre.is_empty() {
        origine.to_string()
    } else {
        propre.to_string()
    }
}

/// Titre d'album de REPLI pour une seconde tentative LRCLIB, ou `None`.
///
/// `None` veut dire « il n'y a rien à retenter » : pas d'album, ou un titre que
/// [`album_sans_mention_de_qualite`] laisse inchangé. C'est cette fonction qui
/// garantit que la requête supplémentaire est **bornée aux titres porteurs
/// d'une mention de qualité** et n'est jamais payée par les autres.
pub fn album_de_repli(album_name: Option<&str>) -> Option<String> {
    let album = album_name.map(str::trim).filter(|a| !a.is_empty())?;
    let propre = album_sans_mention_de_qualite(album);
    (propre != album).then_some(propre)
}

/// Fetch raw lyrics from LRCLIB for a given artist/track/album/duration.
///
/// Returns `Ok(None)` when LRCLIB has no entry (HTTP 404), `Err` on
/// network/protocol failures.
///
/// #3815 — une seconde tentative, et une seule : si le premier appel rend 404
/// **et** que le titre d'album porte une mention de qualité (« Innuendo -
/// 24/96 »), on rejoue avec le titre nettoyé. L'ordre compte : le titre tel
/// quel passe TOUJOURS en premier, donc rien de ce qui marche aujourd'hui ne
/// peut régresser. Un 429/503 sort par `?` sans rien retenter.
pub async fn fetch_lrclib_raw(
    client: &reqwest::Client,
    artist: &str,
    track_name: &str,
    album_name: Option<&str>,
    duration_secs: Option<i64>,
) -> Result<Option<LrclibRaw>, String> {
    let premier = lrclib_get(client, artist, track_name, album_name, duration_secs).await?;
    if premier.is_some() {
        return Ok(premier);
    }
    match album_de_repli(album_name) {
        Some(propre) => {
            debug!(album_nettoye = %propre, "lrclib_retry_album_nettoye");
            lrclib_get(
                client,
                artist,
                track_name,
                Some(propre.as_str()),
                duration_secs,
            )
            .await
        }
        None => Ok(None),
    }
}

/// Un appel à `/api/get`, et rien d'autre.
///
/// Short 5 s timeout: this is called from an interactive endpoint and must
/// fail fast.
async fn lrclib_get(
    client: &reqwest::Client,
    artist: &str,
    track_name: &str,
    album_name: Option<&str>,
    duration_secs: Option<i64>,
) -> Result<Option<LrclibRaw>, String> {
    let mut url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlencoding::encode(artist),
        urlencoding::encode(track_name),
    );

    if let Some(album) = album_name.filter(|a| !a.trim().is_empty()) {
        url.push_str(&format!("&album_name={}", urlencoding::encode(album)));
    }
    if let Some(dur) = duration_secs {
        url.push_str(&format!("&duration={dur}"));
    }

    debug!(url = %url, "lrclib_fetch");

    let resp = client
        .get(&url)
        .header("User-Agent", format!("Tune/{}", crate::version()))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("lrclib request failed: {e}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    if !resp.status().is_success() {
        // 429/503 = « ralentis ». On le compte pour que la passe de fond
        // s'arrête au premier signal au lieu de continuer à cogner.
        if matches!(
            resp.status(),
            reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE
        ) {
            LRCLIB_RATE_LIMIT_HITS.fetch_add(1, Ordering::Relaxed);
            warn!(status = %resp.status(), "lrclib_rate_limited");
        }
        return Err(format!("lrclib returned {}", resp.status()));
    }

    let body: LrclibResponse = resp
        .json()
        .await
        .map_err(|e| format!("lrclib parse error: {e}"))?;

    Ok(Some(LrclibRaw {
        synced_lyrics: body.synced_lyrics,
        plain_lyrics: body.plain_lyrics,
    }))
}

/// Fetch lyrics from LRCLIB for a given artist/track/duration.
///
/// `duration_secs` is the track length in seconds (integer). LRCLIB
/// uses it for disambiguation when multiple versions exist.
pub async fn fetch_from_lrclib(
    client: &reqwest::Client,
    artist: &str,
    track_name: &str,
    album_name: Option<&str>,
    duration_secs: Option<i64>,
) -> Result<Lyrics, String> {
    let raw = fetch_lrclib_raw(client, artist, track_name, album_name, duration_secs)
        .await?
        .unwrap_or_default();

    let lines = raw
        .synced_lyrics
        .as_deref()
        .map(parse_lrc)
        .unwrap_or_default();

    Ok(Lyrics {
        synced: !lines.is_empty(),
        lines,
        plain_text: raw.plain_lyrics,
        source: "lrclib".into(),
    })
}

// ---------------------------------------------------------------------------
// Cache key for metadata-only lookups (radio: title+artist, no track id)
// ---------------------------------------------------------------------------

/// Normalise un champ de métadonnée (titre ou artiste) pour la clé de cache :
/// minuscules + espaces internes réduits à un seul + trim. Deux variantes ICY
/// du même morceau (« Miles Davis » / « miles  davis ») partagent ainsi la
/// même entrée `lyrics_cache`.
pub fn normalize_meta(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Identifiant synthétique de cache pour une paire titre+artiste normalisée.
///
/// `lyrics_cache` est clé par `track_id` (PK, sans FK) ; les pistes de la
/// bibliothèque ont des ids AUTOINCREMENT strictement positifs. Les paroles
/// « radio » (pas de piste) sont donc rangées sous un id **négatif** dérivé
/// d'un FNV-1a 64 bits de `artist\u{1f}title` normalisés — aucune migration,
/// aucune collision possible avec une vraie piste.
pub fn meta_cache_id(title: &str, artist: &str) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let key = format!("{}\u{1f}{}", normalize_meta(artist), normalize_meta(title));
    let mut hash = FNV_OFFSET;
    for b in key.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Borne sur 63 bits puis négation ; 0 est réservé (« pas d'id »).
    let positive = (hash & 0x7fff_ffff_ffff_ffff) as i64;
    if positive == 0 { -1 } else { -positive }
}

// ---------------------------------------------------------------------------
// Cache layer (`lyrics_cache` table — exists in both SQLite and Postgres)
// ---------------------------------------------------------------------------

/// One row of the `lyrics_cache` table. `synced_lyrics` holds raw LRC text,
/// `plain_lyrics` the unsynced fallback; both `None` = cached negative.
#[derive(Debug, Clone, Default)]
pub struct LyricsCacheEntry {
    pub synced_lyrics: Option<String>,
    pub plain_lyrics: Option<String>,
    pub source: String,
    /// ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SSZ`).
    pub fetched_at: Option<String>,
}

impl LyricsCacheEntry {
    pub fn is_negative(&self) -> bool {
        self.synced_lyrics
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            && self
                .plain_lyrics
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
    }

    /// True when a cached negative is still fresh (no re-fetch needed).
    pub fn negative_still_fresh(&self) -> bool {
        let Some(ref fetched) = self.fetched_at else {
            return false;
        };
        // Both engines store `YYYY-MM-DDTHH:MM:SSZ`: lexicographic order is
        // chronological order for this fixed-width format.
        fetched.as_str() >= negative_retry_cutoff().as_str()
    }

    /// Vrai quand cette entrée **dispense d'interroger LRCLIB** : soit elle
    /// porte des paroles (les positifs n'expirent pas), soit c'est un échec
    /// encore frais.
    ///
    /// Règle unique partagée par la récupération à la demande
    /// ([`get_lyrics`]) et par la passe de fond
    /// (`crate::library::lyrics_pass`) : une recherche déjà payée — y compris
    /// une recherche **infructueuse** — ne doit jamais être repayée.
    pub fn spares_a_fetch(&self) -> bool {
        !self.is_negative() || self.negative_still_fresh()
    }
}

/// Horodatage ISO-8601 UTC en deçà duquel un échec en cache est périmé et
/// mérite une nouvelle tentative. Format à largeur fixe : la comparaison
/// lexicographique vaut comparaison chronologique, sur les deux moteurs.
pub fn negative_retry_cutoff() -> String {
    (chrono::Utc::now() - chrono::Duration::days(NEGATIVE_CACHE_TTL_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn dialect_sql(db: &Arc<dyn DbBackend>, f: impl Fn(&dyn SqlDialect) -> String) -> String {
    match db.engine() {
        Engine::Sqlite => f(&SqliteDialect),
        Engine::Postgres => f(&PostgresDialect),
    }
}

/// Load the cached `lyrics_cache` row for a track, if any.
pub fn load_cache_entry(db: &Arc<dyn DbBackend>, track_id: i64) -> Option<LyricsCacheEntry> {
    let sql = dialect_sql(db, |d| {
        format!(
            "SELECT synced_lyrics, plain_lyrics, source, fetched_at \
             FROM lyrics_cache WHERE track_id = {}",
            d.placeholder(1)
        )
    });
    let params: [&dyn ToSqlValue; 1] = [&track_id];

    let row = db.query_one(&sql, &params).ok()??;
    if row.len() < 4 {
        return None;
    }

    Some(LyricsCacheEntry {
        synced_lyrics: row[0].as_str().map(|s| s.to_string()),
        plain_lyrics: row[1].as_str().map(|s| s.to_string()),
        source: row[2].as_str().unwrap_or("lrclib").to_string(),
        fetched_at: row[3].as_str().map(|s| s.to_string()),
    })
}

/// Upsert a `lyrics_cache` row (works on SQLite and Postgres). Storing a
/// row with both bodies `None` records a negative result.
pub fn store_cache_entry(
    db: &Arc<dyn DbBackend>,
    track_id: i64,
    title: &str,
    artist: &str,
    synced_lyrics: Option<&str>,
    plain_lyrics: Option<&str>,
) {
    let sql = dialect_sql(db, |d| {
        format!(
            "INSERT INTO lyrics_cache \
             (track_id, title, artist, synced_lyrics, plain_lyrics, source, fetched_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT(track_id) DO UPDATE SET \
             title = excluded.title, artist = excluded.artist, \
             synced_lyrics = excluded.synced_lyrics, \
             plain_lyrics = excluded.plain_lyrics, \
             source = excluded.source, fetched_at = excluded.fetched_at",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
        )
    });

    let fetched_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let source = "lrclib";
    let params: [&dyn ToSqlValue; 7] = [
        &track_id,
        &title,
        &artist,
        &synced_lyrics as &dyn ToSqlValue,
        &plain_lyrics as &dyn ToSqlValue,
        &source,
        &fetched_at,
    ];

    if let Err(e) = db.execute(&sql, &params) {
        warn!(error = %e, track_id, "lyrics_cache_store_failed");
    }
}

// ---------------------------------------------------------------------------
// Public API: cache-first, fallback to LRCLIB
// ---------------------------------------------------------------------------

/// Get lyrics for a track. Checks the DB cache first, then falls back
/// to LRCLIB if not cached (negatives are retried after 14 days).
pub async fn get_lyrics(
    db: &Arc<dyn DbBackend>,
    client: &reqwest::Client,
    track_id: i64,
    title: &str,
    artist: &str,
    album: Option<&str>,
    duration_ms: i64,
) -> Result<Lyrics, String> {
    // 1. Try cache.
    if let Some(cached) = load_cache_entry(db, track_id) {
        if cached.spares_a_fetch() {
            debug!(track_id, "lyrics_cache_hit");
            let lines = cached
                .synced_lyrics
                .as_deref()
                .map(parse_lrc)
                .unwrap_or_default();
            return Ok(Lyrics {
                synced: !lines.is_empty(),
                lines,
                plain_text: cached.plain_lyrics,
                source: cached.source,
            });
        }
    }

    // 2. Fetch from LRCLIB.
    let duration_secs = if duration_ms > 0 {
        Some(duration_ms / 1000)
    } else {
        None
    };

    let raw = fetch_lrclib_raw(client, artist, title, album, duration_secs)
        .await?
        .unwrap_or_default();

    // 3. Cache result (even empty — avoids repeated failed lookups).
    store_cache_entry(
        db,
        track_id,
        title,
        artist,
        raw.synced_lyrics.as_deref(),
        raw.plain_lyrics.as_deref(),
    );

    let lines = raw
        .synced_lyrics
        .as_deref()
        .map(parse_lrc)
        .unwrap_or_default();

    Ok(Lyrics {
        synced: !lines.is_empty(),
        lines,
        plain_text: raw.plain_lyrics,
        source: "lrclib".into(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── #2997 : sémantique du décalage de paroles par zone ─────────────────

    #[test]
    fn decalage_nul_rend_la_position_inchangee() {
        // TÉMOIN : c'est le cas de toutes les zones existantes. Zéro doit être
        // l'identité, sinon le correctif déplacerait les paroles de tout le
        // monde en prétendant ne rien changer.
        for pos in [0, 1, 12_340, 16_000, 3_600_000] {
            assert_eq!(sync_position_ms(pos, 0), pos, "position {pos}");
        }
    }

    #[test]
    fn decalage_positif_retarde_les_paroles() {
        // Positif = paroles retardées : on lit les horodatages comme si l'on
        // était PLUS TÔT dans le morceau, donc chaque ligne s'active plus tard.
        assert_eq!(sync_position_ms(16_000, 3_000), 13_000);
        assert_eq!(sync_position_ms(60_000, 60_000), 0);
    }

    #[test]
    fn decalage_negatif_avance_les_paroles() {
        assert_eq!(sync_position_ms(28_000, -3_000), 31_000);
    }

    #[test]
    fn la_position_corrigee_ne_descend_jamais_sous_zero() {
        // Au tout début d'un morceau, un décalage positif dépasse la position.
        // Une position négative n'existe pas et laisserait le début du morceau
        // sans ligne active.
        assert_eq!(sync_position_ms(1_000, 5_000), 0);
        assert_eq!(sync_position_ms(0, 60_000), 0);
    }

    #[test]
    fn les_bornes_du_reglage_ne_debordent_pas() {
        // Le réglage est borné à ±60 s par la route ; ces bornes doivent
        // rester sans surprise même à des positions extrêmes.
        assert_eq!(sync_position_ms(i64::MAX, -60_000), i64::MAX);
        assert_eq!(sync_position_ms(i64::MIN, 60_000), 0);
    }

    #[test]
    fn parse_lrc_basic() {
        let lrc = "\
[00:12.34] First line
[00:15.00] Second line
[01:02.50] Third line
";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].time_ms, 12_340);
        assert_eq!(lines[0].text, "First line");
        assert_eq!(lines[1].time_ms, 15_000);
        assert_eq!(lines[2].time_ms, 62_500);
        assert_eq!(lines[2].text, "Third line");
    }

    #[test]
    fn parse_lrc_three_digit_frac() {
        let lrc = "[00:05.123] Precise";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 5_123);
    }

    #[test]
    fn parse_lrc_empty_and_garbage() {
        let lrc = "\n\nnot a timestamp\n[bad] nope\n";
        let lines = parse_lrc(lrc);
        assert!(lines.is_empty());
    }

    #[test]
    fn parse_lrc_single_digit_frac() {
        let lrc = "[00:03.5] Single";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 3_500);
    }

    #[test]
    fn normalize_meta_case_and_whitespace() {
        assert_eq!(normalize_meta("  Miles   DAVIS "), "miles davis");
        assert_eq!(normalize_meta("So\tWhat"), "so what");
        assert_eq!(normalize_meta(""), "");
    }

    #[test]
    fn meta_cache_id_stable_and_negative() {
        let a = meta_cache_id("So What", "Miles Davis");
        // Négatif : ne peut jamais entrer en collision avec un track_id réel.
        assert!(a < 0);
        // Stable et insensible à la casse / aux espaces multiples.
        assert_eq!(a, meta_cache_id("so  what", "MILES  DAVIS"));
        // Titre et artiste ne sont pas interchangeables.
        assert_ne!(a, meta_cache_id("Miles Davis", "So What"));
        // Une autre paire donne une autre clé.
        assert_ne!(a, meta_cache_id("Blue in Green", "Miles Davis"));
    }

    #[test]
    fn negative_cache_freshness() {
        let fresh = LyricsCacheEntry {
            fetched_at: Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            ..Default::default()
        };
        assert!(fresh.is_negative());
        assert!(fresh.negative_still_fresh());

        let stale = LyricsCacheEntry {
            fetched_at: Some("2020-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        assert!(stale.is_negative());
        assert!(!stale.negative_still_fresh());

        let missing = LyricsCacheEntry::default();
        assert!(!missing.negative_still_fresh());
    }

    // ── #3815 : la mention de qualité ne part plus dans /api/get ───────────
    //
    // Les témoins vont PAR PAIRES : ce qui doit être nettoyé, et ce qui doit
    // rester intact. C'est la seconde moitié qui fait la valeur de la règle —
    // une règle plus large passerait la première liste et casserait la seconde.

    #[test]
    fn les_trois_exemples_du_testeur_sont_nettoyes() {
        // TÉMOIN : les trois titres cités mot pour mot par Pierre M (fil 1748).
        // Si l'un d'eux cesse d'être nettoyé, le ticket n'est plus couvert.
        assert_eq!(
            album_sans_mention_de_qualite("Unplugged - SACD(2021)"),
            "Unplugged"
        );
        assert_eq!(
            album_sans_mention_de_qualite("Innuendo - 24/96"),
            "Innuendo"
        );
        assert_eq!(
            album_sans_mention_de_qualite("A Kind of Magic - 24/96(1986)"),
            "A Kind of Magic"
        );
    }

    #[test]
    fn les_formes_voisines_sont_nettoyees_aussi() {
        // « avec ou sans parenthèse, des tirets,... » — les trois formes de
        // suffixe que le testeur décrit, et les conteneurs les plus courants.
        for (sale, propre) in [
            ("Innuendo (24/96)", "Innuendo"),
            ("Innuendo [24-96]", "Innuendo"),
            ("Innuendo 24/192", "Innuendo"),
            ("Brothers in Arms - SACD", "Brothers in Arms"),
            ("Brothers in Arms – DSD64", "Brothers in Arms"),
            ("Love Over Gold - 24/192 (1982)", "Love Over Gold"),
            ("Kind of Blue (Hi-Res)", "Kind of Blue"),
            ("Kind of Blue - 96kHz", "Kind of Blue"),
            ("Kind of Blue - 24bit", "Kind of Blue"),
            ("Kind of Blue (MQA)", "Kind of Blue"),
            (
                "Raconte-moi... (Bonus Edition) - 2010 (24-96)",
                "Raconte-moi... (Bonus Edition)",
            ),
        ] {
            assert_eq!(album_sans_mention_de_qualite(sale), propre, "« {sale} »");
        }
    }

    #[test]
    fn un_titre_legitime_ressort_intact() {
        // TÉMOIN INVERSE, et c'est le plus important des deux : une fusion à
        // tort coûte plus cher qu'un nettoyage manqué. Chacun de ces titres
        // contient de quoi tromper une règle trop large — un nombre, une
        // profondeur, une fréquence, une année, un mot d'édition.
        for intact in [
            "24 Carat Black",        // la mention est au DÉBUT, pas en suffixe
            "24/7",                  // « 7 » n'est pas une fréquence
            "Rock 24/7",             // idem, cette fois EN suffixe
            "1999",                  // une année seule ne fonde rien
            "Unplugged (2021)",      // idem, même entre parenthèses
            "96 Tears",              // un nombre qui n'est pas un suffixe
            "The Wall (Remastered)", // un mot inconnu arrête tout
            "Kind of Blue (Legacy Edition)",
            "Hi-Res Audio Sampler", // la mention est au DÉBUT
            "SACD",                 // le titre ENTIER est la mention
            "DSD",
            "24/96",
            "Innuendo",
            "MTV Unplugged in New York",
            "Sixteen Stone",
            "Aqualung - 40th Anniversary Edition",
        ] {
            assert_eq!(
                album_sans_mention_de_qualite(intact),
                intact,
                "« {intact} » ne doit PAS être touché"
            );
        }
    }

    #[test]
    fn le_repli_n_existe_que_si_le_nettoyage_change_quelque_chose() {
        // TÉMOIN : c'est lui qui borne le COÛT. `None` = pas de seconde
        // requête. Si un titre sans mention de qualité rendait `Some`, chaque
        // échec LRCLIB paierait un appel de plus — sur une passe de fond,
        // le double du trafic vers un service communautaire sans clef d'API.
        assert_eq!(album_de_repli(None), None);
        assert_eq!(album_de_repli(Some("")), None);
        assert_eq!(album_de_repli(Some("   ")), None);
        assert_eq!(album_de_repli(Some("Innuendo")), None);
        assert_eq!(album_de_repli(Some("The Wall (Remastered)")), None);
        assert_eq!(
            album_de_repli(Some("Innuendo - 24/96")),
            Some("Innuendo".to_string())
        );
    }
}
