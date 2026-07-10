use tracing::{debug, info, warn};

const MB_USER_AGENT: &str = "Tune/0.1.0 (https://mozaiklabs.fr)";

/// A fetched bio together with its provenance, for CC BY-SA attribution.
pub struct BioResult {
    pub text: String,
    pub source: String, // "wikipedia" | "lastfm"
    pub source_url: Option<String>,
    pub license: String, // e.g. "CC-BY-SA-4.0"
    pub lang: String,    // "fr" | "en"
}

/// Fetch artist bio from Wikipedia FR via Wikidata, with Last.fm fallback.
pub async fn fetch_artist_bio(
    client: &reqwest::Client,
    mbid: &str,
    artist_name: &str,
    lastfm_key: &str,
    lang: &str,
) -> Option<BioResult> {
    // 1. Wikipedia in the preferred language via MusicBrainz → Wikidata → sitelinks
    if let Some(bio) = fetch_bio_via_wikidata(client, mbid, lang).await {
        if bio.text.len() > 50 {
            return Some(bio);
        }
    }

    // 2. Last.fm fallback
    if !lastfm_key.is_empty() {
        if let Some(bio) = fetch_bio_lastfm(client, artist_name, lastfm_key, lang).await {
            if bio.text.len() > 50 {
                return Some(bio);
            }
        }
    }

    None
}

/// MusicBrainz → Wikidata QID → French Wikipedia extract.
async fn fetch_bio_via_wikidata(
    client: &reqwest::Client,
    mbid: &str,
    lang: &str,
) -> Option<BioResult> {
    let url = format!("https://musicbrainz.org/ws/2/artist/{mbid}?inc=url-rels&fmt=json");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let relations = data["relations"].as_array()?;

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

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Wikidata → sitelinks → frwiki or enwiki title
    let wd_url = format!("https://www.wikidata.org/wiki/Special:EntityData/{qid}.json");
    let wd_resp = client.get(&wd_url).send().await.ok()?;
    if !wd_resp.status().is_success() {
        return None;
    }
    let wd_data: serde_json::Value = wd_resp.json().await.ok()?;

    // Prefer the user's language, fall back to English.
    let (wiki_lang, wiki_title): (String, String) = if let Some(t) = wd_data
        .pointer(&format!("/entities/{qid}/sitelinks/{lang}wiki/title"))
        .and_then(|v| v.as_str())
    {
        (lang.to_string(), t.to_string())
    } else if let Some(t) = wd_data
        .pointer(&format!("/entities/{qid}/sitelinks/enwiki/title"))
        .and_then(|v| v.as_str())
    {
        ("en".to_string(), t.to_string())
    } else {
        return None;
    };

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Wikipedia MediaWiki API → full intro extract (not just first sentence)
    let wp_url = format!(
        "https://{wiki_lang}.wikipedia.org/w/api.php?action=query&titles={}&prop=extracts&exintro=1&explaintext=1&format=json",
        urlencoding::encode(&wiki_title)
    );
    let wp_resp = client.get(&wp_url).send().await.ok()?;
    if !wp_resp.status().is_success() {
        return None;
    }
    let wp_data: serde_json::Value = wp_resp.json().await.ok()?;
    let pages = wp_data.pointer("/query/pages")?;
    let page = pages.as_object()?.values().next()?;
    let extract = page.get("extract")?.as_str()?;
    if extract.len() < 50 {
        return None;
    }
    Some(BioResult {
        text: extract.trim().to_string(),
        source: "wikipedia".to_string(),
        source_url: Some(format!(
            "https://{wiki_lang}.wikipedia.org/wiki/{}",
            urlencoding::encode(&wiki_title)
        )),
        license: "CC-BY-SA-4.0".to_string(),
        lang: wiki_lang.to_string(),
    })
}

/// Last.fm artist.getInfo → bio summary.
async fn fetch_bio_lastfm(
    client: &reqwest::Client,
    artist_name: &str,
    api_key: &str,
    lang: &str,
) -> Option<BioResult> {
    let resp = client
        .get("https://ws.audioscrobbler.com/2.0/")
        .query(&[
            ("method", "artist.getInfo"),
            ("artist", artist_name),
            ("api_key", api_key),
            ("format", "json"),
            ("lang", lang),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let bio = data
        .pointer("/artist/bio/content")
        .and_then(|v| v.as_str())?;
    let clean = strip_html(bio);
    if clean.len() < 50 {
        return None;
    }
    let source_url = data
        .pointer("/artist/url")
        .and_then(|v| v.as_str())
        .map(String::from);
    Some(BioResult {
        text: clean,
        source: "lastfm".to_string(),
        source_url,
        license: "CC-BY-SA-3.0".to_string(),
        lang: "fr".to_string(),
    })
}

/// Fetch album bio: Wikipedia FR → Wikipedia EN → Last.fm fallback.
pub async fn fetch_album_bio(
    client: &reqwest::Client,
    artist_name: &str,
    album_title: &str,
    lastfm_key: &str,
    lang: &str,
) -> Option<BioResult> {
    // 1. Wikipedia in the preferred language
    if let Some(bio) = fetch_album_bio_wikipedia(client, album_title, artist_name, lang).await {
        if bio.text.len() > 50 {
            return Some(bio);
        }
    }

    // 2. Wikipedia EN fallback
    if lang != "en" {
        if let Some(bio) = fetch_album_bio_wikipedia(client, album_title, artist_name, "en").await {
            if bio.text.len() > 50 {
                return Some(bio);
            }
        }
    }

    // 3. Last.fm fallback
    if !lastfm_key.is_empty() {
        if let Some(bio) =
            fetch_album_bio_lastfm(client, artist_name, album_title, lastfm_key, lang).await
        {
            if bio.text.len() > 50 {
                return Some(bio);
            }
        }
    }

    None
}

/// Search Wikipedia for an album page and extract the intro.
async fn fetch_album_bio_wikipedia(
    client: &reqwest::Client,
    album_title: &str,
    artist_name: &str,
    lang: &str,
) -> Option<BioResult> {
    // Search for "{album_title} {artist_name} album"
    let query = format!("{album_title} {artist_name} album");
    let search_url = format!(
        "https://{lang}.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&srnamespace=0&srlimit=3&format=json",
        urlencoding::encode(&query)
    );
    let resp = client
        .get(&search_url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let results = data.pointer("/query/search")?.as_array()?;
    if results.is_empty() {
        return None;
    }

    // Try first few search results — pick the best match
    let album_lower = album_title.to_lowercase();
    let title = results
        .iter()
        .find_map(|r| {
            let t = r["title"].as_str()?;
            if t.to_lowercase().contains(&album_lower) {
                Some(t.to_string())
            } else {
                None
            }
        })
        .or_else(|| {
            results
                .first()?
                .get("title")?
                .as_str()
                .map(|s| s.to_string())
        })?;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Fetch the extract
    let extract_url = format!(
        "https://{lang}.wikipedia.org/w/api.php?action=query&prop=extracts&exintro=1&explaintext=1&titles={}&format=json",
        urlencoding::encode(&title)
    );
    let wp_resp = client
        .get(&extract_url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !wp_resp.status().is_success() {
        return None;
    }
    let wp_data: serde_json::Value = wp_resp.json().await.ok()?;
    let pages = wp_data.pointer("/query/pages")?;
    let page = pages.as_object()?.values().next()?;
    let extract = page.get("extract")?.as_str()?;
    if extract.len() < 50 {
        return None;
    }
    Some(BioResult {
        text: extract.trim().to_string(),
        source: "wikipedia".to_string(),
        source_url: Some(format!(
            "https://{lang}.wikipedia.org/wiki/{}",
            urlencoding::encode(&title)
        )),
        license: "CC-BY-SA-4.0".to_string(),
        lang: lang.to_string(),
    })
}

/// Last.fm album.getInfo → wiki summary.
async fn fetch_album_bio_lastfm(
    client: &reqwest::Client,
    artist_name: &str,
    album_title: &str,
    api_key: &str,
    lang: &str,
) -> Option<BioResult> {
    let resp = client
        .get("https://ws.audioscrobbler.com/2.0/")
        .query(&[
            ("method", "album.getInfo"),
            ("artist", artist_name),
            ("album", album_title),
            ("api_key", api_key),
            ("format", "json"),
            ("lang", lang),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let wiki = data
        .pointer("/album/wiki/content")
        .and_then(|v| v.as_str())?;
    let clean = strip_html(wiki);
    if clean.len() < 50 {
        return None;
    }
    let source_url = data
        .pointer("/album/url")
        .and_then(|v| v.as_str())
        .map(String::from);
    Some(BioResult {
        text: clean,
        source: "lastfm".to_string(),
        source_url,
        license: "CC-BY-SA-3.0".to_string(),
        lang: "fr".to_string(),
    })
}

fn strip_html(s: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    re.replace_all(s, "").trim().to_string()
}

/// Batch enrich artist bios: Wikipedia FR via Wikidata + Last.fm fallback.
/// Submits each bio to mozaiklabs.fr community API.
pub async fn batch_enrich_artist_bios(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    lang: &str,
) {
    let lang = if lang.is_empty() { "fr" } else { lang };
    let artist_repo = crate::db::artist_repo::ArtistRepo::with_backend(db.clone());
    let artists = match artist_repo.list_without_bio() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_artist_bio_list_failed");
            return;
        }
    };

    if artists.is_empty() {
        info!("batch_artist_bio_skip_all_have_bios");
        return;
    }

    info!(count = artists.len(), "batch_artist_bio_enrichment_started");

    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    let lastfm_key = std::env::var("LASTFM_API_KEY")
        .or_else(|_| std::env::var("TUNE_LASTFM_KEY"))
        .unwrap_or_default();

    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    let mut enriched = 0u32;
    let mut failed = 0u32;

    for (artist_id, name, mbid) in &artists {
        if mbid.is_empty() {
            // No MusicBrainz ID — can't fetch via Wikidata, try Last.fm only
            if lastfm_key.is_empty() {
                failed += 1;
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        } else {
            // MusicBrainz rate limit: 1 req/s + margin for sub-requests
            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        }

        match fetch_artist_bio(&client, mbid, name, &lastfm_key, lang).await {
            Some(bio) => {
                artist_repo
                    .update_bio_full(
                        *artist_id,
                        &bio.text,
                        &bio.source,
                        bio.source_url.clone(),
                        &bio.license,
                        &bio.lang,
                    )
                    .ok();
                enriched += 1;
                info!(
                    artist_id,
                    artist = %name,
                    bio_len = bio.text.len(),
                    source = %bio.source,
                    "batch_artist_bio_enriched"
                );

                // Submit to mozaiklabs.fr community
                if !instance_id.is_empty() {
                    let mbid = mbid.clone();
                    let name = name.clone();
                    let instance_id = instance_id.clone();
                    let bio = bio.text.clone();
                    tokio::spawn(async move {
                        submit_artist_bio(
                            "https://mozaiklabs.fr",
                            &mbid,
                            &name,
                            &instance_id,
                            &bio,
                        )
                        .await
                        .ok();
                    });
                }
            }
            None => {
                failed += 1;
                debug!(artist_id, artist = %name, "batch_artist_bio_not_found");
            }
        }
    }

    info!(
        total = artists.len(),
        enriched, failed, "batch_artist_bio_enrichment_complete"
    );

    settings
        .set(
            "artist_bio_enrich_result",
            &serde_json::json!({
                "total": artists.len(),
                "enriched": enriched,
                "failed": failed,
            })
            .to_string(),
        )
        .ok();
}

/// Batch enrich album bios: Wikipedia FR → Wikipedia EN → Last.fm fallback.
/// Processes 4 albums concurrently for speed.
pub async fn batch_enrich_album_bios(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    lang: &str,
) {
    let lang = if lang.is_empty() { "fr" } else { lang };
    let album_repo = crate::db::album_repo::AlbumRepo::with_backend(db.clone());
    let albums = match album_repo.list_without_bio() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_album_bio_list_failed");
            return;
        }
    };

    if albums.is_empty() {
        info!("batch_album_bio_skip_all_have_bios");
        return;
    }

    info!(count = albums.len(), "batch_album_bio_enrichment_started");

    let lastfm_key = std::env::var("LASTFM_API_KEY")
        .or_else(|_| std::env::var("TUNE_LASTFM_KEY"))
        .unwrap_or_default();

    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();

    let mut enriched = 0u32;
    let mut failed = 0u32;
    let album_repo = crate::db::album_repo::AlbumRepo::with_backend(db.clone());

    for (album_id, title, artist_name) in albums.iter() {
        // Gentle rate limit: 2s between each album to avoid Wikipedia/Last.fm bans
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

        let artist = artist_name.as_deref().unwrap_or("Unknown Artist");
        let result = fetch_album_bio(&client, artist, title, &lastfm_key, lang).await;

        match result {
            Some(bio) => {
                album_repo
                    .update_bio_full(
                        *album_id,
                        &bio.text,
                        &bio.source,
                        bio.source_url.clone(),
                        &bio.license,
                        &bio.lang,
                    )
                    .ok();
                enriched += 1;
                info!(
                    album_id,
                    album = %title,
                    artist = %artist,
                    bio_len = bio.text.len(),
                    source = %bio.source,
                    "batch_album_bio_enriched"
                );
            }
            None => {
                failed += 1;
                debug!(album_id, album = %title, "batch_album_bio_not_found");
            }
        }
    }

    info!(
        total = albums.len(),
        enriched, failed, "batch_album_bio_enrichment_complete"
    );

    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db);
    settings
        .set(
            "album_bio_enrich_result",
            &serde_json::json!({
                "total": albums.len(),
                "enriched": enriched,
                "failed": failed,
            })
            .to_string(),
        )
        .ok();
}

/// Submit an artist bio to mozaiklabs.fr community.
async fn submit_artist_bio(
    base_url: &str,
    mbid: &str,
    artist_name: &str,
    instance_id: &str,
    bio: &str,
) -> Result<(), String> {
    let url = format!(
        "{}/api/v1/community/artist-bios",
        base_url.trim_end_matches('/')
    );
    let client = crate::http::client::shared();

    let resp = client
        .post(&url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "mbid": mbid,
            "artist_name": artist_name,
            "instance_id": instance_id,
            "bio": bio,
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("submit artist bio failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("submit bio failed: {}", resp.status()));
    }

    debug!(mbid, artist_name, "community_artist_bio_submitted");
    Ok(())
}
