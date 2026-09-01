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

    // 3. TheAudioDB fallback (niche artists Wikipedia/Last.fm miss)
    if let Some(bio) = fetch_artist_bio_theaudiodb(client, mbid, lang).await {
        if bio.text.len() > 50 {
            return Some(bio);
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
    // Sans MBID il n'y a rien à demander : l'URL deviendrait
    // `.../ws/2/artist/?inc=url-rels`, une requête que MusicBrainz rejette,
    // mais qui consommerait quand même le budget d'une requête par seconde.
    // Depuis #1311 les artistes sans MBID entrent dans la boucle, ce chemin
    // est donc réellement emprunté — `fetch_artist_bio_theaudiodb` se garde
    // déjà de la même façon.
    if mbid.is_empty() {
        return None;
    }
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
    bio_lastfm_depuis_json(&data, "/artist/bio/content", "/artist/url", lang)
}

/// Construit le `BioResult` d'une reponse Last.fm — la part testable des deux
/// recuperations Last.fm (artiste et album), dont les corps etaient identiques
/// a deux pointeurs JSON pres.
///
/// ## Ce qui change ici (#1849)
///
/// `lang` est desormais **inscrit dans le resultat**. Les deux fonctions
/// appelantes recevaient deja la langue demandee et la transmettaient bien a
/// Last.fm (`("lang", lang)`), mais estampillaient ensuite `lang: "fr"` en dur.
/// Ce champ finit dans `artists.bio_lang` / `albums.bio_lang` via
/// `update_bio_full`, et c'est lui que les routes interrogent pour decider si
/// la bio stockee convient au lecteur : une bio rapportee en anglais etait
/// etiquetee francaise, donc rejetee pour tout lecteur anglophone a chaque
/// ouverture de fiche. La promesse de #2126 — « un re-enrichissement
/// renseignera `bio_lang` » — ne tenait pas pour ce chemin.
///
/// ⚠️ Reserve : Last.fm retombe silencieusement sur l'anglais quand il n'a rien
/// dans la langue demandee, et ne dit pas ce qu'il a rendu. On enregistre donc
/// l'intention, pas une certitude — ce qui reste strictement plus juste que
/// « fr » en toute circonstance.
fn bio_lastfm_depuis_json(
    data: &serde_json::Value,
    pointeur_texte: &str,
    pointeur_url: &str,
    lang: &str,
) -> Option<BioResult> {
    let bio = data.pointer(pointeur_texte).and_then(|v| v.as_str())?;
    let clean = strip_html(bio);
    if clean.len() < 50 {
        return None;
    }
    let source_url = data
        .pointer(pointeur_url)
        .and_then(|v| v.as_str())
        .map(String::from);
    Some(BioResult {
        text: clean,
        source: "lastfm".to_string(),
        source_url,
        license: "CC-BY-SA-3.0".to_string(),
        lang: lang.to_ascii_lowercase(),
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

    // 4. TheAudioDB fallback (niche albums Wikipedia/Last.fm miss)
    if let Some(bio) = fetch_album_bio_theaudiodb(client, artist_name, album_title, lang).await {
        if bio.text.len() > 50 {
            return Some(bio);
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
    bio_lastfm_depuis_json(&data, "/album/wiki/content", "/album/url", lang)
}

fn strip_html(s: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    re.replace_all(s, "").trim().to_string()
}

/// Les noms sous lesquels la clé Last.fm peut avoir été posée dans
/// l'environnement, du plus officiel au plus ancien.
///
/// `TUNE_LASTFM_API_KEY` est celui que `.env.tune.example` documente et que
/// `config.rs` lit (`env_str("TUNE_LASTFM_API_KEY", ..)`) ; `artwork.rs` lit
/// bien les trois. L'enrichissement des biographies était le seul endroit à
/// ignorer le nom officiel — voir `cle_lastfm_dans`.
const NOMS_CLE_LASTFM: [&str; 3] = ["TUNE_LASTFM_API_KEY", "LASTFM_API_KEY", "TUNE_LASTFM_KEY"];

/// Première valeur non vide parmi [`NOMS_CLE_LASTFM`], lue par `lecture`.
///
/// ## Ce qui manquait (#1311)
///
/// Ce module lisait `LASTFM_API_KEY` puis `TUNE_LASTFM_KEY`, et **jamais**
/// `TUNE_LASTFM_API_KEY` — le seul nom que la documentation d'installation
/// et `config.rs` retiennent. Une installation configurée comme la doc le
/// dit se retrouvait donc avec une clé vide ici : le repli Last.fm, qui est
/// la seule source de biographie pour un artiste sans MBID, était court-
/// circuité en silence (`if lastfm_key.is_empty() { failed += 1; continue }`).
///
/// Le paramètre `lecture` rend la liste des noms vérifiable sans toucher à
/// l'environnement du processus de test.
fn cle_lastfm_dans(lecture: impl Fn(&str) -> Option<String>) -> String {
    NOMS_CLE_LASTFM
        .iter()
        .find_map(|nom| {
            lecture(nom)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_default()
}

/// La clé Last.fm telle que l'utilisateur l'a réellement rangée : d'abord le
/// réglage saisi dans Tune, sinon l'environnement.
///
/// `routes/lastfm_social.rs` écrit la clé de l'interface dans le réglage
/// `lastfm_api_key`. Personne ne la relisait ici : un utilisateur qui saisit
/// sa clé dans Tune n'en tirait aucune biographie (#1311).
fn cle_lastfm_avec_reglage(reglage: Option<String>) -> String {
    if let Some(depuis_reglages) = reglage
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return depuis_reglages;
    }
    cle_lastfm_dans(|nom| std::env::var(nom).ok())
}

/// Idem, en allant chercher le réglage dans la base.
fn cle_lastfm(db: &std::sync::Arc<dyn crate::db::backend::DbBackend>) -> String {
    let reglages = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    cle_lastfm_avec_reglage(reglages.get("lastfm_api_key").ok().flatten())
}

/// TheAudioDB API key. Defaults to the public test key ("2"); production
/// installs can override with a Patreon key via env.
fn theaudiodb_key() -> String {
    std::env::var("THEAUDIODB_API_KEY")
        .or_else(|_| std::env::var("TUNE_THEAUDIODB_KEY"))
        .unwrap_or_else(|_| "2".to_string())
}

/// Pick a per-language field from a TheAudioDB object (e.g. `strBiography` +
/// `FR`/`EN`), preferring `lang`, then English. Returns (text, resolved_lang).
fn pick_lang_field(obj: &serde_json::Value, prefix: &str, lang: &str) -> Option<(String, String)> {
    let try_lang = |l: &str| {
        let key = format!("{prefix}{l}");
        obj.get(key.as_str())
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(v) = try_lang(&lang.to_uppercase()) {
        return Some((v, lang.to_lowercase()));
    }
    try_lang("EN").map(|v| (v, "en".to_string()))
}

/// TheAudioDB artist biography by MusicBrainz ID (fallback for niche artists
/// that Wikipedia/Last.fm miss). Honors `lang`.
async fn fetch_artist_bio_theaudiodb(
    client: &reqwest::Client,
    mbid: &str,
    lang: &str,
) -> Option<BioResult> {
    if mbid.is_empty() {
        return None;
    }
    let key = theaudiodb_key();
    let url = format!("https://www.theaudiodb.com/api/v1/json/{key}/artist-mb.php?i={mbid}");
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let artist = data["artists"].as_array()?.first()?;
    let (bio, bio_lang) = pick_lang_field(artist, "strBiography", lang)?;
    if bio.len() < 50 {
        return None;
    }
    let source_url = artist["idArtist"]
        .as_str()
        .map(|id| format!("https://www.theaudiodb.com/artist/{id}"));
    Some(BioResult {
        text: bio,
        source: "theaudiodb".to_string(),
        source_url,
        license: "TheAudioDB".to_string(),
        lang: bio_lang,
    })
}

/// TheAudioDB album description by artist + title. Honors `lang`.
async fn fetch_album_bio_theaudiodb(
    client: &reqwest::Client,
    artist_name: &str,
    album_title: &str,
    lang: &str,
) -> Option<BioResult> {
    let key = theaudiodb_key();
    let url = format!(
        "https://www.theaudiodb.com/api/v1/json/{key}/searchalbum.php?s={}&a={}",
        urlencoding::encode(artist_name),
        urlencoding::encode(album_title)
    );
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: serde_json::Value = resp.json().await.ok()?;
    let album = data["album"].as_array()?.first()?;
    let (bio, bio_lang) = pick_lang_field(album, "strDescription", lang)?;
    if bio.len() < 50 {
        return None;
    }
    let source_url = album["idAlbum"]
        .as_str()
        .map(|id| format!("https://www.theaudiodb.com/album/{id}"));
    Some(BioResult {
        text: bio,
        source: "theaudiodb".to_string(),
        source_url,
        license: "TheAudioDB".to_string(),
        lang: bio_lang,
    })
}

/// Batch enrich artist bios: Wikipedia FR via Wikidata + Last.fm fallback.
/// Submits each bio to mozaiklabs.fr community API.
/// Le bilan d'une passe de biographies, tel qu'il est rangé dans les réglages
/// pour que l'interface puisse enfin le montrer.
///
/// ## Le bilan était écrit ; personne ne le lisait (#1311)
///
/// Les deux passes rangeaient déjà `total` / `enriched` / `failed` sous
/// `artist_bio_enrich_result` et `album_bio_enrich_result` à la fin de leur
/// travail. Une recherche de ces deux clés dans tout le dépôt ne rendait
/// qu'une seule ligne chacune : celle de l'**écriture**. Aucune route, aucun
/// écran ne les relisait — le bilan était un mécanisme juste, sans appelant.
///
/// C'est ce qui rend « les bios ne sont pas disponibles » impossible à
/// instruire, et c'est le vrai défaut derrière ce ticket : quand une passe
/// rentre à vide, Tune le SAIT, l'écrit, et n'en dit rien. L'utilisateur ne
/// peut pas distinguer une passe qui n'a trouvé personne à enrichir, une passe
/// dont toutes les sources ont répondu « je n'ai rien », et une passe qui
/// n'avait aucune source à interroger. Ce sont trois causes différentes, avec
/// trois remèdes différents, derrière un seul écran vide.
///
/// Deux champs s'ajoutent donc à ce que la passe rangeait déjà :
///
/// - `sans_source` — combien de candidats n'avaient **aucune** source
///   possible. Ce sont des échecs *certains*, connus d'avance, et ils se
///   confondaient jusqu'ici avec les « pas trouvé » dans le `failed` global.
/// - `fini_le` — sans horodatage, un bilan resservi ne dit pas s'il vient de
///   la passe qu'on vient de lancer ou d'une passe d'il y a trois semaines.
///
/// `lastfm_configure` accompagne les deux : c'est le réglage que
/// l'utilisateur peut corriger lui-même.
///
/// La forme est **la même pour les deux passes**, pour que l'écran n'ait
/// qu'une structure à lire.
fn bilan_de_passe(
    total: usize,
    enriched: u32,
    failed: u32,
    sans_source: usize,
    lastfm_configure: bool,
) -> String {
    serde_json::json!({
        "total": total,
        "enriched": enriched,
        "failed": failed,
        "sans_source": sans_source,
        "lastfm_configure": lastfm_configure,
        "fini_le": chrono::Utc::now().to_rfc3339(),
    })
    .to_string()
}

pub async fn batch_enrich_artist_bios(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    lang: &str,
) {
    batch_enrich_artist_bios_scoped(db, lang, None).await
}

/// Variante à portée (#1660) : seuls les artistes du répertoire demandé sont
/// candidats. `None` = passe complète, code identique.
pub async fn batch_enrich_artist_bios_scoped(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    lang: &str,
    scope: Option<crate::metadata::enrich_scope::EnrichScope>,
) {
    let lang = if lang.is_empty() { "fr" } else { lang };
    let artist_repo = crate::db::artist_repo::ArtistRepo::with_backend(db.clone());
    let mut artists = match artist_repo.list_without_bio() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_artist_bio_list_failed");
            return;
        }
    };
    if let Some(scope) = &scope {
        let avant = artists.len();
        artists.retain(|(id, ..)| scope.contient_artiste(*id));
        info!(
            dir = %scope.dir,
            retained = artists.len(),
            dropped = avant - artists.len(),
            "batch_artist_bio_scope_applied"
        );
    }

    if artists.is_empty() {
        info!("batch_artist_bio_skip_all_have_bios");
        return;
    }

    let client = crate::http::client::builder()
        .user_agent(MB_USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    let lastfm_key = cle_lastfm(&db);

    // Le journal dit maintenant ce qui départage une passe muette d'une passe
    // qui travaille : combien d'artistes n'ont pas de MBID (donc pas de chemin
    // Wikipédia/Wikidata ni TheAudioDB), et si une clé Last.fm — la seule
    // source par le NOM — est configurée. C'est exactement l'extrait que
    // #1311 réclamait sans jamais l'obtenir.
    let sans_mbid = artists.iter().filter(|(_, _, m)| m.is_empty()).count();
    info!(
        count = artists.len(),
        sans_mbid,
        lastfm_configure = !lastfm_key.is_empty(),
        "batch_artist_bio_enrichment_started"
    );
    if sans_mbid > 0 && lastfm_key.is_empty() {
        warn!(
            sans_mbid,
            "batch_artist_bio_sans_source_par_nom: ces artistes n'ont ni MBID ni cle Last.fm — aucune source ne peut les servir"
        );
    }

    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());

    let mut enriched = 0u32;
    let mut failed = 0u32;

    for (artist_id, name, mbid) in &artists {
        if mbid.is_empty() {
            // No MusicBrainz ID — can't fetch via Wikidata, try Last.fm only.
            //
            // Cette branche existait depuis l'origine mais était INATTEIGNABLE :
            // `sql::list_without_bio` exigeait un MBID non vide, donc `mbid`
            // ne pouvait pas être vide ici. La requête ne filtre plus (#1311).
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
                // Community contribution of this bio now goes through the single
                // bio_sync upload path (POST /community/bios); the redundant direct
                // push to /community/artist-bios was removed.
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

    // `sans_source` : un artiste sans MBID **et** sans clé Last.fm n'a aucune
    // source — ni Wikidata/Wikipédia ni TheAudioDB, qui partent tous du MBID,
    // ni la recherche par nom, qui demande la clé. La boucle ci-dessus les
    // compte dans `failed` sans les distinguer ; ils sont pourtant les seuls
    // dont l'échec était connu AVANT la première requête, et le seul cas que
    // l'utilisateur peut corriger lui-même (en posant sa clé).
    let sans_source = if lastfm_key.is_empty() { sans_mbid } else { 0 };

    settings
        .set(
            "artist_bio_enrich_result",
            &bilan_de_passe(
                artists.len(),
                enriched,
                failed,
                sans_source,
                !lastfm_key.is_empty(),
            ),
        )
        .ok();
}

/// Batch enrich album bios: Wikipedia FR → Wikipedia EN → Last.fm fallback.
/// Processes 4 albums concurrently for speed.
pub async fn batch_enrich_album_bios(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    lang: &str,
) {
    batch_enrich_album_bios_scoped(db, lang, None).await
}

/// Variante à portée (#1660) : seuls les albums du répertoire demandé sont
/// candidats. `None` = passe complète, code identique.
pub async fn batch_enrich_album_bios_scoped(
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
    lang: &str,
    scope: Option<crate::metadata::enrich_scope::EnrichScope>,
) {
    let lang = if lang.is_empty() { "fr" } else { lang };
    let album_repo = crate::db::album_repo::AlbumRepo::with_backend(db.clone());
    let mut albums = match album_repo.list_without_bio() {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "batch_album_bio_list_failed");
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
            "batch_album_bio_scope_applied"
        );
    }

    if albums.is_empty() {
        info!("batch_album_bio_skip_all_have_bios");
        return;
    }

    let lastfm_key = cle_lastfm(&db);
    info!(
        count = albums.len(),
        lastfm_configure = !lastfm_key.is_empty(),
        "batch_album_bio_enrichment_started"
    );

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
    // Chemin sœur de la passe artistes, et c'est bien pour cela qu'on écrit ce
    // zéro plutôt que de laisser le champ de côté : `fetch_album_bio` commence
    // par Wikipédia (langue demandée, puis anglais), qui ne réclame **aucune**
    // clé. Un album a donc toujours au moins une source à interroger, alors
    // qu'un artiste sans MBID n'en a aucune sans clé Last.fm. Le champ garde
    // la même forme pour les deux passes — l'écran n'a qu'une structure à
    // lire — et il dit ici une vérité mesurée, pas une valeur par défaut.
    let sans_source = 0usize;
    settings
        .set(
            "album_bio_enrich_result",
            &bilan_de_passe(
                albums.len(),
                enriched,
                failed,
                sans_source,
                !lastfm_key.is_empty(),
            ),
        )
        .ok();
}

#[cfg(test)]
mod tests_bilan_de_passe {
    use super::bilan_de_passe;

    /// #1311 — le bilan doit porter de quoi DISTINGUER les causes d'une passe
    /// rentrée à vide, pas seulement son décompte d'échecs.
    ///
    /// `failed` seul confond « la source n'avait rien » et « il n'y avait pas
    /// de source à interroger ». Ce sont deux situations différentes : la
    /// première ne se corrige pas côté utilisateur, la seconde se corrige en
    /// posant une clé Last.fm.
    ///
    /// Contre-épreuve : retirer `sans_source` (ou `lastfm_configure`) de
    /// `bilan_de_passe` fait rougir ce test.
    #[test]
    fn le_bilan_distingue_l_echec_certain_du_pas_trouve() {
        let brut = bilan_de_passe(120, 0, 120, 118, false);
        let v: serde_json::Value = serde_json::from_str(&brut).expect("bilan JSON");

        assert_eq!(v["total"], 120);
        assert_eq!(v["enriched"], 0);
        assert_eq!(v["failed"], 120);
        assert_eq!(
            v["sans_source"], 118,
            "les candidats sans aucune source doivent se compter a part"
        );
        assert_eq!(
            v["lastfm_configure"], false,
            "l'ecran doit pouvoir dire a l'utilisateur ce qu'il peut corriger"
        );
        assert!(
            v["fini_le"].as_str().is_some_and(|d| d.len() >= 20),
            "un bilan sans horodatage ne dit pas s'il date de la passe qu'on vient de lancer"
        );
    }

    /// Témoin : le décompte historique ne change pas de nom ni de type.
    /// Un écran qui lisait déjà `total`/`enriched`/`failed` continue de les
    /// trouver — ce test reste vert avant comme après le correctif.
    #[test]
    fn les_champs_historiques_restent_en_place() {
        let brut = bilan_de_passe(7, 5, 2, 0, true);
        let v: serde_json::Value = serde_json::from_str(&brut).expect("bilan JSON");
        assert_eq!(v["total"], 7);
        assert_eq!(v["enriched"], 5);
        assert_eq!(v["failed"], 2);
    }
}

#[cfg(test)]
mod tests_cle_lastfm {
    use super::{cle_lastfm_avec_reglage, cle_lastfm_dans};

    /// #1311 — le nom documenté était le seul que ce module ne lisait pas.
    ///
    /// `.env.tune.example` et `config.rs` ne connaissent que
    /// `TUNE_LASTFM_API_KEY`. `bio_batch` cherchait `LASTFM_API_KEY` puis
    /// `TUNE_LASTFM_KEY` : une installation configurée selon la doc n'avait
    /// donc pas de clé ici, et le repli Last.fm — seule source par le nom —
    /// ne partait jamais.
    ///
    /// Contre-épreuve : retirer `"TUNE_LASTFM_API_KEY"` de `NOMS_CLE_LASTFM`
    /// fait rougir ce test.
    #[test]
    fn le_nom_documente_de_la_cle_est_lu() {
        let cle = cle_lastfm_dans(|nom| {
            (nom == "TUNE_LASTFM_API_KEY").then(|| "cle-de-la-doc".to_string())
        });
        assert_eq!(
            cle, "cle-de-la-doc",
            "TUNE_LASTFM_API_KEY est le nom que la doc et config.rs retiennent"
        );
    }

    /// Les deux noms historiques restent acceptés : personne ne doit perdre
    /// une clé qui fonctionnait.
    #[test]
    fn les_noms_historiques_restent_acceptes() {
        assert_eq!(
            cle_lastfm_dans(|nom| (nom == "LASTFM_API_KEY").then(|| "ancienne".to_string())),
            "ancienne"
        );
        assert_eq!(
            cle_lastfm_dans(|nom| (nom == "TUNE_LASTFM_KEY").then(|| "tres-ancienne".to_string())),
            "tres-ancienne"
        );
    }

    /// Une variable posée mais vide ne doit pas masquer la suivante.
    #[test]
    fn une_variable_vide_ne_masque_pas_les_suivantes() {
        let cle = cle_lastfm_dans(|nom| match nom {
            "TUNE_LASTFM_API_KEY" => Some("   ".to_string()),
            "LASTFM_API_KEY" => Some("la-vraie".to_string()),
            _ => None,
        });
        assert_eq!(cle, "la-vraie");
    }

    /// La clé saisie dans l'interface (réglage `lastfm_api_key`) prime, et
    /// surtout : elle est enfin lue. Aucun chemin ne la consultait (#1311).
    #[test]
    fn la_cle_saisie_dans_l_interface_est_prise_en_compte() {
        assert_eq!(
            cle_lastfm_avec_reglage(Some("  cle-interface  ".to_string())),
            "cle-interface",
            "le reglage lastfm_api_key ecrit par l'interface doit servir a l'enrichissement"
        );
    }
}

#[cfg(test)]
mod tests_bio_lastfm {
    use super::bio_lastfm_depuis_json;
    use serde_json::json;

    fn reponse_artiste(texte: &str) -> serde_json::Value {
        json!({
            "artist": {
                "bio": { "content": texte },
                "url": "https://www.last.fm/music/Pink+Floyd"
            }
        })
    }

    const ASSEZ_LONG: &str =
        "Pink Floyd were an English rock band formed in London in nineteen sixty-five.";

    /// Le defaut de #1849 : la langue demandee etait jetee et remplacee par
    /// « fr » en dur, quelle que soit la langue reclamee a Last.fm.
    ///
    /// Contre-epreuve (mesuree) : remettre `lang: "fr".to_string()` dans
    /// `bio_lastfm_depuis_json` fait rougir les TROIS tests de langue de ce
    /// module — celui-ci, `la_bio_d_album_partage_le_parseur_et_la_langue` et
    /// `la_langue_est_normalisee_en_minuscules` — et eux seuls. Les trois
    /// autres (balisage, bio trop courte, reponse vide) restent verts : ils
    /// gardent le parseur, pas la langue, et ne font donc pas doublon.
    #[test]
    fn la_langue_demandee_est_inscrite_et_non_fr_en_dur() {
        let bio = bio_lastfm_depuis_json(
            &reponse_artiste(ASSEZ_LONG),
            "/artist/bio/content",
            "/artist/url",
            "en",
        )
        .expect("une bio assez longue doit etre retenue");
        assert_eq!(
            bio.lang, "en",
            "une bio reclamee en anglais ne doit pas etre etiquetee francaise"
        );
    }

    /// Une bio d'album passe par le meme parseur, aux pointeurs pres.
    #[test]
    fn la_bio_d_album_partage_le_parseur_et_la_langue() {
        let data = json!({
            "album": {
                "wiki": { "content": ASSEZ_LONG },
                "url": "https://www.last.fm/music/Pink+Floyd/Animals"
            }
        });
        let bio = bio_lastfm_depuis_json(&data, "/album/wiki/content", "/album/url", "de")
            .expect("une bio assez longue doit etre retenue");
        assert_eq!(bio.lang, "de");
        assert_eq!(
            bio.source_url.as_deref(),
            Some("https://www.last.fm/music/Pink+Floyd/Animals")
        );
    }

    /// `fr-FR` et `FR` designent la meme chose que `fr` pour `langue_convient`,
    /// qui compare en minuscules : on normalise a l'ecriture plutot que de
    /// laisser une casse etrangere dormir en base.
    #[test]
    fn la_langue_est_normalisee_en_minuscules() {
        let bio = bio_lastfm_depuis_json(
            &reponse_artiste(ASSEZ_LONG),
            "/artist/bio/content",
            "/artist/url",
            "EN",
        )
        .expect("une bio assez longue doit etre retenue");
        assert_eq!(bio.lang, "en");
    }

    /// Le balisage de Last.fm ne doit pas atterrir dans la fiche.
    #[test]
    fn le_balisage_html_est_retire() {
        let brut = format!("<p>{ASSEZ_LONG}</p> <a href=\"x\">Read more</a>");
        let bio = bio_lastfm_depuis_json(
            &reponse_artiste(&brut),
            "/artist/bio/content",
            "/artist/url",
            "en",
        )
        .expect("une bio assez longue doit etre retenue");
        assert!(!bio.text.contains('<'), "balisage restant : {}", bio.text);
        assert!(bio.text.contains("Pink Floyd were an English rock band"));
    }

    /// Last.fm rend un pied de page (« Read more on Last.fm ») pour les
    /// artistes qu'il ne connait pas : trop court, donc pas une biographie.
    #[test]
    fn une_bio_trop_courte_est_refusee() {
        assert!(
            bio_lastfm_depuis_json(
                &reponse_artiste("<a href=\"x\">Read more</a>"),
                "/artist/bio/content",
                "/artist/url",
                "en",
            )
            .is_none()
        );
    }

    /// Champ absent : rien a inscrire, surtout pas une bio vide etiquetee.
    #[test]
    fn une_reponse_sans_bio_ne_rend_rien() {
        assert!(
            bio_lastfm_depuis_json(&json!({}), "/artist/bio/content", "/artist/url", "en")
                .is_none()
        );
    }
}
