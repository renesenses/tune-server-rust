use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use tracing::{debug, info, warn};

/// Best-effort diagnostic counter of artwork downloads rejected with a
/// rate-limit status (429/503), incremented in [`download_image`]. An
/// enrichment batch reads the delta over its own run and reports it, so a run
/// that "found nothing" can tell retryable throttling apart from genuinely
/// absent artwork (Jean Valjean #1096). It counts download-level hits (a single
/// artist may hit several sources) and is process-global — fine because
/// enrichment runs are serialised; `Relaxed` is enough for a diagnostic count.
/// Per-artist precision will come with the fetch-layer refactor (typed
/// `FetchOutcome` propagated up the source cascade).
static ARTWORK_RATE_LIMIT_HITS: AtomicU32 = AtomicU32::new(0);

/// Candidate filenames for folder-level cover art.
///
/// On case-insensitive filesystems (NTFS, APFS) duplicates are harmless.
/// On case-sensitive mounts (some NAS/SMB) we need several variants.
const FOLDER_COVER_NAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "front.jpg",
    "front.jpeg",
    "front.png",
    "album.jpg",
    "album.jpeg",
    "album.png",
    "Cover.jpg",
    "Cover.jpeg",
    "Cover.png",
    "Folder.jpg",
    "Folder.jpeg",
    "Folder.png",
    "Front.jpg",
    "Front.jpeg",
    "Front.png",
    "COVER.JPG",
    "COVER.JPEG",
    "COVER.PNG",
    "FOLDER.JPG",
    "FOLDER.JPEG",
    "FOLDER.PNG",
    "FRONT.JPG",
    "FRONT.JPEG",
    "FRONT.PNG",
];

const MB_USER_AGENT: &str = "Tune/0.1.0 (https://mozaiklabs.fr)";

/// Wrap an absolute Windows path for extended-length (`\\?\`) access.
///
/// The Win32 file APIs (used by `std::fs` and `lofty`) reject paths longer than
/// MAX_PATH (260 chars) with "The system cannot find the path specified.
/// (os error 3)" unless the path carries the extended-length `\\?\` prefix.
/// This surfaced as missing cover art for albums whose folder/file names push
/// the full path past 260 chars on Windows (Thibaud): the tracks scan fine but
/// the artwork read on the full path fails.
///
/// Pure string transform, safe on every platform: it only rewrites paths that
/// look like Windows absolute paths (drive `C:\…` or UNC `\\server\…`), so on
/// Unix (paths starting with `/`) and for relative paths it is a no-op. Verbatim
/// paths require `\` separators, so we normalize `/` and only touch already
/// absolute paths.
pub(crate) fn extended_path(path: &Path) -> std::borrow::Cow<'_, Path> {
    use std::borrow::Cow;
    let s = match path.to_str() {
        Some(s) => s,
        None => return Cow::Borrowed(path), // non-UTF-8: leave untouched
    };
    if s.starts_with("\\\\?\\") {
        return Cow::Borrowed(path); // already verbatim
    }
    let b = s.as_bytes();
    let is_drive = b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'\\' || b[2] == b'/');
    let is_unc = s.starts_with("\\\\") || s.starts_with("//");
    if !is_drive && !is_unc {
        return Cow::Borrowed(path); // Unix / relative path: no-op
    }
    let normalized = s.replace('/', "\\");
    let prefixed = if is_unc {
        // \\server\share\… -> \\?\UNC\server\share\…
        format!("\\\\?\\UNC\\{}", normalized.trim_start_matches('\\'))
    } else {
        format!("\\\\?\\{normalized}")
    };
    Cow::Owned(std::path::PathBuf::from(prefixed))
}

pub fn extract_cover_art(audio_path: &Path) -> Option<(Vec<u8>, String)> {
    use lofty::file::TaggedFileExt;

    match lofty::read_from_path(&*extended_path(audio_path)) {
        Ok(tagged) => {
            if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
                if let Some(pic) = tag.pictures().first() {
                    let mime = match pic.mime_type() {
                        Some(lofty::picture::MimeType::Jpeg) => "image/jpeg",
                        Some(lofty::picture::MimeType::Png) => "image/png",
                        Some(lofty::picture::MimeType::Bmp) => "image/bmp",
                        _ => "image/jpeg",
                    };
                    return Some((pic.data().to_vec(), mime.to_string()));
                }
            }
        }
        Err(e) => {
            debug!(
                path = %audio_path.display(),
                error = %e,
                "artwork_lofty_read_failed"
            );
        }
    }

    // DSF files store their ID3v2 tag — including embedded APIC artwork — at
    // an offset that lofty does not read, so the path above finds no picture.
    // Fall back to reading the cover directly from the DSF metadata chunk.
    crate::metadata::extract_dsf_cover(audio_path)
}

pub fn find_folder_cover(audio_path: &Path) -> Option<PathBuf> {
    let dir = audio_path.parent()?;
    for name in FOLDER_COVER_NAMES {
        let candidate = dir.join(name);
        if extended_path(&candidate).exists() {
            return Some(candidate);
        }
    }
    None
}

/// Extensions sous lesquelles une entrée de cache de pochette peut exister,
/// dans l'ordre où on la cherche.
///
/// C'est le **contrat unique** entre celui qui écrit dans le cache
/// ([`save_to_cache`]) et celui qui le sert
/// (`tune-server/src/routes/library/artwork.rs::serve_artwork`). Tant que les
/// deux listes vivaient séparément, un fichier pouvait être écrit sous un nom
/// que la route ne regardait jamais : la base annonçait alors un condensat dont
/// la route rendait 404, et l'écran affichait l'image de remplacement (#2567).
///
/// Les quatre premières sont les seules que [`save_to_cache`] produit
/// désormais. Les suivantes sont les orthographes **héritées** : l'extension du
/// fichier source était jusqu'ici recopiée telle quelle, et
/// `FOLDER_COVER_NAMES` accepte `cover.jpeg`, `FOLDER.JPG`, `Front.png`… Les
/// caches déjà constitués en sont pleins. Les garder ici les guérit **sans rien
/// réécrire et sans changer un seul condensat** : aucune URL de pochette ne
/// bouge, donc aucun cache de navigateur n'est invalidé.
pub const CACHE_EXTENSIONS: &[&str] = &[
    // Écrites aujourd'hui, dans l'ordre de fréquence.
    "jpg", "png", "webp", "bmp", // Héritées : orthographes recopiées d'un fichier source.
    "jpeg", "JPG", "JPEG", "PNG", "Jpg", "Jpeg", "Png",
];

/// Orthographe canonique sous laquelle une entrée de cache doit être écrite.
///
/// Écrire `{hash}.jpeg` ou `{hash}.JPG` revenait à annoncer un condensat que la
/// route ne trouvait pas. On ne touche ni au condensat ni au contenu : seule
/// l'orthographe de l'extension est ramenée à celle que la lecture cherche en
/// premier. Une extension inconnue devient `jpg`, ce qui est déjà ce que
/// `extract_cover_art` suppose pour un type d'image qu'il ne reconnaît pas.
pub fn canonical_cache_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "png",
        "webp" => "webp",
        "bmp" => "bmp",
        _ => "jpg",
    }
}

/// Type MIME d'une entrée de cache, d'après son extension.
pub fn cache_mime(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/jpeg",
    }
}

/// Retrouve le fichier de cache d'un condensat, s'il existe.
///
/// Rend le chemin **et** le type MIME à servir. `None` signifie que le
/// condensat n'a aucun fichier : l'annoncer en base est alors un mensonge.
pub fn find_cached(cache_dir: &Path, hash: &str) -> Option<(PathBuf, &'static str)> {
    for ext in CACHE_EXTENSIONS {
        let path = cache_dir.join(format!("{hash}.{ext}"));
        if path.exists() {
            return Some((path, cache_mime(ext)));
        }
    }
    None
}

pub fn save_to_cache(data: &[u8], cache_dir: &Path, hash: &str, ext: &str) -> Option<PathBuf> {
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
        warn!(
            dir = %cache_dir.display(),
            error = %e,
            "artwork_cache_dir_create_failed — check directory permissions"
        );
        return None;
    }
    // L'extension vient souvent d'un fichier source (`cover.jpeg`,
    // `FOLDER.JPG`) ou d'un type MIME (`image/bmp`). La ramener à l'orthographe
    // que la lecture cherche est la seule façon de garantir que le condensat
    // qu'on va annoncer sera servi (#2567).
    let ext = canonical_cache_ext(ext);
    let filename = format!("{hash}.{ext}");
    let path = cache_dir.join(&filename);
    if let Err(e) = std::fs::write(&path, data) {
        warn!(
            path = %path.display(),
            error = %e,
            size = data.len(),
            "artwork_cache_write_failed — check directory permissions"
        );
        return None;
    }
    Some(path)
}

/// Compute a deterministic hash for an artwork cache key.
///
/// On Windows, backslashes are normalized to forward slashes so that the
/// same audio file always produces the same hash regardless of how the
/// path was constructed (e.g. `C:\Music\a.flac` and `C:/Music/a.flac`
/// yield the same hash).
pub fn artwork_hash(file_path: &str) -> String {
    use md5::{Digest, Md5};
    let normalized = file_path.replace('\\', "/");
    let mut hasher = Md5::new();
    hasher.update(normalized.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Condensat de CONTENU d'une image : SHA-256 des octets, en hexadécimal.
///
/// Adressage par le contenu (#1444) : deux fichiers aux mêmes octets partagent
/// une seule entrée de cache, quel que soit leur chemin. Les compilations
/// éclatées façon Qobuz (#1440) — une jaquette identique recopiée dans N
/// dossiers d'artiste — cessent ainsi de peupler le cache de N copies.
///
/// **Octets bruts, pas pixels décodés.** Mesuré le 29/08/2026 sur les deux
/// bibliothèques de référence : décoder gagne 4 groupes sur 6 285 (.18) et
/// 84 sur 7 600 (.15), normaliser en 256×256 en gagne zéro, et 11 fichiers de
/// .15 sont illisibles par un décodeur — sous un schéma adressé par les pixels
/// ils n'auraient plus d'adresse du tout. Le signal *perceptuel* (« même image
/// malgré un ré-encodage ») existe séparément et reste où il est :
/// [`crate::scanner::compilation::CoverFingerprint`], consommé par le
/// regroupement des compilations.
///
/// 64 hexdigits : accepté tel quel par toutes les routes de lecture
/// (`is_hex_hash` reconnaît 32 **et** 64 caractères, route HTTP comme
/// `upnp_server::artwork_url`), donc aucune URL existante ne change de forme.
pub fn content_hash(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Met en cache une image **fraîchement récupérée en ligne** et rend son
/// adresse, adressée par son CONTENU (#1444).
///
/// Le pendant écriture de [`content_hash`] pour les producteurs qui n'ont
/// aucune entrée héritée à ménager : ils viennent de télécharger des octets
/// neufs, il n'y a rien à sonder. Ils écrivaient jusqu'ici sous un condensat
/// d'**identité** figé — `artwork_hash(mbid)`,
/// `artwork_hash("{artiste}|{titre}")`, `artwork_hash("artist-mbid-{mbid}")`,
/// `artwork_hash("artist-name-{nom}")` — ce qui posait deux défauts opposés,
/// que l'adressage par le contenu referme tous les deux :
///
/// - **la même adresse pour deux images différentes.** Deux albums distincts
///   qui partagent nom d'artiste et titre écrivent au même endroit : le second
///   enrichi écrase la pochette du premier, et les deux lignes de la base
///   pointent la même image. Mesuré le 30/08/2026 sur `.18` : **5 groupes,
///   11 albums** collisionnent sur `{artiste}|{titre}`, plus 1 groupe / 2
///   albums sur le MBID. Le commentaire de la phase 2 des images d'artistes
///   garde la trace du même défaut déjà survenu, en pire — un MBID vide faisait
///   converger *tous* les artistes sans MBID sur `md5("artist-mbid-")`.
/// - **la même adresse pour deux versions successives d'une image.** Un
///   re-téléchargement (`force`, bouton « re-télécharger les images
///   d'artistes ») réécrit sous l'adresse déjà distribuée, que la route sert
///   `Cache-Control: immutable, max-age=31536000` : navigateurs et cache
///   d'images Flutter continuent d'afficher l'ancienne image **un an**. C'est
///   le défaut refermé pour les téléversements en v0.9.127, laissé nu sur les
///   chemins d'enrichissement.
///
/// Sous SHA-256 des octets, deux images différentes ne peuvent pas se retrouver
/// à la même adresse, et deux images identiques au bit près partagent une
/// entrée — ce qui est le comportement voulu, aucun chemin de suppression par
/// entrée n'existant dans le dépôt.
pub fn cache_fetched_image(data: &[u8], cache_dir: &Path, ext: &str) -> Option<String> {
    let hash = content_hash(data);
    // Déjà en cache sous cette adresse : mêmes octets, rien à réécrire.
    if find_cached(cache_dir, &hash).is_some() {
        return Some(hash);
    }
    save_to_cache(data, cache_dir, &hash, ext).map(|_| hash)
}

/// Fetch front cover art from the Cover Art Archive using a MusicBrainz release ID.
pub async fn fetch_cover_art(mbid: &str) -> Option<Vec<u8>> {
    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    // Prefer the 1200px rendition for a crisp full-screen display (Now Playing
    // on Retina). Fall back to 500px if the larger size isn't available for
    // this release.
    for size in ["front-1200", "front-500"] {
        let url = format!("https://coverartarchive.org/release/{mbid}/{size}");
        crate::http::fetch::MUSICBRAINZ.acquire("mb").await;
        let Ok(resp) = client.get(&url).send().await else {
            continue;
        };
        if resp.status().is_success() {
            let Ok(bytes) = resp.bytes().await else {
                continue;
            };
            // Reject tiny responses (likely error pages)
            if bytes.len() < 1000 {
                continue;
            }
            return Some(bytes.to_vec());
        }
    }
    None
}

/// Search MusicBrainz for a release MBID by artist name and album title.
/// Returns the first matching release ID, or None.
pub async fn search_musicbrainz_release(artist: &str, title: &str) -> Option<String> {
    let query = format!(
        "release:\"{}\" AND artist:\"{}\"",
        title.replace('"', ""),
        artist.replace('"', "")
    );
    let url = format!(
        "https://musicbrainz.org/ws/2/release/?query={}&fmt=json&limit=1",
        urlencoding::encode(&query)
    );
    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    crate::http::fetch::MUSICBRAINZ.acquire("mb").await;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let releases = data.get("releases")?.as_array()?;
    let first = releases.first()?;
    first.get("id")?.as_str().map(|s| s.to_string())
}

/// Resolve an artist's MusicBrainz ID from its name (best match).
///
/// Libraries whose files carry no MusicBrainz tags leave artists without an
/// MBID, so the rich MBID-based image sources (Fanart.tv / TheAudioDB /
/// MusicBrainz) can never find them. Look the artist up by name and accept only
/// a high-confidence match (MB returns a 0-100 score) to avoid mis-binding two
/// artists that share a name.
pub async fn search_musicbrainz_artist(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("Unknown Artist") {
        return None;
    }
    let query = format!("artist:\"{}\"", trimmed.replace('"', ""));
    let url = format!(
        "https://musicbrainz.org/ws/2/artist/?query={}&fmt=json&limit=1",
        urlencoding::encode(&query)
    );
    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    crate::http::fetch::MUSICBRAINZ.acquire("mb").await;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let first = data.get("artists")?.as_array()?.first()?;
    // Only accept a confident match; MB scores the query 0-100.
    let score = first.get("score").and_then(|v| v.as_i64()).unwrap_or(0);
    if score < 90 {
        return None;
    }
    first.get("id")?.as_str().map(|s| s.to_string())
}

/// Run batch artwork enrichment for all albums missing cover art.
///
/// Iterates over albums without a `cover_path`, tries Cover Art Archive
/// (by existing MBID or by searching MusicBrainz), saves the image to the
/// artwork cache, and updates the album's `cover_path` in the database.
///
/// Respects MusicBrainz rate limit: max 1 request/second.
/// Upscale an Apple `artworkUrl100` (a 100x100 thumbnail) to a full-resolution
/// rendition by swapping the size segment (e.g. `.../100x100bb.jpg` ->
/// `.../1200x1200bb.jpg`). Returns `None` if the URL isn't in the expected form.
fn itunes_hires_url(art100: &str) -> Option<String> {
    if art100.contains("100x100bb") {
        Some(art100.replace("100x100bb", "1200x1200bb"))
    } else {
        None
    }
}

/// Apple/iTunes cover-art fallback (port of #769, adapted to the v0.9
/// enrichment structure). Cover Art Archive is sparse and the MusicBrainz
/// release match is strict, so many local albums never get a cover. Apple's
/// catalog is far denser for mainstream music and needs no MBID — search by
/// artist + title and download the highest-res rendition available.
async fn fetch_itunes_cover(artist: &str, title: &str) -> Option<Vec<u8>> {
    let term = format!("{artist} {title}");
    let url = format!(
        "https://itunes.apple.com/search?term={}&media=music&entity=album&limit=1",
        urlencoding::encode(&term)
    );
    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let art100 = data
        .get("results")?
        .as_array()?
        .first()?
        .get("artworkUrl100")?
        .as_str()?;
    async fn dl(client: &reqwest::Client, u: &str) -> Option<Vec<u8>> {
        let r = client.get(u).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.bytes().await.ok().map(|b| b.to_vec())
    }
    if let Some(hi) = itunes_hires_url(art100)
        && let Some(bytes) = dl(&client, &hi).await
    {
        return Some(bytes);
    }
    dl(&client, art100).await
}

pub async fn batch_enrich_artwork(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
) {
    batch_enrich_artwork_scoped(db, cache_dir, None).await
}

/// Variante à portée (#1660) : ne retient comme candidats que les albums de la
/// portée. `None` = passe complète, strictement identique à l'historique. Le
/// filtre ne touche QUE la sélection — le reste de la passe est le même code.
pub async fn batch_enrich_artwork_scoped(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
    scope: Option<crate::metadata::enrich_scope::EnrichScope>,
) {
    let album_repo = crate::db::album_repo::AlbumRepo::with_backend(db.clone());
    let mut albums = match album_repo.list_without_cover() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_artwork_list_failed");
            return;
        }
    };
    if let Some(scope) = &scope {
        let avant = albums.len();
        albums.retain(|(id, ..)| scope.contient_album(*id));
        info!(
            dir = %scope.dir,
            retained = albums.len(),
            dropped = avant - albums.len(),
            "batch_artwork_scope_applied"
        );
    }

    if albums.is_empty() {
        info!("batch_artwork_skip_all_have_covers");
        return;
    }

    info!(count = albums.len(), "batch_artwork_enrichment_started");

    let mut enriched = 0u32;
    let mut searched = 0u32;
    let mut failed = 0u32;

    for (album_id, title, artist_name, mbid) in &albums {
        let artist = artist_name.as_deref().unwrap_or("Unknown Artist");

        // Step 1: Determine MBID — use existing or search MusicBrainz
        let resolved_mbid = if let Some(id) = mbid {
            if !id.is_empty() {
                Some(id.clone())
            } else {
                None
            }
        } else {
            None
        };

        let mbid_to_use = if let Some(id) = resolved_mbid {
            Some(id)
        } else {
            // Search MusicBrainz for the release (the shared MB rate limiter
            // inside `search_musicbrainz_release` enforces the ~1 req/s spacing).
            searched += 1;
            let found = search_musicbrainz_release(artist, title).await;
            if let Some(ref id) = found {
                // Store the discovered MBID on the album for future use
                db.execute(
                    "UPDATE albums SET musicbrainz_release_id = ? WHERE id = ? AND (musicbrainz_release_id IS NULL OR musicbrainz_release_id = '')",
                    &[&id.as_str() as &dyn crate::db::backend::ToSqlValue, album_id],
                ).ok();
                debug!(album_id, mbid = %id, album = %title, "batch_artwork_mbid_found");
            }
            found
        };

        // Step 2: Cover Art Archive first (needs an MBID; the shared MB rate
        // limiter inside `fetch_cover_art` enforces the ~1 req/s spacing);
        // fall back to Apple/iTunes artwork, which needs no MBID and has a far
        // denser catalog for mainstream music. Without this fallback, albums
        // with no MB match or no CAA image never got a cover (Fabien: 0/22).
        let mut fetched: Option<Vec<u8>> = None;
        if let Some(ref mbid_val) = mbid_to_use {
            fetched = fetch_cover_art(mbid_val).await;
            if fetched.is_none() {
                debug!(album_id, album = %title, mbid = %mbid_val, "batch_artwork_caa_not_found");
            }
        } else {
            debug!(album_id, album = %title, artist = %artist, "batch_artwork_no_mbid");
        }
        if fetched.is_none() && artist != "Unknown Artist" {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            fetched = fetch_itunes_cover(artist, title).await;
            if fetched.is_some() {
                debug!(album_id, album = %title, "batch_artwork_itunes_found");
            }
        }

        match fetched {
            Some(data) => {
                // Adressage par le CONTENU (#1444). L'ancienne clé était
                // l'identité de l'album — le MBID, sinon `{artiste}|{titre}` —
                // ce qui faisait écrire DEUX albums distincts au même endroit
                // dès qu'ils partagent artiste et titre (5 groupes / 11 albums
                // mesurés sur .18) : le second enrichi écrasait la pochette du
                // premier. Voir `cache_fetched_image`.
                std::fs::create_dir_all(&cache_dir).ok();
                if let Some(hash) = cache_fetched_image(&data, &cache_dir, "jpg") {
                    album_repo.update_cover_path(*album_id, &hash).ok();
                    enriched += 1;
                    info!(
                        album_id,
                        album = %title,
                        artist = %artist,
                        hash = %hash,
                        size = data.len(),
                        "batch_artwork_enriched"
                    );
                } else {
                    failed += 1;
                    warn!(album_id, album = %title, "batch_artwork_save_failed");
                }
            }
            None => {
                failed += 1;
                debug!(album_id, album = %title, artist = %artist, "batch_artwork_not_found");
            }
        }
    }

    info!(
        total = albums.len(),
        enriched, searched, failed, "batch_artwork_enrichment_complete"
    );

    // Store result in settings for status reporting
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db);
    settings
        .set(
            "artwork_enrich_result",
            &serde_json::json!({
                "total": albums.len(),
                "enriched": enriched,
                "searched": searched,
                "failed": failed,
            })
            .to_string(),
        )
        .ok();
}

/// Fetch an artist image from multiple sources (best-effort cascade).
///
/// Order: mozaiklabs community → Fanart.tv → TheAudioDB → MusicBrainz
/// direct image → MusicBrainz→Wikidata→Wikimedia → Discogs → Last.fm.
pub async fn fetch_artist_image(
    mbid: &str,
    artist_name: &str,
    discogs_token: Option<&str>,
) -> Option<Vec<u8>> {
    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;

    // 1. Mozaiklabs community by MBID (fastest, no rate limit) — highest priority
    if !mbid.is_empty() {
        if let Some(bytes) = fetch_artist_image_mozaiklabs(&client, mbid).await {
            return Some(bytes);
        }
    }

    // 1b. Mozaiklabs community by NAME — keeps mozaiklabs the top priority even
    // for artists without an MBID (which never reach the by-MBID lookup above),
    // BEFORE falling back to any external source.
    if !artist_name.is_empty() {
        if let Some(bytes) = fetch_artist_image_mozaiklabs_by_name(&client, artist_name).await {
            return Some(bytes);
        }
    }

    // Sources 2–5 are keyed by MBID; skip them entirely for artists without one
    // (avoids pointless requests + their rate-limit sleeps during a force pass).
    if !mbid.is_empty() {
        // 2. Fanart.tv
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Some(bytes) = fetch_artist_image_fanart(&client, mbid).await {
            return Some(bytes);
        }

        // 3. TheAudioDB (free API, good coverage)
        if let Some(bytes) = fetch_artist_image_theaudiodb(&client, mbid).await {
            return Some(bytes);
        }

        // 4+5. MusicBrainz: try direct image relation, then Wikidata→Wikimedia
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Some(bytes) = fetch_artist_image_musicbrainz_full(&client, mbid).await {
            return Some(bytes);
        }
    }

    // 6. Discogs (if token configured, search by artist name)
    if !artist_name.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Some(bytes) = fetch_artist_image_discogs(&client, artist_name, discogs_token).await {
            return Some(bytes);
        }
    }

    // 7. Last.fm (artist.getinfo → image array, "extralarge" or "mega")
    if !artist_name.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(bytes) = fetch_artist_image_lastfm(&client, artist_name).await {
            return Some(bytes);
        }
    }

    None
}

/// Fetch an artist thumbnail from Fanart.tv using a MusicBrainz artist ID.
async fn fetch_artist_image_fanart(client: &reqwest::Client, mbid: &str) -> Option<Vec<u8>> {
    let api_key = std::env::var("FANART_TV_API_KEY").ok()?;
    if api_key.is_empty() {
        return None;
    }
    let url = format!("http://webservice.fanart.tv/v3/music/{mbid}?api_key={api_key}");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let thumb_url = data
        .get("artistthumb")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|obj| obj.get("url"))
        .and_then(|v| v.as_str())?;
    download_image(client, thumb_url).await
}

async fn fetch_artist_image_mozaiklabs(client: &reqwest::Client, mbid: &str) -> Option<Vec<u8>> {
    let resp = client
        .get(format!("https://mozaiklabs.fr/api/v1/artists/{mbid}"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let image_url = data
        .pointer("/data/image_url")
        .or_else(|| data.get("image_url"))
        .and_then(|v| v.as_str())?;
    if image_url.is_empty() {
        return None;
    }
    let full_url = if image_url.starts_with('/') {
        format!("https://mozaiklabs.fr{image_url}")
    } else {
        image_url.to_string()
    };
    download_image(client, &full_url).await
}

/// Fetch an artist image from mozaiklabs.fr by **name** (community metadata),
/// via `GET /api/v1/artists/search?q=<name>`. Used as a fallback for artists
/// without an MBID so mozaiklabs stays the priority source for them too.
///
/// Requires an exact (case-insensitive) name match on a result that actually
/// has a non-empty `image_url`, to avoid grabbing the wrong artist from the
/// substring (`ilike %q%`) search.
async fn fetch_artist_image_mozaiklabs_by_name(
    client: &reqwest::Client,
    artist_name: &str,
) -> Option<Vec<u8>> {
    let q = artist_name.trim();
    if q.len() < 2 {
        return None;
    }
    let url = format!(
        "https://mozaiklabs.fr/api/v1/artists/search?q={}",
        urlencoding::encode(q)
    );
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let results = data.get("data")?.as_array()?;
    let image_url = results
        .iter()
        .filter(|a| {
            a.get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| artist_name_matches_exactly(n, q))
        })
        .find_map(|a| {
            a.get("image_url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })?;
    let full_url = if image_url.starts_with('/') {
        format!("https://mozaiklabs.fr{image_url}")
    } else {
        image_url.to_string()
    };
    download_image(client, &full_url).await
}

/// Fetch artist image from TheAudioDB (free API key "2").
async fn fetch_artist_image_theaudiodb(client: &reqwest::Client, mbid: &str) -> Option<Vec<u8>> {
    let url = format!("https://theaudiodb.com/api/v1/json/2/artist-mb.php?i={mbid}");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let artist = data["artists"].as_array()?.first()?;
    let thumb_url = artist["strArtistThumb"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| artist["strArtistFanart"].as_str().filter(|s| !s.is_empty()))
        .or_else(|| artist["strArtistCutout"].as_str().filter(|s| !s.is_empty()))?;
    download_image(client, thumb_url).await
}

/// Fetch artist image from MusicBrainz: tries direct Wikimedia image relation
/// first, then falls back to Wikidata → P18 image property.
async fn fetch_artist_image_musicbrainz_full(
    client: &reqwest::Client,
    mbid: &str,
) -> Option<Vec<u8>> {
    let url = format!("https://musicbrainz.org/ws/2/artist/{mbid}?inc=url-rels&fmt=json");
    crate::http::fetch::MUSICBRAINZ.acquire("mb").await;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let relations = match data["relations"].as_array() {
        Some(r) => r,
        None => return None,
    };

    // Try direct Wikimedia Commons image relation
    if let Some(commons_page) = relations.iter().find_map(|r| {
        if r["type"].as_str() == Some("image") {
            r["url"]["resource"].as_str().map(|s| s.to_string())
        } else {
            None
        }
    }) {
        if let Some(filename) = commons_page.rsplit("File:").next() {
            let direct_url = format!(
                "https://commons.wikimedia.org/wiki/Special:Redirect/file/{}?width=500",
                filename.replace(' ', "_")
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let Some(bytes) = download_image(client, &direct_url).await {
                return Some(bytes);
            }
        }
    }

    // Fallback: Wikidata relation → P18 image → Wikimedia Commons
    let wikidata_url = relations.iter().find_map(|r| {
        if r["type"].as_str() == Some("wikidata") {
            r["url"]["resource"].as_str().map(|s| s.to_string())
        } else {
            None
        }
    })?;
    let qid = wikidata_url.rsplit('/').next()?;
    if !qid.starts_with('Q') {
        return None;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    fetch_image_from_wikidata(client, qid).await
}

/// Resolve a Wikidata entity QID to an image via the P18 property.
async fn fetch_image_from_wikidata(client: &reqwest::Client, qid: &str) -> Option<Vec<u8>> {
    let url = format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let image_filename = data
        .pointer(&format!("/entities/{qid}/claims/P18"))
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|claim| claim.pointer("/mainsnak/datavalue/value"))
        .and_then(|v| v.as_str())?;
    let direct_url = format!(
        "https://commons.wikimedia.org/wiki/Special:Redirect/file/{}?width=500",
        image_filename.replace(' ', "_")
    );
    download_image(client, &direct_url).await
}

/// Deux noms d'artiste désignent-ils la MÊME entrée ? À la casse et aux
/// espaces de bord près, rien d'autre.
///
/// C'est le pendant serveur de `trouverArtisteExact` (client web,
/// `libraryNavigation.ts`), et il obéit à la même règle : on ne replie NI les
/// accents NI la ponctuation. « Motorhead » et « Motörhead » sont deux entrées
/// distinctes ; les confondre poserait la mauvaise image.
fn artist_name_matches_exactly(candidate: &str, wanted: &str) -> bool {
    let candidate = candidate.trim();
    let wanted = wanted.trim();
    // Un nom vide se « retrouve » partout : il ne départage rien.
    !candidate.is_empty() && !wanted.is_empty() && candidate.to_lowercase() == wanted.to_lowercase()
}

/// Retire le désambiguïsateur numérique que Discogs accole aux homonymes :
/// « Marquis De Sade (2) » → « Marquis De Sade ».
///
/// Il sert UNIQUEMENT à repérer que Discogs déclare plusieurs entités du même
/// nom — pas à les rendre équivalentes.
fn strip_discogs_disambiguator(title: &str) -> &str {
    let t = title.trim_end();
    let Some(rest) = t.strip_suffix(')') else {
        return title.trim();
    };
    let Some(open) = rest.rfind(" (") else {
        return title.trim();
    };
    let inside = &rest[open + 2..];
    if !inside.is_empty() && inside.bytes().all(|b| b.is_ascii_digit()) {
        rest[..open].trim()
    } else {
        title.trim()
    }
}

/// Choisit l'image de couverture d'une réponse `database/search?type=artist`.
///
/// Deux refus, et dans les deux cas on ne pose AUCUNE image (#2221) :
///
/// 1. **Aucun résultat ne porte le nom cherché.** Prendre le premier venu — ce
///    que faisait `per_page=1` + `results[0]` — donne l'entrée la plus
///    populaire chez Discogs, pas l'artiste de la bibliothèque.
/// 2. **Discogs déclare lui-même un homonyme** en numérotant une seconde
///    entrée du même nom (« Marquis De Sade » / « Marquis De Sade (2) » : le
///    personnage historique et le groupe rennais). Rien ne permet alors de
///    trancher, et une image fausse s'installe durablement — l'utilisateur
///    peut la signaler, mais la passe d'enrichissement suivante la reposerait.
fn discogs_pick_cover_image<'a>(data: &'a serde_json::Value, artist_name: &str) -> Option<&'a str> {
    let results = data.get("results")?.as_array()?;
    let mut exact = results.iter().filter(|r| {
        r.get("title")
            .or_else(|| r.get("name"))
            .and_then(|v| v.as_str())
            .is_some_and(|t| {
                artist_name_matches_exactly(strip_discogs_disambiguator(t), artist_name)
            })
    });
    let chosen = exact.next()?;
    if exact.next().is_some() {
        return None;
    }
    chosen
        .get("cover_image")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && !s.contains("spacer.gif"))
}

/// Choisit l'URL d'image d'une réponse `artist.getinfo` de Last.fm.
///
/// `artist.getinfo` répond sur un simple nom et peut rediriger vers un autre
/// artiste. On exige donc que le nom RENDU soit celui demandé (#2221) : sans
/// ce recoupement, rien ne départage et on ne pose aucune image.
fn lastfm_pick_image_url<'a>(data: &'a serde_json::Value, artist_name: &str) -> Option<&'a str> {
    let returned = data.pointer("/artist/name").and_then(|v| v.as_str())?;
    if !artist_name_matches_exactly(returned, artist_name) {
        return None;
    }
    let images = data.pointer("/artist/image").and_then(|v| v.as_array())?;
    ["mega", "extralarge", "large"].iter().find_map(|&size| {
        images.iter().find_map(|img| {
            if img.get("size").and_then(|v| v.as_str())? != size {
                return None;
            }
            img.get("#text").and_then(|v| v.as_str()).filter(|url| {
                !url.is_empty()
                    && !url.contains("/noimage/")
                    && !url.contains("2a96cbd8b46e442fc41c2b86b821562f")
            })
        })
    })
}

/// Fetch artist image from Discogs by searching the artist name.
async fn fetch_artist_image_discogs(
    client: &reqwest::Client,
    artist_name: &str,
    token: Option<&str>,
) -> Option<Vec<u8>> {
    // Prefer the token passed by the caller (resolved from DB settings — where
    // the UI stores it), falling back to the environment. Previously this read
    // env only, so a Discogs token configured in the app never applied and no
    // artist images were fetched (Progman).
    let token = token
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TUNE_DISCOGS_TOKEN").ok())
        .or_else(|| std::env::var("DISCOGS_TOKEN").ok())?;
    if token.is_empty() {
        return None;
    }
    let resp = client
        .get("https://api.discogs.com/database/search")
        // Dix résultats, pas un seul : avec `per_page=1` on ne PEUT pas voir
        // qu'un homonyme existe, et on repart avec l'entrée la plus populaire.
        .query(&[("type", "artist"), ("per_page", "10"), ("q", artist_name)])
        .header("Authorization", format!("Discogs token={token}"))
        .header("User-Agent", "TuneServer/1.0")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let Some(cover_url) = discogs_pick_cover_image(&data, artist_name) else {
        debug!(
            artist = artist_name,
            "artist_image_discogs_no_unambiguous_match"
        );
        return None;
    };
    download_image(client, cover_url).await
}

/// Fetch artist image from Last.fm using the `artist.getinfo` endpoint.
///
/// The response contains an `image` array with sizes: small, medium, large,
/// extralarge, mega. We prefer "mega" first, then "extralarge".
async fn fetch_artist_image_lastfm(client: &reqwest::Client, artist_name: &str) -> Option<Vec<u8>> {
    let api_key = std::env::var("TUNE_LASTFM_API_KEY")
        .or_else(|_| std::env::var("LASTFM_API_KEY"))
        .or_else(|_| std::env::var("TUNE_LASTFM_KEY"))
        .ok()?;
    if api_key.is_empty() {
        return None;
    }
    let resp = client
        .get("https://ws.audioscrobbler.com/2.0/")
        .query(&[
            ("method", "artist.getinfo"),
            ("artist", artist_name),
            ("api_key", &api_key),
            ("format", "json"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let Some(image_url) = lastfm_pick_image_url(&data, artist_name) else {
        debug!(artist = artist_name, "artist_image_lastfm_no_exact_match");
        return None;
    };
    let image_url = image_url.to_string();

    download_image(client, &image_url).await
}

async fn download_image(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    // Single choke point for every artwork/artist-image download: classify the
    // failure via the shared `FetchOutcome` so an enrichment run that "finds
    // nothing" can be diagnosed — rate-limit vs genuinely absent vs network —
    // instead of every failure being an indistinguishable `None` (#1096).
    // Behaviour is unchanged: all failure cases still return `None`.
    match crate::http::fetch::fetch_bytes(client, url, 1000).await {
        crate::http::fetch::FetchOutcome::Success(bytes) => Some(bytes),
        crate::http::fetch::FetchOutcome::RateLimited => {
            // Counter read per-run by the enrichment batch to report throttling.
            ARTWORK_RATE_LIMIT_HITS.fetch_add(1, Ordering::Relaxed);
            warn!(url, "artwork_download_rate_limited");
            None
        }
        other => {
            debug!(url, reason = other.reason(), "artwork_download_failed");
            None
        }
    }
}

/// Run batch artist image enrichment for all artists with an MBID but no image.
///
/// Whether an artist's recorded artwork actually exists (so enrichment can
/// re-fetch when the DB claims an image but the cache file is gone).
/// A remote `http(s)` `image_path` is served by redirect, so treat it as
/// present. Local paths are cache hashes → probe both `.jpg` and `.png`.
/// Whether the artwork referenced by `image_path` exists **as a local cache
/// file**. A remote `http(s)` URL counts as NOT cached: streaming services
/// (Tidal, Deezer, Amazon) store the artist picture as a remote URL, which
/// leaves no local file, is served only as a redirect that many renderers/
/// clients can't load, and blocks enrichment from ever caching a real image
/// (Fabien: full scan + Tidal premium, artwork_cache empty, no artist images).
/// Returning false for URLs makes enrichment localize them into the cache.
/// Also lets callers detect a stale DB `image_path` whose cache file is gone
/// (moved/wiped `artwork_cache`).
pub fn cached_artwork_exists(cache_dir: &std::path::Path, image_path: &str) -> bool {
    if image_path.starts_with("http") {
        return false;
    }
    cache_dir.join(format!("{image_path}.jpg")).exists()
        || cache_dir.join(format!("{image_path}.png")).exists()
}

/// Nom de la passe « par nom » (phase 3) dans le réglage
/// `artist_artwork_enrich_result`, aux côtés de `mbid` et `images`.
///
/// Publique parce que `tune-server` la traduit en libellé affiché. Deux
/// chaînes recopiées de part et d'autre se seraient désaccordées en silence, et
/// l'écran aurait annoncé « MusicBrainz » pendant que la passe interroge
/// Discogs et Last.fm.
pub const PHASE_PAR_NOM: &str = "names";

/// Cadence de publication de l'avancement : une écriture toutes les cinq
/// fiches, et toujours sur la dernière.
///
/// Les trois passes appliquaient déjà cette règle, chacune avec sa propre copie
/// de la condition. Une seule définition désormais — et le `traites == total`
/// n'est pas décoratif : sans lui, un lot de douze cesserait d'afficher à dix.
pub(crate) fn doit_publier_avancement(traites: usize, total: usize) -> bool {
    traites % 5 == 0 || traites == total
}

/// Instantané d'avancement de la passe 3 — recherche d'image **par nom**
/// (Discogs puis Last.fm), pour les artistes qui n'ont toujours ni MBID ni
/// image après les passes 1 et 2.
///
/// Cette passe ne publiait **rien** : ni `processed`, ni `total`, ni `phase`.
/// Le dernier état écrit restait donc celui de la fin de passe 2 —
/// `processed == total`, soit 100 % — pendant toute sa durée, puis la tâche
/// disparaissait. Or sur une bibliothèque non étiquetée c'est la seule passe
/// qui travaille : le compteur montait jusqu'au total, se figeait, et la
/// fenêtre se refermait sans qu'aucun bilan intermédiaire n'ait été dit
/// (#2227 Jean Valjean ; #2257 Sandro, 350 artistes sans MBID).
pub(crate) fn avancement_par_nom(
    traites: usize,
    total: usize,
    discogs_enriched: u32,
    lastfm_enriched: u32,
) -> serde_json::Value {
    serde_json::json!({
        "status": "running",
        "phase": PHASE_PAR_NOM,
        "processed": traites,
        "total": total,
        // `enriched` est le champ que les deux autres passes publient et que
        // l'écran lit : il doit compter les images posées par CETTE passe.
        "enriched": discogs_enriched + lastfm_enriched,
        "discogs_enriched": discogs_enriched,
        "lastfm_enriched": lastfm_enriched,
    })
}

/// Source ayant effectivement posé l'image d'un artiste cherché par nom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceParNom {
    Discogs,
    Lastfm,
}

/// Le corps de la passe 3, séparé de ses accès réseau.
///
/// La publication d'avancement vit ici, donc un test peut l'observer sans
/// appeler le moindre service d'images : `poser_image` porte à elle seule les
/// requêtes Discogs / Last.fm et l'écriture en cache.
pub(crate) async fn passe_par_nom<F, Fut>(
    settings: &crate::db::settings_repo::SettingsRepo,
    artistes: &[(i64, String)],
    mut poser_image: F,
) -> (u32, u32)
where
    F: FnMut(i64, String) -> Fut,
    Fut: std::future::Future<Output = Option<SourceParNom>>,
{
    let total = artistes.len();
    let publier = |traites: usize, discogs: u32, lastfm: u32| {
        settings
            .set(
                "artist_artwork_enrich_result",
                &avancement_par_nom(traites, total, discogs, lastfm).to_string(),
            )
            .ok();
    };

    // Basculer l'affichage sur cette passe DÈS son démarrage. Sinon l'écran
    // reste sur le 100 % de la passe 2 le temps des cinq premières fiches,
    // c'est-à-dire précisément le compteur figé qui a été signalé.
    publier(0, 0, 0);

    let mut discogs_enriched = 0u32;
    let mut lastfm_enriched = 0u32;
    for (index, (artist_id, name)) in artistes.iter().enumerate() {
        match poser_image(*artist_id, name.clone()).await {
            Some(SourceParNom::Discogs) => discogs_enriched += 1,
            Some(SourceParNom::Lastfm) => lastfm_enriched += 1,
            None => {}
        }
        let traites = index + 1;
        if doit_publier_avancement(traites, total) {
            publier(traites, discogs_enriched, lastfm_enriched);
        }
    }
    (discogs_enriched, lastfm_enriched)
}

/// Phase 1: Check community-approved images from mozaiklabs.fr first.
/// Phase 2: For remaining artists, fetch from mozaiklabs API / Fanart.tv / MusicBrainz,
/// then submit discovered images back to the community (fire-and-forget).
///
/// Respects rate limit: ~1 request/second.
pub async fn batch_enrich_artist_artwork(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
) {
    batch_enrich_artist_artwork_inner(db, cache_dir, false, None).await
}

/// Variante à portée (#1660) : seuls les artistes de la portée sont candidats
/// (phases communautaire ET sources externes). `None` = passe complète.
pub async fn batch_enrich_artist_artwork_scoped(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
    scope: Option<crate::metadata::enrich_scope::EnrichScope>,
) {
    batch_enrich_artist_artwork_inner(db, cache_dir, false, scope).await
}

/// Force variant: re-fetch artwork for EVERY artist with an MBID, ignoring the
/// "already has an image" guard. Fixes libraries where `image_path` is set to
/// stale/broken entries that never render (Fabien: full scan + premium, still
/// no artist images — the normal pass skips because the DB claims images exist).
pub async fn batch_refetch_artist_artwork(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
) {
    batch_enrich_artist_artwork_inner(db, cache_dir, true, None).await
}

async fn batch_enrich_artist_artwork_inner(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: PathBuf,
    force: bool,
    scope: Option<crate::metadata::enrich_scope::EnrichScope>,
) {
    let artist_repo = crate::db::artist_repo::ArtistRepo::with_backend(db.clone());
    // Snapshot the global rate-limit counter so the result can report how many
    // downloads THIS run had throttled (429/503) — a "found nothing" run with a
    // high count is retryable, not genuinely empty (#1096).
    let rl_start = ARTWORK_RATE_LIMIT_HITS.load(Ordering::Relaxed);

    // --- Phase 1: Bulk-apply community-approved artist images ---
    let mut community_applied = 0u32;
    if let Ok(approved) =
        crate::cloud::community::fetch_approved_artist_images("https://mozaiklabs.fr", None).await
    {
        for img in &approved {
            // Check if this artist is in our DB and still needs an image.
            // Gate on the cache file actually existing, not just the DB column:
            // a scan can set image_path while the cache write failed, leaving a
            // grey square that would otherwise be skipped forever (Sandro).
            if let Ok(Some(artist)) = artist_repo.get_by_musicbrainz_id(&img.mbid) {
                // In force mode, re-apply even if the DB claims a cached image
                // (the point is to overwrite stale/broken entries).
                if !force
                    && artist
                        .image_path
                        .as_deref()
                        .is_some_and(|ip| cached_artwork_exists(&cache_dir, ip))
                {
                    continue;
                }
                let artist_id = match artist.id {
                    Some(id) => id,
                    None => continue,
                };
                // Portée par répertoire (#1660) : la passe communautaire ne
                // pose rien sur un artiste hors du répertoire demandé.
                if scope
                    .as_ref()
                    .is_some_and(|s| !s.contient_artiste(artist_id))
                {
                    continue;
                }
                let client = crate::http::client::builder()
                    .user_agent(MB_USER_AGENT)
                    .timeout(std::time::Duration::from_secs(15))
                    .build();
                if let Ok(client) = client {
                    if let Some(data) = download_image(&client, &img.image_url).await {
                        // Adressage par le CONTENU (#1444) : sous
                        // `artwork_hash("artist-mbid-{mbid}")`, le mode `force`
                        // — dont c'est tout l'objet — réécrivait sous l'adresse
                        // déjà distribuée, servie `immutable, max-age=31536000` :
                        // l'ancienne photo restait affichée un an.
                        std::fs::create_dir_all(&cache_dir).ok();
                        if let Some(hash) = cache_fetched_image(&data, &cache_dir, "jpg") {
                            artist_repo.update_image(artist_id, &hash, "community").ok();
                            community_applied += 1;
                            info!(
                                artist_id,
                                artist = %img.artist_name,
                                hash = %hash,
                                "batch_artist_artwork_community_applied"
                            );
                        }
                    }
                }
            }
        }
        if community_applied > 0 {
            info!(
                community_applied,
                "batch_artist_artwork_community_phase_done"
            );
        }
    }

    // --- Phase 2: Fetch from external sources ---
    // Force mode re-fetches EVERY artist (overwriting stale entries), including
    // those without an MBID — mozaiklabs-by-name + other by-name sources can
    // still find them. Normal mode only targets artists without an image.
    let mut artists = match if force {
        artist_repo.list_all_id_name_mbid()
    } else {
        artist_repo.list_without_image()
    } {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_artist_artwork_list_failed");
            return;
        }
    };
    if force {
        info!(count = artists.len(), "batch_artist_artwork_force_refetch");
    }

    // Re-queue artists whose image_path is set in the DB but whose cache file is
    // actually missing. list_without_image only checks the column, so a scan
    // that set image_path while the cache write failed (or a cache that was
    // later cleared/moved) leaves a grey square that would be skipped forever
    // (Fabien: "j'ai pas les images d'artistes" despite a full scan + premium).
    // This extends the Phase-1 cache-existence guard (Sandro) to Phase 2.
    // Skipped in force mode, which already includes every MBID artist.
    if !force {
        match artist_repo.list_with_image_and_mbid() {
            Ok(with_image) => {
                let before = artists.len();
                for (id, name, mbid, image_path) in with_image {
                    if !cached_artwork_exists(&cache_dir, &image_path) {
                        artists.push((id, name, mbid));
                    }
                }
                let requeued = artists.len() - before;
                if requeued > 0 {
                    info!(requeued, "batch_artist_artwork_missing_cache_requeued");
                }
            }
            Err(e) => warn!(error = %e, "batch_artist_artwork_with_image_list_failed"),
        }
    }

    // Artists WITHOUT an MBID are excluded by every list above (they all
    // require musicbrainz_id), yet the enrichment loop below can resolve an
    // MBID from the name and fall back to by-name image sources. Untagged
    // artists therefore stayed grey forever in normal mode — the whole batch
    // even reported "all have images" while these were simply never looked at
    // (Fabien: 171/1183 artists unmatched, Tidal premium, grey squares).
    // Include (a) those with no image at all, and (b) those whose stored
    // image is a remote URL / missing cache file (Tidal stores unusable
    // remote URLs), so the loop can localize a real picture. (Port of #769.)
    if !force {
        let before_no_mbid = artists.len();
        match artist_repo.list_without_image_no_mbid() {
            Ok(no_img) => {
                for (id, name) in no_img {
                    artists.push((id, name, String::new()));
                }
            }
            Err(e) => warn!(error = %e, "batch_artist_artwork_no_mbid_list_failed"),
        }
        match artist_repo.list_with_image_no_mbid() {
            Ok(with_img) => {
                for (id, name, image_path) in with_img {
                    if !cached_artwork_exists(&cache_dir, &image_path) {
                        artists.push((id, name, String::new()));
                    }
                }
            }
            Err(e) => warn!(error = %e, "batch_artist_artwork_with_image_no_mbid_list_failed"),
        }
        let added_no_mbid = artists.len() - before_no_mbid;
        if added_no_mbid > 0 {
            info!(added_no_mbid, "batch_artist_artwork_no_mbid_included");
        }
    }

    // Portée par répertoire (#1660) : l'intersection se fait APRÈS toutes les
    // additions ci-dessus (requeues cache manquant, sans-MBID), pour que la
    // portée s'applique à la sélection complète des candidats.
    if let Some(scope) = &scope {
        let avant = artists.len();
        artists.retain(|(id, ..)| scope.contient_artiste(*id));
        info!(
            dir = %scope.dir,
            retained = artists.len(),
            dropped = avant - artists.len(),
            "batch_artist_artwork_scope_applied"
        );
    }

    if artists.is_empty() {
        info!("batch_artist_artwork_skip_all_have_images");
        // Store result even when nothing to fetch
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(db);
        settings
            .set(
                "artist_artwork_enrich_result",
                &serde_json::json!({
                    "status": "done",
                    "phase": "done",
                    "total": 0,
                    "enriched": 0,
                    "failed": 0,
                    "community_applied": community_applied,
                })
                .to_string(),
            )
            .ok();
        return;
    }

    info!(
        count = artists.len(),
        "batch_artist_artwork_enrichment_started"
    );

    // Get instance_id for community submissions
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    // Consentement explicite avant de rien remonter au cloud communautaire.
    // Le seul garde-fou etait « avoir un instance_id » — or il est genere tout
    // seul au demarrage, donc l'envoi des images d'artistes etait en pratique
    // inconditionnel. Lu ICI, une fois, et non a chaque artiste : la boucle
    // dure des heures et le choix de l'utilisateur au moment ou il lance
    // l'enrichissement est celui qui fait foi pour cette passe.
    let contribution_consentie = crate::cloud::consent::contribution_autorisee(&settings);
    // Discogs token as configured in the app UI (stored in settings), so the
    // by-name Discogs image lookup actually works (Progman: no artist images).
    let discogs_token = settings
        .get("discogs_token")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("TUNE_DISCOGS_TOKEN")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            std::env::var("DISCOGS_TOKEN")
                .ok()
                .filter(|s| !s.is_empty())
        });

    let mut enriched = 0u32;
    let mut failed = 0u32;
    let total_images = artists.len();

    for (i, (artist_id, name, mbid)) in artists.iter().enumerate() {
        // Rate limit: short delay between community lookups (no rate limit),
        // longer delay only when hitting external APIs (MusicBrainz etc.)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Resolve a MusicBrainz ID from the artist name when the files carried
        // no MB tag. Without it the rich image sources (Fanart/TheAudioDB/
        // MusicBrainz) and community matching can't find this artist — the whole
        // reason untagged libraries end up with almost no artist images. Persist
        // it so future runs and community lookups reuse it. MB asks for ~1 req/s,
        // so only pay that extra delay for artists we actually have to look up.
        let mut mbid = mbid.clone();
        if mbid.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            if let Some(found) = search_musicbrainz_artist(name).await {
                artist_repo.update_mbid(*artist_id, &found).ok();
                info!(artist_id, artist = %name, mbid = %found, "batch_artist_artwork_mbid_resolved");
                mbid = found;
            }
        }

        match fetch_artist_image(&mbid, name, discogs_token.as_deref()).await {
            Some(data) => {
                // Adressage par le CONTENU (#1444), plus par l'identité de
                // l'artiste. L'ancienne clé était `artist-mbid-{mbid}`, sinon
                // `artist-name-{nom}` — et sa forme précédente, un
                // `artist-mbid-` à MBID VIDE, avait déjà fait converger TOUS
                // les artistes sans MBID sur `md5("artist-mbid-")` : Keith
                // Jarrett, Duke Ellington… partageaient une seule photo, chacun
                // écrasant celle du précédent. Le passage par le nom a réduit
                // la famille de collisions sans la fermer (deux artistes
                // homonymes restent une seule adresse), et le mode `force`
                // réécrivait sous une adresse servie `immutable` un an. Le
                // condensat des octets ferme les deux.
                std::fs::create_dir_all(&cache_dir).ok();
                if let Some(hash) = cache_fetched_image(&data, &cache_dir, "jpg") {
                    artist_repo.update_image(*artist_id, &hash, "auto").ok();
                    enriched += 1;
                    info!(
                        artist_id,
                        artist = %name,
                        hash = %hash,
                        size = data.len(),
                        "batch_artist_artwork_enriched"
                    );

                    // Fire-and-forget: submit to community for sharing.
                    // Seulement si l'utilisateur l'a explicitement autorise, et
                    // seulement quand on a un MBID — le depot communautaire est
                    // indexe par MBID, y poster avec un MBID vide n'a aucun sens.
                    if contribution_consentie && !instance_id.is_empty() && !mbid.is_empty() {
                        let mbid = mbid.clone();
                        let name = name.clone();
                        let instance_id = instance_id.clone();
                        let image_data = data.clone();
                        tokio::spawn(async move {
                            if let Err(e) = crate::cloud::community::submit_artist_image(
                                "https://mozaiklabs.fr",
                                &mbid,
                                &name,
                                &instance_id,
                                &image_data,
                            )
                            .await
                            {
                                debug!(mbid = %mbid, error = %e, "community_artist_image_submit_failed");
                            }
                        });
                    }
                } else {
                    failed += 1;
                    warn!(artist_id, artist = %name, "batch_artist_artwork_save_failed");
                }
            }
            None => {
                failed += 1;
                debug!(artist_id, artist = %name, mbid = %mbid, "batch_artist_artwork_not_found");
            }
        }

        // Publish live progress for the UI (Fabien: enrichment looked frozen).
        if doit_publier_avancement(i + 1, total_images) {
            settings
                .set(
                    "artist_artwork_enrich_result",
                    &serde_json::json!({
                        "status": "running",
                        "phase": "images",
                        "processed": i + 1,
                        "total": total_images,
                        "enriched": enriched,
                        "failed": failed,
                        "community_applied": community_applied,
                        "rate_limit_hits":
                            ARTWORK_RATE_LIMIT_HITS.load(Ordering::Relaxed).saturating_sub(rl_start),
                    })
                    .to_string(),
                )
                .ok();
        }
    }

    info!(
        total = artists.len(),
        enriched, failed, community_applied, "batch_artist_artwork_phase2_complete"
    );

    // --- Phase 3: Try Discogs + Last.fm by name for artists without MBID and without image ---
    let mut discogs_enriched = 0u32;
    let mut lastfm_enriched = 0u32;
    let discogs_available = discogs_token.is_some();
    let lastfm_available = std::env::var("TUNE_LASTFM_API_KEY")
        .or_else(|_| std::env::var("LASTFM_API_KEY"))
        .or_else(|_| std::env::var("TUNE_LASTFM_KEY"))
        .map(|t| !t.is_empty())
        .unwrap_or(false);

    if discogs_available || lastfm_available {
        let no_mbid_artists = match artist_repo.list_without_image_no_mbid() {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, "batch_artist_artwork_no_mbid_list_failed");
                Vec::new()
            }
        };

        if !no_mbid_artists.is_empty() {
            info!(
                count = no_mbid_artists.len(),
                "batch_artist_artwork_phase3_started"
            );
            let client = crate::http::client::builder()
                .user_agent(MB_USER_AGENT)
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default();

            // Le parcours et la publication d'avancement sont dans
            // `passe_par_nom` ; ne reste ici que ce qui touche le réseau et le
            // cache. Le `continue` d'antan — Discogs a posé, on saute Last.fm —
            // devient le retour anticipé de cette fermeture.
            let (d, l) = passe_par_nom(&settings, &no_mbid_artists, |artist_id, name| {
                let client = &client;
                let cache_dir = &cache_dir;
                let artist_repo = &artist_repo;
                let discogs_token = discogs_token.as_deref();
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                    // Try Discogs first
                    if discogs_available {
                        if let Some(data) =
                            fetch_artist_image_discogs(client, &name, discogs_token).await
                        {
                            // Adressage par le CONTENU (#1444) : deux artistes
                            // homonymes ne partagent plus une seule adresse.
                            std::fs::create_dir_all(cache_dir).ok();
                            if let Some(hash) = cache_fetched_image(&data, cache_dir, "jpg") {
                                artist_repo.update_image(artist_id, &hash, "discogs").ok();
                                info!(artist_id, artist = %name, "batch_artist_artwork_discogs_enriched");
                                return Some(SourceParNom::Discogs);
                            }
                        }
                    }

                    // Fallback to Last.fm
                    if lastfm_available {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        if let Some(data) = fetch_artist_image_lastfm(client, &name).await {
                            // Adressage par le CONTENU (#1444), même raison
                            // qu'au passage Discogs juste au-dessus.
                            std::fs::create_dir_all(cache_dir).ok();
                            if let Some(hash) = cache_fetched_image(&data, cache_dir, "jpg") {
                                artist_repo.update_image(artist_id, &hash, "lastfm").ok();
                                info!(artist_id, artist = %name, "batch_artist_artwork_lastfm_enriched");
                                return Some(SourceParNom::Lastfm);
                            }
                        }
                    }
                    None
                }
            })
            .await;
            discogs_enriched = d;
            lastfm_enriched = l;
            info!(
                discogs_enriched,
                lastfm_enriched,
                total = no_mbid_artists.len(),
                "batch_artist_artwork_phase3_complete"
            );
        }
    }

    let total_enriched = enriched + discogs_enriched + lastfm_enriched;
    info!(
        total_enriched,
        phase2_enriched = enriched,
        phase3_discogs = discogs_enriched,
        phase3_lastfm = lastfm_enriched,
        community_applied,
        "batch_artist_artwork_enrichment_complete"
    );

    // Store result in settings for status reporting
    settings
        .set(
            "artist_artwork_enrich_result",
            &serde_json::json!({
                "status": "done",
                "phase": "done",
                "total": artists.len(),
                "enriched": total_enriched,
                "phase2_enriched": enriched,
                "phase3_discogs": discogs_enriched,
                "phase3_lastfm": lastfm_enriched,
                "failed": failed,
                "community_applied": community_applied,
                // How many downloads were rate-limited during this run: lets the
                // UI say "N restants dont X à réessayer" instead of implying the
                // artists are simply absent (#1096).
                "rate_limit_hits":
                    ARTWORK_RATE_LIMIT_HITS.load(Ordering::Relaxed).saturating_sub(rl_start),
            })
            .to_string(),
        )
        .ok();
}

/// Save cover art bytes that were **already read during the metadata pass** into
/// the artwork cache, without re-opening the audio file.
///
/// `get_or_extract` opens the file a *second* time through lofty to pull the
/// embedded picture. On some Windows setups that second open fails with
/// `os error 3` (path not found) on accented paths even though the metadata read
/// moments earlier succeeded — so the album ends up with no cover although its
/// tags carry one (Thibaud). When the scan already has the cover bytes from the
/// metadata read, call this instead: same cache key as `get_or_extract`, so a
/// later `get_or_extract` on the same file is a cache hit.
pub fn save_embedded_cover(
    audio_path: &Path,
    cache_dir: &Path,
    cover: &(Vec<u8>, String),
) -> Option<String> {
    // Entrée héritée, adressée par le CHEMIN de la piste : la sonder d'abord
    // garde les URL déjà distribuées valables (la route sert `immutable,
    // max-age=31536000`) et évite tout travail sur un rescan. La sonde
    // interroge la MÊME liste que la route. Tant qu'elle ne regardait que
    // `jpg`/`png`, une entrée héritée (`.jpeg`, `.JPG`, `.bmp`) passait pour
    // absente et était réécrite à chaque passe (#2567).
    let legacy = artwork_hash(&audio_path.to_string_lossy());
    if find_cached(cache_dir, &legacy).is_some() {
        return Some(legacy);
    }

    let (data, mime) = cover;
    // Nouvelle écriture : adressée par le CONTENU (#1444). Mêmes octets dans
    // N fichiers = une seule entrée, et un rescan retombe sur elle sans rien
    // réécrire.
    let hash = content_hash(data);
    if find_cached(cache_dir, &hash).is_some() {
        return Some(hash);
    }
    let ext = if mime.contains("png") {
        "png"
    } else if mime.contains("bmp") {
        "bmp"
    } else {
        "jpg"
    };
    if save_to_cache(data, cache_dir, &hash, ext).is_some() {
        return Some(hash);
    }
    warn!(
        path = %audio_path.display(),
        cache_dir = %cache_dir.display(),
        "embedded_cover_from_tag_save_failed"
    );
    None
}

/// Pochette du DOSSIER d'un fichier, mise en cache — sans jamais regarder les
/// tags du fichier lui-même.
///
/// Sert à choisir la pochette d'un ALBUM, où l'ordre de priorité n'est pas le
/// même que pour une piste. Une `cover.jpg` posée dans le dossier est un choix
/// délibéré de l'utilisateur ; une pochette intégrée à UNE piste est un
/// accident de tag. Sur une compilation « maison », c'est la seconde qui
/// gagnait et se retrouvait attribuée à tout le répertoire (testeur, forum).
pub fn folder_cover_hash(audio_path: &Path, cache_dir: &Path) -> Option<String> {
    let folder_cover = find_folder_cover(audio_path)?;
    // Entrée héritée, adressée par le CHEMIN de la pochette : la sonder
    // d'abord garde les URL déjà distribuées valables et épargne la lecture du
    // fichier sur un rescan.
    let legacy = artwork_hash(&folder_cover.to_string_lossy());
    if find_cached(cache_dir, &legacy).is_some() {
        return Some(legacy);
    }
    let data = std::fs::read(&*extended_path(&folder_cover)).ok()?;
    // Nouvelle écriture : adressée par le CONTENU (#1444). La même `cover.jpg`
    // recopiée dans N dossiers d'artiste (compilation éclatée façon Qobuz,
    // #1440) ne peuple plus le cache que d'UNE entrée.
    let hash = content_hash(&data);
    if find_cached(cache_dir, &hash).is_some() {
        return Some(hash);
    }
    let ext = folder_cover
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg");
    save_to_cache(&data, cache_dir, &hash, ext).map(|_| hash)
}

/// Noms acceptés pour une photo d'ARTISTE posée à côté des pistes.
///
/// Reprend à l'identique la liste que l'import parcourait en ligne.
pub const FOLDER_ARTIST_IMAGE_NAMES: &[&str] =
    &["artist.jpg", "artist.png", "Artist.jpg", "Artist.png"];

/// Photo d'ARTISTE posée dans le dossier des pistes (`artist.jpg`), mise en
/// cache, adressée par son CONTENU (#1444).
///
/// C'est littéralement le défaut que nomme le titre du ticket : l'adresse était
/// `artwork_hash(chemin du fichier)`. La même `artist.jpg` recopiée dans les N
/// dossiers d'album d'un artiste — ce que font tous les extracteurs de
/// bibliothèque — produisait **N entrées de cache** pour une seule photo, et le
/// moindre déplacement du dossier en fabriquait une de plus en laissant
/// l'ancienne orpheline.
///
/// Sonde d'abord l'entrée héritée, adressée par le chemin : une URL déjà
/// distribuée reste valable (la route sert `immutable, max-age=31536000`) et un
/// rescan ne relit pas le fichier. Même contrat que [`folder_cover_hash`].
pub fn folder_artist_image_hash(audio_path: &Path, cache_dir: &Path) -> Option<String> {
    let parent = audio_path.parent()?;
    for name in FOLDER_ARTIST_IMAGE_NAMES {
        let candidate = parent.join(name);
        if !candidate.exists() {
            continue;
        }
        // Entrée héritée, adressée par le CHEMIN de l'image.
        let legacy = artwork_hash(&candidate.to_string_lossy());
        if find_cached(cache_dir, &legacy).is_some() {
            return Some(legacy);
        }
        let Ok(data) = std::fs::read(&*extended_path(&candidate)) else {
            debug!(path = %candidate.display(), "folder_artist_image_read_failed");
            continue;
        };
        let ext = candidate
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jpg");
        // Nouvelle écriture : adressée par le CONTENU.
        if let Some(hash) = cache_fetched_image(&data, cache_dir, ext) {
            return Some(hash);
        }
        warn!(
            path = %candidate.display(),
            cache_dir = %cache_dir.display(),
            "folder_artist_image_cache_write_failed"
        );
    }
    None
}

pub fn get_or_extract(audio_path: &Path, cache_dir: &Path) -> Option<String> {
    // Entrée héritée, adressée par le CHEMIN de la piste — même liste que la
    // route (#2567). La sonder d'abord garde les URL déjà distribuées valables
    // et évite de rouvrir le fichier audio sur un rescan.
    let legacy = artwork_hash(&audio_path.to_string_lossy());
    if find_cached(cache_dir, &legacy).is_some() {
        return Some(legacy);
    }

    // Try embedded cover art from the audio file tags.
    // Nouvelle écriture : adressée par le CONTENU (#1444) — la même jaquette
    // intégrée à N pistes ne peuple le cache que d'UNE entrée.
    if let Some((data, mime)) = extract_cover_art(audio_path) {
        let hash = content_hash(&data);
        if find_cached(cache_dir, &hash).is_some() {
            return Some(hash);
        }
        let ext = if mime.contains("png") { "png" } else { "jpg" };
        if save_to_cache(&data, cache_dir, &hash, ext).is_some() {
            return Some(hash);
        }
        warn!(
            path = %audio_path.display(),
            cache_dir = %cache_dir.display(),
            "artwork_extracted_but_save_failed_trying_folder"
        );
    }

    // Try folder-level cover art (cover.jpg, folder.jpg, front.jpg, etc.).
    // Ici l'ancien schéma hachait le chemin de la PISTE : chaque piste du
    // dossier dupliquait la même pochette dans le cache. Le condensat de
    // contenu les fait toutes converger vers une seule entrée.
    if let Some(folder_cover) = find_folder_cover(audio_path) {
        match std::fs::read(&*extended_path(&folder_cover)) {
            Ok(data) => {
                let hash = content_hash(&data);
                if find_cached(cache_dir, &hash).is_some() {
                    return Some(hash);
                }
                let ext = folder_cover
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("jpg");
                if save_to_cache(&data, cache_dir, &hash, ext).is_some() {
                    debug!(
                        folder_cover = %folder_cover.display(),
                        "artwork_from_folder_cover"
                    );
                    return Some(hash);
                }
                warn!(
                    path = %folder_cover.display(),
                    cache_dir = %cache_dir.display(),
                    "folder_cover_read_but_save_failed"
                );
            }
            Err(e) => {
                debug!(
                    path = %folder_cover.display(),
                    error = %e,
                    "folder_cover_read_failed"
                );
            }
        }
    }

    None
}

/// Re-extract embedded cover art for local albums that still have no
/// `cover_path`, reading directly from their track files (never the network).
///
/// The incremental scan only extracts covers from files it actually
/// re-processes; unchanged files are skipped. So an improvement to embedded-art
/// extraction (e.g. DSF ID3v2 covers stored at the DSF metadata offset that
/// lofty ignores — Thibaud) never reaches a library whose files are unchanged.
/// Running this at the end of a scan self-heals those albums: any local album
/// with a missing cover gets its embedded art re-extracted from the first track
/// that yields one. Returns the number of albums filled.
pub fn backfill_embedded_covers(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    cache_dir: &Path,
) -> usize {
    use crate::db::album_repo::AlbumRepo;
    use crate::db::track_repo::TrackRepo;

    let album_repo = AlbumRepo::with_backend(db.clone());
    let track_repo = TrackRepo::with_backend(db.clone());
    let coverless = album_repo.list_without_cover().unwrap_or_default();

    let mut filled = 0usize;
    for (album_id, _title, _artist, _mbid) in &coverless {
        let tracks = track_repo.list_by_album(*album_id).unwrap_or_default();

        // La pochette du DOSSIER d'abord : elle décrit l'album, là où une
        // pochette intégrée ne décrit qu'une piste. Sur une compilation
        // « maison », la première piste taguée imposait sa jaquette à tout le
        // répertoire — un disque de Brel illustré par la pochette du seul titre
        // dont le fichier portait une image.
        if let Some(hash) = tracks
            .iter()
            .filter_map(|t| t.file_path.as_ref())
            .find_map(|p| folder_cover_hash(Path::new(p), cache_dir))
        {
            if album_repo.force_update_cover_path(*album_id, &hash).is_ok() {
                filled += 1;
            }
            continue;
        }

        for track in &tracks {
            let Some(ref file_path) = track.file_path else {
                continue;
            };
            if let Some(hash) = get_or_extract(Path::new(file_path), cache_dir) {
                if album_repo.force_update_cover_path(*album_id, &hash).is_ok() {
                    filled += 1;
                }
                break;
            }
        }
    }
    if filled > 0 {
        info!(filled, "backfill_embedded_covers_done");
    }
    filled
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // #2227 / #2257 — la passe PAR NOM doit publier son avancement.
    //
    // Elle n'écrivait rien du tout : l'écran restait sur le 100 % de la passe
    // précédente pendant tout son travail, puis la fenêtre se refermait. Sur
    // une bibliothèque non étiquetée, c'est pourtant la seule passe qui
    // travaille.
    // ---------------------------------------------------------------------

    fn base_neuve() -> std::sync::Arc<dyn crate::db::backend::DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        std::sync::Arc::new(db)
    }

    /// Toutes les cinq fiches, et toujours la dernière.
    #[test]
    fn la_cadence_publie_toutes_les_cinq_fiches_et_la_derniere() {
        let publiees: Vec<usize> = (1..=12)
            .filter(|t| doit_publier_avancement(*t, 12))
            .collect();
        assert_eq!(publiees, vec![5, 10, 12]);

        // Un lot plus petit que la cadence publie quand même : sinon un
        // enrichissement de trois artistes n'afficherait jamais rien.
        let petites: Vec<usize> = (1..=3).filter(|t| doit_publier_avancement(*t, 3)).collect();
        assert_eq!(petites, vec![3]);
    }

    /// L'instantané se distingue de celui de la passe 2, sans quoi le sondeur
    /// de la route afficherait « MusicBrainz » pendant une recherche Discogs.
    #[test]
    fn l_instantane_par_nom_porte_sa_phase_et_ses_compteurs() {
        let v = avancement_par_nom(7, 350, 2, 1);
        assert_eq!(v["status"], "running");
        assert_eq!(v["phase"], PHASE_PAR_NOM);
        assert_ne!(
            v["phase"], "images",
            "la passe par nom n'est pas la passe 2"
        );
        assert_eq!(v["processed"], 7);
        assert_eq!(v["total"], 350);
        assert_eq!(v["enriched"], 3);
    }

    /// LE cas signalé : douze artistes sans MBID, aucun service d'images
    /// appelé. On relit le réglage AVANT chaque fiche, ce qui donne la suite
    /// exacte de ce qu'un écran qui sonde aurait vu.
    #[tokio::test]
    async fn la_passe_par_nom_publie_son_avancement_fiche_apres_fiche() {
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(base_neuve());
        let artistes: Vec<(i64, String)> = (1..=12).map(|i| (i, format!("Artiste {i}"))).collect();

        // La passe 2 vient de finir : l'écran est à 12/12, phase « images ».
        settings
            .set(
                "artist_artwork_enrich_result",
                &serde_json::json!({
                    "status": "running", "phase": "images",
                    "processed": 12, "total": 12, "enriched": 0,
                })
                .to_string(),
            )
            .unwrap();

        let lu = |s: &crate::db::settings_repo::SettingsRepo| -> serde_json::Value {
            serde_json::from_str(&s.get("artist_artwork_enrich_result").unwrap().unwrap()).unwrap()
        };

        let vus = std::cell::RefCell::new(Vec::<serde_json::Value>::new());
        let (discogs, lastfm) = passe_par_nom(&settings, &artistes, |artist_id, _name| {
            vus.borrow_mut().push(lu(&settings));
            async move {
                // Un artiste sur quatre trouve une image. Aucun réseau.
                if artist_id % 4 == 0 {
                    Some(SourceParNom::Discogs)
                } else {
                    None
                }
            }
        })
        .await;

        let vus = vus.into_inner();
        assert_eq!(vus.len(), 12);

        // Avant la toute première fiche, l'écran a DÉJÀ quitté le 100 % de la
        // passe 2 : c'est le compteur figé qui a été signalé.
        assert_eq!(vus[0]["phase"], PHASE_PAR_NOM);
        assert_eq!(vus[0]["processed"], 0);
        assert_eq!(vus[0]["total"], 12);

        // Cadence : rien ne bouge avant la cinquième, puis la dixième.
        for avant in 1..5 {
            assert_eq!(
                vus[avant]["processed"],
                0,
                "publication hors cadence avant la fiche {}",
                avant + 1
            );
        }
        assert_eq!(vus[5]["processed"], 5, "cinquième fiche publiée");
        assert_eq!(vus[10]["processed"], 10, "dixième fiche publiée");

        // Et la dernière tranche n'est pas perdue.
        let fin = lu(&settings);
        assert_eq!(fin["phase"], PHASE_PAR_NOM);
        assert_eq!(fin["processed"], 12);
        assert_eq!(fin["total"], 12);
        assert_eq!(fin["enriched"], 3);
        assert_eq!((discogs, lastfm), (3, 0));
    }

    // ---------------------------------------------------------------------
    // #2221 — la recherche d'image d'artiste PAR NOM doit départager.
    //
    // La phase 3 de l'enrichissement ne traite que les artistes SANS MBID —
    // exactement ceux dont on ne peut vérifier l'identité autrement que par le
    // nom. Une image fausse posée là s'installe durablement : mieux vaut ne
    // rien poser.
    // ---------------------------------------------------------------------

    /// La casse et les espaces de bord ne comptent pas. Le reste, si.
    #[test]
    fn nom_exact_a_la_casse_et_aux_espaces_pres() {
        assert!(artist_name_matches_exactly("  Pink Floyd ", "pink floyd"));
        assert!(artist_name_matches_exactly(
            "MARQUIS DE SADE",
            "Marquis de Sade"
        ));

        // Un approchant n'est PAS une correspondance : chercher « Air »
        // ramènerait « Airbourne ».
        assert!(!artist_name_matches_exactly("Airbourne", "Air"));
        assert!(!artist_name_matches_exactly("Air", "Airbourne"));

        // Pas de repli des accents : deux entrées distinctes de la
        // bibliothèque, comme côté web (`trouverArtisteExact`).
        assert!(!artist_name_matches_exactly("Motörhead", "Motorhead"));

        // Un nom vide se « retrouve » partout — c'est le piège qui fabrique une
        // fausse preuve. Il ne départage rien, donc il ne correspond à rien.
        assert!(!artist_name_matches_exactly("", ""));
        assert!(!artist_name_matches_exactly("   ", "Pink Floyd"));
        assert!(!artist_name_matches_exactly("Pink Floyd", ""));
    }

    /// Le désambiguïsateur numérique de Discogs, et rien d'autre.
    #[test]
    fn desambiguisateur_discogs_retire_les_parentheses_numeriques() {
        assert_eq!(
            strip_discogs_disambiguator("Marquis De Sade (2)"),
            "Marquis De Sade"
        );
        assert_eq!(strip_discogs_disambiguator("Nirvana (5)"), "Nirvana");
        // Ce qui n'est pas un nombre entre parenthèses reste intact.
        assert_eq!(
            strip_discogs_disambiguator("Nine Inch Nails (US)"),
            "Nine Inch Nails (US)"
        );
        assert_eq!(strip_discogs_disambiguator("Sunn O)))"), "Sunn O)))");
        assert_eq!(strip_discogs_disambiguator("Front 242"), "Front 242");
        assert_eq!(strip_discogs_disambiguator("Aphex Twin"), "Aphex Twin");
    }

    /// LE cas signalé au forum : le marquis (personnage historique) et le
    /// groupe rennais partagent le nom. Discogs le dit lui-même en numérotant
    /// la seconde entrée. Rien ne départage ⇒ on ne pose AUCUNE image.
    #[test]
    fn discogs_homonyme_declare_refuse_toute_image() {
        let data = serde_json::json!({
            "results": [
                { "title": "Marquis De Sade", "cover_image": "https://img.discogs.com/marquis-personnage.jpg" },
                { "title": "Marquis De Sade (2)", "cover_image": "https://img.discogs.com/groupe-rennais.jpg" }
            ]
        });
        assert_eq!(discogs_pick_cover_image(&data, "Marquis de Sade"), None);
    }

    /// Aucun résultat ne porte le nom cherché : le premier venu n'est pas une
    /// réponse. C'est ce que faisait `per_page=1` + `results[0]`.
    #[test]
    fn discogs_sans_correspondance_exacte_refuse_le_premier_venu() {
        let data = serde_json::json!({
            "results": [
                { "title": "Airbourne", "cover_image": "https://img.discogs.com/airbourne.jpg" },
                { "title": "Air France", "cover_image": "https://img.discogs.com/airfrance.jpg" }
            ]
        });
        assert_eq!(discogs_pick_cover_image(&data, "Air"), None);
    }

    /// Une seule entrée porte le nom : elle départage, on la prend.
    #[test]
    fn discogs_correspondance_unique_est_acceptee() {
        let data = serde_json::json!({
            "results": [
                { "title": "Pink Floyd", "cover_image": "https://img.discogs.com/pink-floyd.jpg" },
                { "title": "Pink Floyd Tribute Band", "cover_image": "https://img.discogs.com/tribute.jpg" }
            ]
        });
        assert_eq!(
            discogs_pick_cover_image(&data, "pink floyd"),
            Some("https://img.discogs.com/pink-floyd.jpg")
        );
    }

    /// Seule l'entrée numérotée existe : Discogs ne déclare alors aucun
    /// homonyme, la correspondance est unique.
    #[test]
    fn discogs_entree_numerotee_seule_est_acceptee() {
        let data = serde_json::json!({
            "results": [
                { "title": "Marquis De Sade (2)", "cover_image": "https://img.discogs.com/groupe-rennais.jpg" }
            ]
        });
        assert_eq!(
            discogs_pick_cover_image(&data, "Marquis de Sade"),
            Some("https://img.discogs.com/groupe-rennais.jpg")
        );
    }

    /// Le gabarit vide de Discogs n'est pas une image (garde existante).
    #[test]
    fn discogs_spacer_gif_n_est_pas_une_image() {
        let data = serde_json::json!({
            "results": [
                { "title": "Untel", "cover_image": "https://img.discogs.com/spacer.gif" }
            ]
        });
        assert_eq!(discogs_pick_cover_image(&data, "Untel"), None);
    }

    /// Last.fm redirige silencieusement vers un autre artiste (« Sade » →
    /// « Marquis de Sade »). Le nom rendu ne correspond pas ⇒ on refuse.
    #[test]
    fn lastfm_nom_rendu_different_refuse_l_image() {
        let data = serde_json::json!({
            "artist": {
                "name": "Marquis de Sade",
                "image": [ { "size": "mega", "#text": "https://lastfm.freetls.fastly.net/mega.jpg" } ]
            }
        });
        assert_eq!(lastfm_pick_image_url(&data, "Sade"), None);
    }

    /// Nom rendu identique : on prend la plus grande taille disponible.
    #[test]
    fn lastfm_nom_rendu_identique_prend_la_plus_grande_taille() {
        let data = serde_json::json!({
            "artist": {
                "name": "Pink Floyd",
                "image": [
                    { "size": "large", "#text": "https://lastfm.freetls.fastly.net/large.jpg" },
                    { "size": "mega", "#text": "https://lastfm.freetls.fastly.net/mega.jpg" }
                ]
            }
        });
        assert_eq!(
            lastfm_pick_image_url(&data, "  pink floyd "),
            Some("https://lastfm.freetls.fastly.net/mega.jpg")
        );
    }

    /// Le gabarit « étoile grise » de Last.fm n'est pas une image : on retombe
    /// sur la taille inférieure (garde existante).
    #[test]
    fn lastfm_placeholder_est_ignore() {
        let data = serde_json::json!({
            "artist": {
                "name": "Pink Floyd",
                "image": [
                    { "size": "large", "#text": "https://lastfm.freetls.fastly.net/large.jpg" },
                    { "size": "mega", "#text": "https://lastfm.freetls.fastly.net/2a96cbd8b46e442fc41c2b86b821562f.png" }
                ]
            }
        });
        assert_eq!(
            lastfm_pick_image_url(&data, "Pink Floyd"),
            Some("https://lastfm.freetls.fastly.net/large.jpg")
        );
    }

    /// Une réponse Last.fm sans nom d'artiste ne départage rien.
    #[test]
    fn lastfm_sans_nom_rendu_refuse_l_image() {
        let data = serde_json::json!({
            "artist": {
                "image": [ { "size": "mega", "#text": "https://lastfm.freetls.fastly.net/mega.jpg" } ]
            }
        });
        assert_eq!(lastfm_pick_image_url(&data, "Pink Floyd"), None);
    }

    /// Le cas du testeur : une compilation « maison » où UNE seule piste porte
    /// une pochette intégrée. C'est la pochette du DOSSIER qui doit décrire
    /// l'album, pas celle d'un fichier isolé.
    #[test]
    fn folder_cover_wins_and_is_shared_by_every_track() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("Compilation maison");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("cover.jpg"), b"POCHETTE-DU-DOSSIER").unwrap();
        let t1 = music.join("01 - Brel.flac");
        let t2 = music.join("02 - Barbara.flac");
        std::fs::write(&t1, b"").unwrap();
        std::fs::write(&t2, b"").unwrap();

        let cache = dir.path().join("cache");
        let h1 = folder_cover_hash(&t1, &cache).expect("pochette de dossier trouvée");
        let h2 = folder_cover_hash(&t2, &cache).expect("pochette de dossier trouvée");

        // Deux pistes du même dossier partagent LA MÊME entrée de cache : le
        // hachage porte sur la pochette, pas sur le fichier audio.
        assert_eq!(h1, h2);
        assert_eq!(
            std::fs::read(cache.join(format!("{h1}.jpg"))).unwrap(),
            b"POCHETTE-DU-DOSSIER"
        );
    }

    fn nb_fichiers(dir: &Path) -> usize {
        std::fs::read_dir(dir).map(|it| it.count()).unwrap_or(0)
    }

    /// Vecteur connu : SHA-256 de la chaîne vide. Le condensat de contenu doit
    /// être exactement cela — 64 hexdigits, la forme que `is_hex_hash` accepte
    /// déjà partout (routes HTTP et `upnp_server::artwork_url`).
    #[test]
    fn content_hash_est_un_sha256_hexadecimal() {
        assert_eq!(
            content_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let h = content_hash(b"POCHETTE");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Le cas #1440/#1444 : une compilation éclatée façon Qobuz — la MÊME
    /// jaquette recopiée dans N dossiers d'artiste. Adressée par le chemin,
    /// chaque dossier fabriquait sa propre entrée de cache ; adressée par le
    /// contenu, ils convergent tous vers UNE entrée.
    #[test]
    fn meme_octets_dans_deux_dossiers_une_seule_entree_de_cache() {
        let dir = tempfile::tempdir().unwrap();
        let jaquette = b"JAQUETTE-COMMUNE-DE-LA-COMPILATION";
        let mut hashes = Vec::new();
        for artiste in ["Corte Real", "Autre Artiste"] {
            let dossier = dir.path().join(artiste).join("OUF L'anthologie");
            std::fs::create_dir_all(&dossier).unwrap();
            std::fs::write(dossier.join("cover.jpg"), jaquette).unwrap();
            let piste = dossier.join("01 - Opium.flac");
            std::fs::write(&piste, b"").unwrap();
            hashes.push(folder_cover_hash(&piste, &dir.path().join("cache")).unwrap());
        }
        assert_eq!(hashes[0], hashes[1], "mêmes octets = même adresse");
        assert_eq!(
            nb_fichiers(&dir.path().join("cache")),
            1,
            "une seule entrée de cache pour N dossiers"
        );
    }

    /// Deux jaquettes différentes ne partagent rien : adresses distinctes,
    /// deux entrées.
    #[test]
    fn octets_differents_deux_entrees_distinctes() {
        let dir = tempfile::tempdir().unwrap();
        let mut hashes = Vec::new();
        for (nom, octets) in [("A", b"JAQUETTE-A".as_slice()), ("B", b"JAQUETTE-B")] {
            let dossier = dir.path().join(nom);
            std::fs::create_dir_all(&dossier).unwrap();
            std::fs::write(dossier.join("cover.jpg"), octets).unwrap();
            let piste = dossier.join("01.flac");
            std::fs::write(&piste, b"").unwrap();
            hashes.push(folder_cover_hash(&piste, &dir.path().join("cache")).unwrap());
        }
        assert_ne!(hashes[0], hashes[1]);
        assert_eq!(nb_fichiers(&dir.path().join("cache")), 2);
    }

    /// Un rescan ne duplique rien et rend la même adresse : le deuxième
    /// passage retombe sur l'entrée de contenu écrite au premier.
    #[test]
    fn rescan_stable_meme_adresse_sans_duplication() {
        let dir = tempfile::tempdir().unwrap();
        let dossier = dir.path().join("Album");
        std::fs::create_dir_all(&dossier).unwrap();
        std::fs::write(dossier.join("cover.jpg"), b"JAQUETTE").unwrap();
        let piste = dossier.join("01.flac");
        std::fs::write(&piste, b"").unwrap();
        let cache = dir.path().join("cache");
        let h1 = folder_cover_hash(&piste, &cache).unwrap();
        let h2 = folder_cover_hash(&piste, &cache).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(nb_fichiers(&cache), 1);
    }

    /// Un cache déjà constitué sous l'ancien schéma (condensat du CHEMIN de la
    /// pochette) reste servi tel quel : la route sert `immutable,
    /// max-age=31536000`, une URL distribuée doit rester valable. Le rescan ne
    /// doit ni l'invalider ni écrire une entrée de contenu en doublon.
    #[test]
    fn entree_heritee_par_chemin_reste_servie_sans_doublon() {
        let dir = tempfile::tempdir().unwrap();
        let dossier = dir.path().join("Album");
        std::fs::create_dir_all(&dossier).unwrap();
        let pochette = dossier.join("cover.jpg");
        std::fs::write(&pochette, b"JAQUETTE-HERITEE").unwrap();
        let piste = dossier.join("01.flac");
        std::fs::write(&piste, b"").unwrap();
        let cache = dir.path().join("cache");
        // Cache constitué par une version antérieure : entrée au condensat du
        // chemin.
        let legacy = artwork_hash(&pochette.to_string_lossy());
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(format!("{legacy}.jpg")), b"JAQUETTE-HERITEE").unwrap();

        let h = folder_cover_hash(&piste, &cache).unwrap();
        assert_eq!(h, legacy, "l'adresse déjà distribuée est conservée");
        assert_eq!(
            nb_fichiers(&cache),
            1,
            "aucune entrée de contenu en doublon"
        );
    }

    // ------------------------------------------------------------------
    // #1444 — les producteurs restés adressés par l'IDENTITÉ.
    // ------------------------------------------------------------------

    /// Le défaut nommé par le titre du ticket, sur le seul producteur local
    /// qui l'avait encore : la photo d'artiste posée dans le dossier était
    /// adressée par le CHEMIN du fichier. La même `artist.jpg` recopiée dans
    /// les N dossiers d'album d'un artiste — ce que fait tout extracteur de
    /// bibliothèque — écrivait N entrées de cache pour une seule photo.
    #[test]
    fn artist_jpg_identique_dans_n_dossiers_une_seule_entree() {
        let dir = tempfile::tempdir().unwrap();
        let photo = b"PHOTO-DE-L-ARTISTE";
        let cache = dir.path().join("cache");
        let mut adresses = Vec::new();
        for album in ["Album 1", "Album 2", "Album 3"] {
            let dossier = dir.path().join("Keith Jarrett").join(album);
            std::fs::create_dir_all(&dossier).unwrap();
            std::fs::write(dossier.join("artist.jpg"), photo).unwrap();
            let piste = dossier.join("01.flac");
            std::fs::write(&piste, b"").unwrap();
            adresses.push(folder_artist_image_hash(&piste, &cache).unwrap());
        }
        assert_eq!(adresses[0], adresses[1], "mêmes octets = même adresse");
        assert_eq!(adresses[1], adresses[2], "mêmes octets = même adresse");
        assert_eq!(
            nb_fichiers(&cache),
            1,
            "une seule entrée de cache pour N dossiers"
        );
        assert_eq!(
            std::fs::read(cache.join(format!("{}.jpg", adresses[0]))).unwrap(),
            photo
        );
    }

    /// Témoin de non-fusion — le sens que la migration de clé doit garantir
    /// AUSSI : deux photos différentes n'ont jamais la même adresse. Vert des
    /// deux côtés du correctif ; c'est ce qui rend la bascule sûre.
    #[test]
    fn deux_artist_jpg_differents_ne_fusionnent_jamais() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let mut adresses = Vec::new();
        for (nom, octets) in [
            ("Keith Jarrett", b"PHOTO-JARRETT".as_slice()),
            ("Duke Ellington", b"PHOTO-ELLINGTON"),
        ] {
            let dossier = dir.path().join(nom);
            std::fs::create_dir_all(&dossier).unwrap();
            std::fs::write(dossier.join("artist.jpg"), octets).unwrap();
            let piste = dossier.join("01.flac");
            std::fs::write(&piste, b"").unwrap();
            adresses.push(folder_artist_image_hash(&piste, &cache).unwrap());
        }
        assert_ne!(adresses[0], adresses[1]);
        assert_eq!(nb_fichiers(&cache), 2);
        assert_eq!(
            std::fs::read(cache.join(format!("{}.jpg", adresses[0]))).unwrap(),
            b"PHOTO-JARRETT"
        );
        assert_eq!(
            std::fs::read(cache.join(format!("{}.jpg", adresses[1]))).unwrap(),
            b"PHOTO-ELLINGTON"
        );
    }

    /// Témoin de non-régression : une entrée déjà constituée sous l'ancien
    /// schéma (condensat du CHEMIN) reste servie sous la MÊME adresse — la
    /// route sert `immutable, max-age=31536000`, aucune URL distribuée ne doit
    /// tomber en 404. Vert des deux côtés du correctif.
    #[test]
    fn artist_jpg_entree_heritee_par_chemin_reste_servie() {
        let dir = tempfile::tempdir().unwrap();
        let dossier = dir.path().join("Album");
        std::fs::create_dir_all(&dossier).unwrap();
        let photo = dossier.join("artist.jpg");
        std::fs::write(&photo, b"PHOTO-HERITEE").unwrap();
        let piste = dossier.join("01.flac");
        std::fs::write(&piste, b"").unwrap();
        let cache = dir.path().join("cache");
        let legacy = artwork_hash(&photo.to_string_lossy());
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(format!("{legacy}.jpg")), b"PHOTO-HERITEE").unwrap();

        let h = folder_artist_image_hash(&piste, &cache).unwrap();
        assert_eq!(h, legacy, "l'adresse déjà distribuée est conservée");
        assert_eq!(nb_fichiers(&cache), 1, "aucun doublon de contenu");
    }

    /// `cache_fetched_image`, le chemin des enrichissements en ligne. Deux
    /// sujets distincts qui partageaient une IDENTITÉ — deux albums de même
    /// artiste et même titre (5 groupes / 11 albums mesurés sur .18), deux
    /// artistes homonymes — écrivaient au même endroit : le second écrasait
    /// l'image du premier. Chacun garde désormais la sienne.
    #[test]
    fn enrichissement_deux_images_ne_se_recouvrent_plus() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let a = b"POCHETTE-EDITION-2011";
        let b = b"POCHETTE-EDITION-2019";
        let ha = cache_fetched_image(a, &cache, "jpg").unwrap();
        let hb = cache_fetched_image(b, &cache, "jpg").unwrap();
        assert_ne!(ha, hb, "deux images différentes, deux adresses");
        assert_eq!(nb_fichiers(&cache), 2);
        assert_eq!(std::fs::read(cache.join(format!("{ha}.jpg"))).unwrap(), a);
        assert_eq!(std::fs::read(cache.join(format!("{hb}.jpg"))).unwrap(), b);
    }

    /// Le re-téléchargement (`force`) obtient une adresse NEUVE. Sous l'ancien
    /// condensat d'identité il réécrivait l'adresse déjà distribuée, servie
    /// `immutable, max-age=31536000` : navigateurs et cache d'images Flutter
    /// affichaient l'ancienne image un an.
    #[test]
    fn re_telechargement_obtient_une_adresse_neuve() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let avant = cache_fetched_image(b"ANCIENNE-PHOTO", &cache, "jpg").unwrap();
        let apres = cache_fetched_image(b"NOUVELLE-PHOTO", &cache, "jpg").unwrap();
        assert_ne!(
            avant, apres,
            "une image remplacée doit changer d'URL, sinon le cache immuable la masque un an"
        );
        // L'ancienne reste lisible : les URL déjà distribuées ne tombent pas.
        assert_eq!(
            std::fs::read(cache.join(format!("{avant}.jpg"))).unwrap(),
            b"ANCIENNE-PHOTO"
        );
    }

    /// Comptage des COLLISIONS de la nouvelle clé, dans le sens qui compte :
    /// N images deux à deux différentes doivent donner N adresses distinctes.
    /// Aucune fusion ne doit apparaître. Témoin vert des deux côtés.
    #[test]
    fn aucune_collision_sur_un_corpus_d_images_distinctes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let mut adresses = std::collections::HashSet::new();
        for i in 0..512u32 {
            let octets = format!("IMAGE-{i}").into_bytes();
            adresses.insert(cache_fetched_image(&octets, &cache, "jpg").unwrap());
        }
        assert_eq!(adresses.len(), 512, "512 images distinctes, 512 adresses");
        assert_eq!(nb_fichiers(&cache), 512, "aucune entrée écrasée");
    }

    /// Toute adresse rendue est SERVABLE. C'est ce que le repli Discogs de
    /// `routes/metadata.rs` ne garantissait pas : `cover_fetcher` dépose son
    /// fichier dans le même répertoire mais sous un nom PRÉFIXÉ
    /// (`discogs_{md5}.jpg`), et la route annonçait le radical `discogs_{md5}`
    /// comme `cover_path`. `is_hex_hash` le refuse — le souligné n'est pas un
    /// hexdigit — donc la lecture le prenait pour un CHEMIN et cherchait
    /// `md5("discogs_{md5}").jpg`, un fichier qui n'a jamais existé. Toute
    /// pochette trouvée par cette route était servie en 404 (#2567).
    #[test]
    fn l_adresse_rendue_est_toujours_servable() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let h = cache_fetched_image(b"POCHETTE-DISCOGS", &cache, "jpg").unwrap();
        assert_eq!(h.len(), 64, "la forme qu'is_hex_hash accepte");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            find_cached(&cache, &h).is_some(),
            "la route doit retrouver le fichier sous l'adresse annoncée"
        );

        // Le radical préfixé d'avant : ni hexadécimal, ni retrouvable.
        let radical = format!("discogs_{}", artwork_hash("un-album"));
        std::fs::write(cache.join(format!("{radical}.jpg")), b"POCHETTE-DISCOGS").unwrap();
        assert!(!radical.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            find_cached(&cache, &artwork_hash(&radical)).is_none(),
            "traité comme un chemin par la lecture : 404 garanti"
        );
    }

    /// `save_embedded_cover` : la même jaquette intégrée à deux pistes
    /// différentes ne peuple le cache que d'une entrée.
    #[test]
    fn pochette_integree_identique_partagee_entre_pistes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let cover = (b"JAQUETTE-INTEGREE".to_vec(), "image/jpeg".to_string());
        let h1 = save_embedded_cover(Path::new("/musique/a/01.flac"), &cache, &cover).unwrap();
        let h2 = save_embedded_cover(Path::new("/musique/b/02.flac"), &cache, &cover).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(nb_fichiers(&cache), 1);
    }

    /// `get_or_extract` sur des pistes sans tags : la pochette de dossier
    /// n'est plus dupliquée par piste — l'ancien schéma hachait le chemin de
    /// la PISTE et écrivait N copies des mêmes octets.
    #[test]
    fn get_or_extract_pochette_de_dossier_une_entree_pour_n_pistes() {
        let dir = tempfile::tempdir().unwrap();
        let dossier = dir.path().join("Album");
        std::fs::create_dir_all(&dossier).unwrap();
        std::fs::write(dossier.join("cover.jpg"), b"JAQUETTE-DOSSIER").unwrap();
        let cache = dir.path().join("cache");
        let mut hashes = Vec::new();
        for nom in ["01.flac", "02.flac"] {
            let piste = dossier.join(nom);
            std::fs::write(&piste, b"").unwrap();
            hashes.push(get_or_extract(&piste, &cache).unwrap());
        }
        assert_eq!(hashes[0], hashes[1]);
        assert_eq!(nb_fichiers(&cache), 1);
    }

    /// Sans pochette dans le dossier, rien n'est inventé — l'appelant retombe
    /// alors sur l'extraction des tags, comme avant.
    #[test]
    fn no_folder_cover_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("Album nu");
        std::fs::create_dir_all(&music).unwrap();
        let t = music.join("01.flac");
        std::fs::write(&t, b"").unwrap();
        assert!(folder_cover_hash(&t, &dir.path().join("cache")).is_none());
    }

    #[test]
    fn extended_path_windows_and_noop() {
        // Windows drive path -> verbatim prefix, separators normalized.
        assert_eq!(
            extended_path(Path::new("C:\\Music\\Long\\file.flac"))
                .to_str()
                .unwrap(),
            "\\\\?\\C:\\Music\\Long\\file.flac"
        );
        assert_eq!(
            extended_path(Path::new("C:/Music/Long/file.flac"))
                .to_str()
                .unwrap(),
            "\\\\?\\C:\\Music\\Long\\file.flac"
        );
        // UNC (NAS) path -> \\?\UNC\server\share\…
        assert_eq!(
            extended_path(Path::new("\\\\nas\\music\\album\\cover.jpg"))
                .to_str()
                .unwrap(),
            "\\\\?\\UNC\\nas\\music\\album\\cover.jpg"
        );
        // Already verbatim -> unchanged.
        assert_eq!(
            extended_path(Path::new("\\\\?\\C:\\a")).to_str().unwrap(),
            "\\\\?\\C:\\a"
        );
        // Unix absolute and relative paths -> untouched no-op.
        assert_eq!(
            extended_path(Path::new("/home/user/music/x.flac"))
                .to_str()
                .unwrap(),
            "/home/user/music/x.flac"
        );
        assert_eq!(
            extended_path(Path::new("album/cover.jpg"))
                .to_str()
                .unwrap(),
            "album/cover.jpg"
        );
    }

    #[test]
    fn itunes_hires_url_upscales() {
        assert_eq!(
            itunes_hires_url("https://x/100x100bb.jpg").as_deref(),
            Some("https://x/1200x1200bb.jpg")
        );
        assert!(itunes_hires_url("https://x/cover.jpg").is_none());
    }

    #[test]
    fn artwork_hash_deterministic() {
        let h1 = artwork_hash("/music/test.flac");
        let h2 = artwork_hash("/music/test.flac");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 32);
    }

    #[test]
    fn nonexistent_file_returns_none() {
        assert!(extract_cover_art(Path::new("/tmp/nonexistent.flac")).is_none());
    }

    #[test]
    fn backfill_fills_missing_covers_then_is_idempotent() {
        use crate::db::album_repo::AlbumRepo;
        use crate::db::artist_repo::ArtistRepo;
        use crate::db::backend::DbBackend;
        use crate::db::models::{Artist, Track};
        use crate::db::sqlite::SqliteDb;
        use crate::db::track_repo::TrackRepo;
        use std::sync::Arc;

        // Isolated temp dir: a track whose folder holds a cover.jpg. This
        // exercises the backfill wiring end to end (list_without_cover →
        // get_or_extract → force_update_cover_path); DSF ID3v2 extraction
        // itself is covered by the metadata parser path.
        let base = crate::test_scratch::scratch_dir("tune_backfill");
        let music = base.join("album");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("cover.jpg"), b"\xff\xd8\xff\xe0dummyjpegdata").unwrap();
        let track_path = music.join("01.flac");
        std::fs::write(&track_path, b"not really flac").unwrap();
        let cache_dir = base.join("cache");

        let sqlite = SqliteDb::open_in_memory().unwrap();
        sqlite.init_schema().unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(sqlite);

        let artist_repo = ArtistRepo::with_backend(backend.clone());
        let album_repo = AlbumRepo::with_backend(backend.clone());
        let track_repo = TrackRepo::with_backend(backend.clone());

        let aid = artist_repo
            .create(&Artist::new("Art Lande".into()))
            .unwrap();
        let alid = album_repo
            .get_or_create("While She Sleeps", aid, Some(1990))
            .unwrap()
            .id
            .unwrap();
        let mut track = Track::new("Snow Dance".into());
        track.artist_id = Some(aid);
        track.album_id = Some(alid);
        track.file_path = Some(track_path.to_string_lossy().into_owned());
        track_repo.create(&track).unwrap();

        // Album starts with no cover.
        assert!(
            album_repo
                .get(alid)
                .unwrap()
                .unwrap()
                .cover_path
                .as_deref()
                .unwrap_or("")
                .is_empty()
        );

        let filled = backfill_embedded_covers(&backend, &cache_dir);
        assert_eq!(filled, 1, "backfill should fill exactly one album");
        let cover = album_repo.get(alid).unwrap().unwrap().cover_path;
        assert!(
            cover.as_deref().is_some_and(|c| !c.is_empty()),
            "album cover_path should be set after backfill"
        );

        // Second run is a no-op: the album now has a cover.
        let filled_again = backfill_embedded_covers(&backend, &cache_dir);
        assert_eq!(
            filled_again, 0,
            "backfill must not re-process covered albums"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn artwork_hash_different_for_different_paths() {
        let h1 = artwork_hash("/music/a.flac");
        let h2 = artwork_hash("/music/b.flac");
        assert_ne!(h1, h2);
    }

    #[test]
    fn artwork_hash_hex_chars() {
        let h = artwork_hash("/test");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn artwork_hash_empty_string() {
        let h = artwork_hash("");
        assert_eq!(h.len(), 32);
        // MD5 of empty string
        assert_eq!(h, "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn artwork_hash_unicode_path() {
        let h = artwork_hash("/music/Rene/album.flac");
        assert_eq!(h.len(), 32);
    }

    #[test]
    fn find_folder_cover_nonexistent_dir() {
        let result = find_folder_cover(Path::new("/tmp/nonexistent_dir_12345/track.flac"));
        assert!(result.is_none());
    }

    #[test]
    fn save_to_cache_and_read() {
        let base = tempfile::TempDir::new().unwrap();
        let dir = base.path().join("cache");

        let data = b"fake image data";
        let result = save_to_cache(data, &dir, "test_hash_123", "jpg");
        assert!(result.is_some());

        let path = result.unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[test]
    fn save_to_cache_creates_dir() {
        let base = tempfile::TempDir::new().unwrap();
        let dir = base.path().join("nouveau");
        assert!(!dir.exists());

        save_to_cache(b"test", &dir, "hash", "png");
        assert!(dir.exists());
    }

    #[test]
    fn get_or_extract_nonexistent() {
        let base = tempfile::TempDir::new().unwrap();
        let cache_dir = base.path().join("cache");
        let result = get_or_extract(Path::new("/tmp/nonexistent_audio_file.flac"), &cache_dir);
        assert!(result.is_none());
    }

    #[test]
    fn artwork_hash_normalizes_backslashes() {
        // Windows path with backslashes should produce the same hash
        // as the equivalent path with forward slashes
        let h_win = artwork_hash("C:\\Users\\Scordia\\Music\\album\\track.flac");
        let h_unix = artwork_hash("C:/Users/Scordia/Music/album/track.flac");
        assert_eq!(h_win, h_unix);
    }

    #[test]
    fn artwork_hash_forward_slashes_unchanged() {
        // Pure Unix paths should hash identically before and after normalization
        let h = artwork_hash("/music/artist/album/track.flac");
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ------------------------------------------------------------------
    // #2567 — un condensat annoncé DOIT avoir un fichier que la route sait
    // servir.
    //
    // La base ne stocke pas un chemin : elle stocke la CLÉ DE CACHE que le
    // client demandera à `/api/v1/library/artwork/{clé}`. Écrire cette clé
    // alors que le fichier porte une extension que la route ne regarde pas
    // revient à annoncer une pochette qu'on ne sert pas — 404, image de
    // remplacement à l'écran, et rien dans le journal.
    //
    // L'invariant tient en une ligne : ce que l'écriture rend, la lecture doit
    // le retrouver. `find_cached` EST la lecture ; les tests ci-dessous ne
    // demandent rien d'autre.
    // ------------------------------------------------------------------

    /// Un album dont la pochette de dossier s'appelle `cover.jpeg` — quatre
    /// lettres, pas trois. `find_folder_cover` l'accepte (elle est dans
    /// `FOLDER_COVER_NAMES`), l'écriture recopiait son extension telle quelle.
    #[test]
    fn pochette_de_dossier_en_jpeg_annoncee_donc_servable() {
        let dir = tempfile::TempDir::new().unwrap();
        let music = dir.path().join("Existence");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("cover.jpeg"), b"POCHETTE").unwrap();
        let track = music.join("01 - Pizza Boy.flac");
        std::fs::write(&track, b"").unwrap();
        let cache = dir.path().join("cache");

        let hash = get_or_extract(&track, &cache).expect("une pochette de dossier a été trouvée");

        assert!(
            find_cached(&cache, &hash).is_some(),
            "condensat {hash} annoncé mais introuvable par la route : \
             la pochette a été écrite sous une extension que serve_artwork ne \
             regarde pas — 404 et image de remplacement (#2567). \
             Présents dans le cache : {:?}",
            listing(&cache)
        );
    }

    /// Même chose avec une extension en majuscules : `FOLDER.JPG` est dans la
    /// liste des noms acceptés, et sur un système de fichiers sensible à la
    /// casse (Linux, la plupart des NAS) `FOLDER.JPG` n'est pas `folder.jpg`.
    #[test]
    fn pochette_de_dossier_en_majuscules_annoncee_donc_servable() {
        let dir = tempfile::TempDir::new().unwrap();
        let music = dir.path().join("Album crié");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("FOLDER.JPG"), b"POCHETTE").unwrap();
        let track = music.join("01.flac");
        std::fs::write(&track, b"").unwrap();
        let cache = dir.path().join("cache");

        let hash = get_or_extract(&track, &cache).expect("une pochette de dossier a été trouvée");

        assert!(
            find_cached(&cache, &hash).is_some(),
            "condensat {hash} annoncé mais introuvable par la route (#2567). \
             Présents dans le cache : {:?}",
            listing(&cache)
        );
    }

    /// Une pochette intégrée au format BMP : `extract_cover_art` rend
    /// `image/bmp`, `save_embedded_cover` écrivait `.bmp`, la route ne
    /// connaissait pas cette extension.
    #[test]
    fn pochette_integree_en_bmp_annoncee_donc_servable() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = dir.path().join("cache");
        let track = dir.path().join("Album/01.flac");
        std::fs::create_dir_all(track.parent().unwrap()).unwrap();

        let cover = (b"BM-POCHETTE".to_vec(), "image/bmp".to_string());
        let hash = save_embedded_cover(&track, &cache, &cover).expect("écriture acquittée");

        assert!(
            find_cached(&cache, &hash).is_some(),
            "condensat {hash} annoncé mais introuvable par la route (#2567). \
             Présents dans le cache : {:?}",
            listing(&cache)
        );
    }

    /// Le même contrat, au ras de l'écriture : quelle que soit l'orthographe de
    /// l'extension qu'on lui passe, `save_to_cache` doit produire un fichier que
    /// la lecture retrouve.
    #[test]
    fn save_to_cache_ecrit_sous_une_extension_que_la_lecture_retrouve() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut manquantes = Vec::new();
        let orthographes = ["jpg", "jpeg", "JPG", "JPEG", "png", "PNG", "webp", "bmp"];
        for (i, ext) in orthographes.iter().enumerate() {
            let cache = dir.path().join(format!("cache{i}"));
            let hash = format!("{i:032x}");
            save_to_cache(b"IMAGE", &cache, &hash, ext).expect("écriture acquittée");
            if find_cached(&cache, &hash).is_none() {
                manquantes.push((*ext, listing(&cache)));
            }
        }
        assert!(
            manquantes.is_empty(),
            "{} orthographe(s) sur {} écrivent un fichier que la route ne sert pas (#2567) : {:?}",
            manquantes.len(),
            orthographes.len(),
            manquantes
        );
    }

    /// L'autre moitié du contrat : l'écriture ne doit plus **produire**
    /// d'orthographe héritée. Sans cela, la liste que la lecture doit connaître
    /// s'allongerait à chaque source d'image nouvelle, et un cache neuf
    /// continuerait de se remplir de noms que seule la clémence de la lecture
    /// rattrape.
    #[test]
    fn l_ecriture_ne_produit_que_des_orthographes_canoniques() {
        let dir = tempfile::TempDir::new().unwrap();
        let attendus = [
            ("jpg", "jpg"),
            ("jpeg", "jpg"),
            ("JPG", "jpg"),
            ("JPEG", "jpg"),
            ("Jpeg", "jpg"),
            ("png", "png"),
            ("PNG", "png"),
            ("webp", "webp"),
            ("WEBP", "webp"),
            ("bmp", "bmp"),
            ("gif", "jpg"), // inconnue : jpg, comme le suppose extract_cover_art
        ];
        let mut ecarts = Vec::new();
        for (i, (donnee, canonique)) in attendus.iter().enumerate() {
            let cache = dir.path().join(format!("c{i}"));
            let hash = format!("{i:032x}");
            save_to_cache(b"IMAGE", &cache, &hash, donnee).expect("écriture acquittée");
            let obtenu = listing(&cache);
            if obtenu != vec![format!("{hash}.{canonique}")] {
                ecarts.push((*donnee, obtenu));
            }
        }
        assert!(
            ecarts.is_empty(),
            "{} orthographe(s) sur {} écrites hors de la forme canonique (#2567) : {:?}",
            ecarts.len(),
            attendus.len(),
            ecarts
        );
    }

    /// Garde-fou contre l'excès inverse : sans pochette nulle part, rien n'est
    /// annoncé. Un condensat inventé vaut pire qu'une absence — il pose un
    /// `src` qui échoue au lieu de laisser l'image de remplacement.
    #[test]
    fn sans_pochette_aucun_condensat_n_est_invente() {
        let dir = tempfile::TempDir::new().unwrap();
        let music = dir.path().join("Album nu");
        std::fs::create_dir_all(&music).unwrap();
        let track = music.join("01.flac");
        std::fs::write(&track, b"").unwrap();
        let cache = dir.path().join("cache");

        assert!(get_or_extract(&track, &cache).is_none());
        assert!(listing(&cache).is_empty(), "rien ne doit être écrit");
    }

    /// Le condensat est une clé de cache : il ne bouge pas d'un appel à
    /// l'autre. S'il bougeait, chaque client retéléchargerait la bibliothèque
    /// entière à chaque passe.
    #[test]
    fn condensat_stable_entre_deux_appels() {
        let dir = tempfile::TempDir::new().unwrap();
        let music = dir.path().join("Existence");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("cover.jpeg"), b"POCHETTE").unwrap();
        let track = music.join("01.flac");
        std::fs::write(&track, b"").unwrap();
        let cache = dir.path().join("cache");

        let a = get_or_extract(&track, &cache).expect("premier appel");
        let b = get_or_extract(&track, &cache).expect("second appel");
        assert_eq!(a, b, "la clé de cache d'une même pochette doit être stable");
        assert!(find_cached(&cache, &a).is_some());
    }

    /// Deux pochettes différentes ne partagent pas une entrée de cache : sinon
    /// la seconde écraserait la première et un album porterait la jaquette
    /// d'un autre.
    #[test]
    fn deux_pochettes_differentes_donnent_deux_condensats() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = dir.path().join("cache");
        let mut hashes = Vec::new();
        for (nom, octets) in [
            ("Album A", &b"POCHETTE-A"[..]),
            ("Album B", &b"POCHETTE-B"[..]),
        ] {
            let music = dir.path().join(nom);
            std::fs::create_dir_all(&music).unwrap();
            std::fs::write(music.join("cover.jpeg"), octets).unwrap();
            let track = music.join("01.flac");
            std::fs::write(&track, b"").unwrap();
            hashes.push(get_or_extract(&track, &cache).expect("pochette trouvée"));
        }
        assert_ne!(hashes[0], hashes[1]);
        let (chemin, _) = find_cached(&cache, &hashes[0]).expect("A servable");
        assert_eq!(std::fs::read(chemin).unwrap(), b"POCHETTE-A");
        let (chemin, _) = find_cached(&cache, &hashes[1]).expect("B servable");
        assert_eq!(std::fs::read(chemin).unwrap(), b"POCHETTE-B");
    }

    /// Noms de fichiers présents dans un répertoire de cache, triés. Sert à
    /// faire dire aux échecs ce qui a réellement été écrit, plutôt que « rien
    /// trouvé ».
    fn listing(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}
