use quick_xml::escape::unescape;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};

const ITUNES_SEARCH_URL: &str = "https://itunes.apple.com/search";
const ITUNES_LOOKUP_URL: &str = "https://itunes.apple.com/lookup";
const APPLE_TOP_BASE: &str = "https://rss.marketingtools.apple.com/api/v2";
const USER_AGENT: &str = "Tune/2.0 (https://mozaiklabs.fr)";
/// Cache TTL for top podcasts (1 hour).
const TOP_CACHE_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Podcast {
    pub name: String,
    pub artist: String,
    pub feed_url: String,
    pub cover_url: String,
    pub description: String,
    pub episode_count: u32,
    pub source_id: String,
    /// Optional genre/category label (e.g. "News", "Music", "Culture").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// Genre IDs recognised by the Apple podcast charts API.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PodcastGenre {
    Arts = 1301,
    Comedy = 1303,
    Education = 1304,
    KidsAndFamily = 1305,
    HealthAndFitness = 1307,
    TvAndFilm = 1309,
    Music = 1310,
    News = 1311,
    ReligionAndSpirituality = 1314,
    Science = 1315,
    Sports = 1316,
    Technology = 1318,
    Business = 1321,
    Government = 1323,
    SocietyAndCulture = 1324,
    TrueCrime = 1325,
    History = 1326,
    Fiction = 1401,
}

impl PodcastGenre {
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            1301 => Some(Self::Arts),
            1303 => Some(Self::Comedy),
            1304 => Some(Self::Education),
            1305 => Some(Self::KidsAndFamily),
            1307 => Some(Self::HealthAndFitness),
            1309 => Some(Self::TvAndFilm),
            1310 => Some(Self::Music),
            1311 => Some(Self::News),
            1314 => Some(Self::ReligionAndSpirituality),
            1315 => Some(Self::Science),
            1316 => Some(Self::Sports),
            1318 => Some(Self::Technology),
            1321 => Some(Self::Business),
            1323 => Some(Self::Government),
            1324 => Some(Self::SocietyAndCulture),
            1325 => Some(Self::TrueCrime),
            1326 => Some(Self::History),
            1401 => Some(Self::Fiction),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Arts => "Arts",
            Self::Comedy => "Comedy",
            Self::Education => "Education",
            Self::KidsAndFamily => "Kids & Family",
            Self::HealthAndFitness => "Health & Fitness",
            Self::TvAndFilm => "TV & Film",
            Self::Music => "Music",
            Self::News => "News",
            Self::ReligionAndSpirituality => "Religion & Spirituality",
            Self::Science => "Science",
            Self::Sports => "Sports",
            Self::Technology => "Technology",
            Self::Business => "Business",
            Self::Government => "Government",
            Self::SocietyAndCulture => "Society & Culture",
            Self::TrueCrime => "True Crime",
            Self::History => "History",
            Self::Fiction => "Fiction",
        }
    }

    /// All available genres.
    pub fn all() -> &'static [PodcastGenre] {
        &[
            Self::Arts,
            Self::Comedy,
            Self::Education,
            Self::KidsAndFamily,
            Self::HealthAndFitness,
            Self::TvAndFilm,
            Self::Music,
            Self::News,
            Self::ReligionAndSpirituality,
            Self::Science,
            Self::Sports,
            Self::Technology,
            Self::Business,
            Self::Government,
            Self::SocietyAndCulture,
            Self::TrueCrime,
            Self::History,
            Self::Fiction,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodcastEpisode {
    pub title: String,
    pub description: String,
    pub audio_url: String,
    pub duration_ms: u64,
    pub published: String,
    pub cover_url: String,
}

/// Un flux lu : l'image de la chaîne (vide si le flux n'en déclare pas) et
/// ses épisodes.
#[derive(Debug, Clone)]
pub struct FluxPodcast {
    pub image_url: String,
    pub episodes: Vec<PodcastEpisode>,
}

pub struct PodcastService {
    client: Client,
    /// Adresse de la recherche Apple par identifiant. Fixe en production ;
    /// un banc la pointe vers un faux Apple local (#6770).
    lookup_base: String,
    /// Racine du palmarès Apple (`…/api/v2`). Fixe en production ; un banc
    /// la pointe vers le même faux Apple (#6770).
    top_base: String,
}

/// #6770 — le pays du palmarès d'où vient chaque identifiant Apple servi,
/// pour qu'une résolution demandée SANS pays (aucun client n'en envoie)
/// parte quand même dans la bonne boutique. Bornée : vidée au-delà.
fn pays_des_palmares() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static PAYS: OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        OnceLock::new();
    PAYS.get_or_init(Default::default)
}

const PAYS_DES_PALMARES_MAX: usize = 10_000;

/// Pourquoi un identifiant Apple du palmarès n'a pas donné de flux (#6770).
///
/// Le palmarès Apple ne porte que l'identifiant : le flux RSS se résout par
/// `lookup`. Les deux échecs ne se valent pas et ne se disent pas pareil :
/// un podcast que la boutique ne connaît pas, et un podcast qu'elle connaît
/// mais qui n'a AUCUN flux public (exclusivité Apple, par exemple).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchecResolution {
    /// Apple ne rend aucun résultat pour cet identifiant, ni dans la boutique
    /// du pays, ni dans la boutique par défaut.
    Introuvable,
    /// Apple connaît le podcast, mais ne publie pas d'adresse de flux.
    SansFluxPublic,
    /// Apple n'a pas répondu, ou pas en JSON.
    Reseau(String),
}

impl EchecResolution {
    /// Code stable, lu par les clients.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Introuvable => "podcast_introuvable",
            Self::SansFluxPublic => "podcast_sans_flux_public",
            Self::Reseau(_) => "podcast_resolution_reseau",
        }
    }

    /// Phrase à montrer telle quelle.
    pub fn message(&self) -> &'static str {
        match self {
            Self::Introuvable => "Ce podcast est introuvable chez Apple.",
            Self::SansFluxPublic => {
                "Ce podcast n'a pas de flux public : il ne peut être ni suivi ni écouté hors d'Apple Podcasts."
            }
            Self::Reseau(_) => "Apple Podcasts ne répond pas. Réessayez dans un instant.",
        }
    }
}

impl PodcastService {
    pub fn new() -> Self {
        Self {
            client: crate::http::client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .unwrap(),
            lookup_base: ITUNES_LOOKUP_URL.to_string(),
            top_base: APPLE_TOP_BASE.to_string(),
        }
    }

    /// Create a PodcastService reusing an existing reqwest client.
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            lookup_base: ITUNES_LOOKUP_URL.to_string(),
            top_base: APPLE_TOP_BASE.to_string(),
        }
    }

    /// Comme [`Self::with_client`], avec une autre adresse de `lookup` :
    /// réservé aux bancs, qui servent un faux Apple local.
    pub fn with_lookup_base(client: Client, lookup_base: &str) -> Self {
        Self {
            client,
            lookup_base: lookup_base.to_string(),
            top_base: APPLE_TOP_BASE.to_string(),
        }
    }

    /// Comme [`Self::with_lookup_base`], palmarès compris : réservé aux bancs.
    pub fn with_apple_bases(client: Client, top_base: &str, lookup_base: &str) -> Self {
        Self {
            client,
            lookup_base: lookup_base.to_string(),
            top_base: top_base.to_string(),
        }
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        country: &str,
        language: Option<&str>,
    ) -> Result<Vec<Podcast>, String> {
        let limit = limit.clamp(1, 50);
        let cc = if country.is_empty() { "US" } else { country };
        let query_lower = query.to_lowercase();
        let lang = language.unwrap_or("");

        // Check search cache (5 min TTL).
        type SearchCache = std::collections::HashMap<String, (Instant, Vec<Podcast>)>;
        static SEARCH_CACHE: OnceLock<Mutex<SearchCache>> = OnceLock::new();
        let cache = SEARCH_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
        let cache_key = format!("{cc}:{lang}:{query_lower}:{limit}");
        {
            let guard = cache.lock().await;
            if let Some((ts, data)) = guard.get(&cache_key)
                && ts.elapsed() < Duration::from_secs(300)
            {
                return Ok(data.clone());
            }
        }

        let mut params = vec![
            ("term", query.to_string()),
            ("media", "podcast".to_string()),
            ("limit", limit.to_string()),
            ("country", cc.to_string()),
        ];
        if !lang.is_empty() {
            params.push((
                "language",
                format!(
                    "{lang}_{}",
                    cc.to_uppercase().chars().take(2).collect::<String>()
                ),
            ));
        }

        let resp = self
            .client
            .get(ITUNES_SEARCH_URL)
            .query(&params)
            .send()
            .await
            .map_err(|e| format!("podcast search: {e}"))?;
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("podcast parse: {e}"))?;
        let results = data["results"].as_array().cloned().unwrap_or_default();
        let podcasts: Vec<Podcast> = results
            .iter()
            .filter_map(|r| {
                let feed_url = r["feedUrl"].as_str()?.to_string();
                let source_id = r["trackId"]
                    .as_u64()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| {
                        use md5::{Digest, Md5};
                        let mut h = Md5::new();
                        h.update(feed_url.as_bytes());
                        format!("{:x}", h.finalize())
                    });
                let category = r["primaryGenreName"].as_str().map(String::from);
                Some(Podcast {
                    name: r["trackName"].as_str()?.to_string(),
                    artist: r["artistName"].as_str().unwrap_or("").to_string(),
                    feed_url,
                    cover_url: r["artworkUrl600"]
                        .as_str()
                        .or_else(|| r["artworkUrl100"].as_str())
                        .unwrap_or("")
                        .to_string(),
                    description: r["description"]
                        .as_str()
                        .or_else(|| r["shortDescription"].as_str())
                        .unwrap_or("")
                        .to_string(),
                    episode_count: r["trackCount"].as_u64().unwrap_or(0) as u32,
                    source_id,
                    category,
                })
            })
            .collect();

        // Update cache.
        {
            let mut guard = cache.lock().await;
            if guard.len() > 200 {
                guard.retain(|_, (ts, _)| ts.elapsed() < Duration::from_secs(300));
            }
            guard.insert(cache_key, (Instant::now(), podcasts.clone()));
        }

        Ok(podcasts)
    }

    pub async fn get_episodes(
        &self,
        feed_url: &str,
        limit: usize,
    ) -> Result<Vec<PodcastEpisode>, String> {
        self.get_feed(feed_url, limit).await.map(|f| f.episodes)
    }

    /// Les épisodes ET l'image de la chaîne d'un flux — celle que l'abonnement
    /// met en cache (#5214) : un flux qui la change se reconnaît ici.
    pub async fn get_feed(&self, feed_url: &str, limit: usize) -> Result<FluxPodcast, String> {
        debug!(feed_url, "podcast_feed_fetching");
        let resp = self
            .client
            .get(feed_url)
            .header(
                "Accept",
                "application/rss+xml, application/xml, text/xml, */*",
            )
            .send()
            .await
            .map_err(|e| format!("podcast feed fetch error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("podcast feed HTTP {}: {}", resp.status(), feed_url));
        }
        let xml_text = resp
            .text()
            .await
            .map_err(|e| format!("podcast feed read: {e}"))?;
        debug!(feed_url, bytes = xml_text.len(), "podcast_feed_fetched");
        Ok(FluxPodcast {
            image_url: extract_channel_image(&xml_text),
            episodes: parse_rss(&xml_text, limit)?,
        })
    }

    /// Le pays — et le SEUL — que couvre [`Self::curated_french_podcasts`]
    /// (#3395).
    ///
    /// La sélection éditoriale est une liste écrite en dur, pas un
    /// fournisseur : elle ne sait pas filtrer par pays, et il n'existe rien à
    /// filtrer hors de France. Les routes annoncent donc ce pays au client
    /// plutôt que de servir du français sous un autre drapeau.
    pub const CURATED_COUNTRY: &'static str = "fr";

    /// La sélection éditoriale POUR un pays (#3395).
    ///
    /// Vide partout ailleurs qu'en France : mieux vaut une section vide, que
    /// le client sait masquer, qu'une liste française rendue quel que soit le
    /// pays demandé — c'est ce qui faisait croire que le sélecteur de pays
    /// était mort.
    pub fn curated_for_country(country: &str) -> Vec<Podcast> {
        if country.eq_ignore_ascii_case(Self::CURATED_COUNTRY) {
            Self::curated_french_podcasts()
        } else {
            Vec::new()
        }
    }

    /// Curated French podcasts from Radio France and other networks.
    ///
    /// Liste FRANÇAISE en dur — voir [`Self::curated_for_country`] pour
    /// l'usage sensible au pays.
    pub fn curated_french_podcasts() -> Vec<Podcast> {
        vec![
            // ── Radio France — France Inter ──
            Podcast {
                name: "Le sept neuf".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10241.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/01/197263c5-db60-4f6f-b5e9-94c5ddb901b0/1400x1400_sc_la-grande-matinale-paracuellos.jpg".into(),
                description: "La matinale de France Inter".into(),
                episode_count: 0,
                source_id: "rf-sept-neuf".into(),
                category: Some("News".into()),
            },
            Podcast {
                name: "Affaires sensibles".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_13940.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2023/04/7b50cf5f-f5bd-4dc4-8b1d-b08666768dcf/1400x1400_sc_affaires-sensibles.jpg".into(),
                description: "Les grandes affaires qui ont marqué l'actualité".into(),
                episode_count: 0,
                source_id: "rf-affaires-sensibles".into(),
                category: Some("Society & Culture".into()),
            },
            Podcast {
                name: "Le Masque et la Plume".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_14007.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/03/50cfc964-6b7a-43f0-b42d-f798ae8819ce/1400x1400_sc_sc-rf-omm-0000041327-ite.jpg".into(),
                description: "L'émission critique cinéma, littérature et théâtre".into(),
                episode_count: 0,
                source_id: "rf-masque-plume".into(),
                category: Some("Arts".into()),
            },
            Podcast {
                name: "Boomerang".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_13937.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/2c34c20f-2aa1-45af-89bd-0851a725c323/1400x1400_bommerang.jpg".into(),
                description: "L'interview culturelle d'Augustin Trapenard".into(),
                episode_count: 0,
                source_id: "rf-boomerang".into(),
                category: Some("Arts".into()),
            },
            Podcast {
                name: "La Terre au carré".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10212.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2023/05/786780d7-1273-4959-894c-52009150c1d2/1400x1400_sc_la-terre-au-carre.jpg".into(),
                description: "L'environnement et les sciences au quotidien par Mathieu Vidard".into(),
                episode_count: 0,
                source_id: "rf-terre-carre".into(),
                category: Some("Science".into()),
            },
            Podcast {
                name: "Le Grand Dimanche Soir".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_18153.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/03/a9d633cc-02b0-4cfa-aa22-9a2836d0af41/1400x1400_sc_le-grand-dimanche-soir.jpg".into(),
                description: "Le spectacle du dimanche soir de Charline Vanhoenacker".into(),
                episode_count: 0,
                source_id: "rf-grand-dimanche".into(),
                category: Some("Comedy".into()),
            },
            // ── Radio France — France Culture ──
            Podcast {
                name: "Les pieds sur terre".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10078.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/a7fc766f-3e49-45a0-a41f-5512d5c0f8c1/1400x1400_les-pieds-sur-terre.jpg".into(),
                description: "Documentaires et témoignages du quotidien".into(),
                episode_count: 0,
                source_id: "rf-pieds-terre".into(),
                category: Some("Society & Culture".into()),
            },
            Podcast {
                name: "Avec philosophie".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10467.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/09/7724d8b9-cd17-41f6-b802-5db0c171842b/1400x1400_avec-philosophie-2.jpg".into(),
                description: "La philosophie au quotidien par Géraldine Muhlmann".into(),
                episode_count: 0,
                source_id: "rf-avec-philosophie".into(),
                category: Some("Education".into()),
            },
            Podcast {
                name: "Les Midis de Culture".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_12360.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/04/3fe0b371-e504-4bbf-b90c-559111dc06fb/1400x1400_sc_sc_sc-fc-midis-de-culture-3000x3000-photo.jpg".into(),
                description: "Le magazine culturel de la mi-journée de France Culture".into(),
                episode_count: 0,
                source_id: "rf-midis-culture".into(),
                category: Some("Arts".into()),
            },
            Podcast {
                name: "Le Cours de l'histoire".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_18370.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/09/4918fee6-34f9-48e5-a35b-c5f72db2e48e/1400x1400_le-cours-de-lhistoire.jpg".into(),
                description: "L'histoire vivante racontée par Xavier Mauduit".into(),
                episode_count: 0,
                source_id: "rf-cours-histoire".into(),
                category: Some("History".into()),
            },
            Podcast {
                name: "LSD, la série documentaire".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_15333.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/4cdb4c4b-f35e-4cf8-ac7d-4e2c2f14e248/1400x1400_lsd.jpg".into(),
                description: "Quatre épisodes pour explorer un sujet en profondeur".into(),
                episode_count: 0,
                source_id: "rf-lsd-documentaire".into(),
                category: Some("Society & Culture".into()),
            },
            Podcast {
                name: "La Science CQFD".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_14312.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/01/c0baf6a7-e839-43d1-8465-3a3a5f53e2f4/1400x1400_la-science-cqfd.jpg".into(),
                description: "La science au quotidien, décryptée et accessible".into(),
                episode_count: 0,
                source_id: "rf-science-cqfd".into(),
                category: Some("Science".into()),
            },
            Podcast {
                name: "Concordance des temps".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10193.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/0b2c3d4e-f5a6-7890-bcde-f12345678901/1400x1400_concordance.jpg".into(),
                description: "Le passé éclaire le présent avec Jean-Noël Jeanneney".into(),
                episode_count: 0,
                source_id: "rf-concordance".into(),
                category: Some("History".into()),
            },
            Podcast {
                name: "Cultures monde".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_14256.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/1a2b3c4d-e5f6-7890-abcd-ef1234567890/1400x1400_cultures-monde.jpg".into(),
                description: "Un regard sur le monde contemporain à travers les cultures".into(),
                episode_count: 0,
                source_id: "rf-cultures-monde".into(),
                category: Some("Society & Culture".into()),
            },
            Podcast {
                name: "Les Matins de France Culture".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10075.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/2b3c4d5e-f6a7-8901-bcde-f23456789012/1400x1400_les-matins.jpg".into(),
                description: "La matinale culturelle et politique de France Culture".into(),
                episode_count: 0,
                source_id: "rf-matins-culture".into(),
                category: Some("News".into()),
            },
            Podcast {
                name: "La Conversation scientifique".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_16256.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/3c4d5e6f-a7b8-9012-cdef-345678901234/1400x1400_conversation-scientifique.jpg".into(),
                description: "Les grandes questions scientifiques par Étienne Klein".into(),
                episode_count: 0,
                source_id: "rf-conversation-scientifique".into(),
                category: Some("Science".into()),
            },
            Podcast {
                name: "Le vif de l'histoire".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_11739.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2023/06/21d18070-9a66-45aa-b484-3d094c39909a/1400x1400_sc_rf_omm_0000038825_ite.jpg".into(),
                description: "L'histoire racontée par Jean Lebrun".into(),
                episode_count: 0,
                source_id: "rf-vif-histoire".into(),
                category: Some("History".into()),
            },
            // ── Radio France — FIP ──
            Podcast {
                name: "Club Jazzafip".into(),
                artist: "FIP".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_12250.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/03/91179aa6-0e09-4748-a281-d0f7a3259a1b/1400x1400_sc_fip-clubjazzafip-3000x3000.jpg".into(),
                description: "Le jazz sur FIP".into(),
                episode_count: 0,
                source_id: "rf-jazzafip".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "Pop & Co".into(),
                artist: "FIP".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_18754.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2024/01/e7e2f4a0-5c3b-4f1e-b1c3-6d8e9f0a1b2c/1400x1400_sc_pop-and-co.jpg".into(),
                description: "La pop culture décryptée sur FIP".into(),
                episode_count: 0,
                source_id: "rf-pop-co".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "Certains l'aiment FIP".into(),
                artist: "FIP".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_18891.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2024/03/a1b2c3d4-e5f6-7890-abcd-ef1234567890/1400x1400_sc_certains-laiment-fip.jpg".into(),
                description: "Les coups de cœur musicaux de FIP".into(),
                episode_count: 0,
                source_id: "rf-certains-fip".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "FIP au Festival".into(),
                artist: "FIP".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_21032.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2024/06/b2c3d4e5-f6a7-8901-bcde-f23456789012/1400x1400_sc_fip-au-festival.jpg".into(),
                description: "FIP en direct des festivals".into(),
                episode_count: 0,
                source_id: "rf-fip-festival".into(),
                category: Some("Music".into()),
            },
            // ── Radio France — France Musique ──
            Podcast {
                name: "Classique ou pas".into(),
                artist: "France Musique".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_18994.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2024/01/c3d4e5f6-a7b8-9012-cdef-345678901234/1400x1400_sc_classique-ou-pas.jpg".into(),
                description: "La musique classique accessible et décomplexée".into(),
                episode_count: 0,
                source_id: "rf-classique-ou-pas".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "Le van Beethoven".into(),
                artist: "France Musique".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_16362.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/d4e5f6a7-b8c9-0123-def0-456789012345/1400x1400_sc_le-van-beethoven.jpg".into(),
                description: "Voyage musical en camping-car à travers l'Europe".into(),
                episode_count: 0,
                source_id: "rf-van-beethoven".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "Grands interprètes de la musique classique".into(),
                artist: "France Musique".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_11403.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/09/e5f6a7b8-c9d0-1234-ef01-567890123456/1400x1400_sc_grands-interpretes.jpg".into(),
                description: "Portraits des plus grands musiciens classiques".into(),
                episode_count: 0,
                source_id: "rf-grands-interpretes".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "Disques de légende".into(),
                artist: "France Musique".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_14017.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/09/4a5b6c7d-e8f9-0123-abcd-ef4567890123/1400x1400_disques-de-legende.jpg".into(),
                description: "Les albums qui ont marqué l'histoire de la musique".into(),
                episode_count: 0,
                source_id: "rf-disques-legende".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "Les Trésors de France Musique".into(),
                artist: "France Musique".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_16399.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/09/5b6c7d8e-f9a0-1234-bcde-f56789012345/1400x1400_tresors-france-musique.jpg".into(),
                description: "Les archives sonores exceptionnelles de France Musique".into(),
                episode_count: 0,
                source_id: "rf-tresors-fm".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "Banzzaï".into(),
                artist: "France Musique".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_11408.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/09/6c7d8e9f-a0b1-2345-cdef-678901234567/1400x1400_banzzai.jpg".into(),
                description: "Le jazz dans tous ses états sur France Musique".into(),
                episode_count: 0,
                source_id: "rf-banzzai".into(),
                category: Some("Music".into()),
            },
            Podcast {
                name: "MAXXI Classique".into(),
                artist: "France Musique".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_19498.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2024/01/7d8e9f0a-b1c2-3456-def0-789012345678/1400x1400_maxxi-classique.jpg".into(),
                description: "Le classique nouvelle génération".into(),
                episode_count: 0,
                source_id: "rf-maxxi-classique".into(),
                category: Some("Music".into()),
            },
            // ── Radio France — franceinfo ──
            Podcast {
                name: "8h30 franceinfo".into(),
                artist: "franceinfo".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_22737.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2024/01/8e9f0a1b-c2d3-4567-ef01-890123456789/1400x1400_8h30-franceinfo.jpg".into(),
                description: "L'interview politique de référence chaque matin".into(),
                episode_count: 0,
                source_id: "rf-8h30-franceinfo".into(),
                category: Some("News".into()),
            },
            Podcast {
                name: "Le vrai ou faux".into(),
                artist: "franceinfo".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_21498.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2024/01/9f0a1b2c-d3e4-5678-f012-901234567890/1400x1400_vrai-ou-faux.jpg".into(),
                description: "Le fact-checking de franceinfo".into(),
                episode_count: 0,
                source_id: "rf-vrai-faux".into(),
                category: Some("News".into()),
            },
        ]
    }

    /// Backward-compatible alias — returns the same curated list.
    pub fn radio_france_podcasts() -> Vec<Podcast> {
        Self::curated_french_podcasts()
    }

    /// L'adresse de `lookup` pour un identifiant Apple, dans la boutique d'un
    /// pays (#6770).
    ///
    /// Sans `country`, Apple cherche dans la boutique AMÉRICAINE : un podcast
    /// classé seulement en France y revient sans résultat (constaté le 22/10
    /// sur « Chroniques criminelles », 1509742508 : `resultCount` 0 sans
    /// pays, 1 avec `country=fr`).
    pub fn lookup_url(base: &str, apple_id: &str, country: Option<&str>) -> String {
        let mut url = format!("{base}?id={apple_id}&entity=podcast");
        if let Some(cc) = country.map(str::trim).filter(|c| !c.is_empty()) {
            url.push_str("&country=");
            url.push_str(&cc.to_lowercase());
        }
        url
    }

    /// Lit la réponse de `lookup` : le flux, ou la raison de son absence.
    fn flux_du_lookup(data: &serde_json::Value) -> Result<String, EchecResolution> {
        let Some(premier) = data["results"].as_array().and_then(|r| r.first()) else {
            return Err(EchecResolution::Introuvable);
        };
        premier["feedUrl"]
            .as_str()
            .filter(|u| !u.is_empty())
            .map(String::from)
            .ok_or(EchecResolution::SansFluxPublic)
    }

    async fn lookup(
        &self,
        apple_id: &str,
        country: Option<&str>,
    ) -> Result<String, EchecResolution> {
        let url = Self::lookup_url(&self.lookup_base, apple_id, country);
        debug!(url = %url, "podcast_lookup");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| EchecResolution::Reseau(e.to_string()))?;
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EchecResolution::Reseau(e.to_string()))?;
        Self::flux_du_lookup(&data)
    }

    /// Résout le flux RSS d'un podcast du palmarès Apple (#6770).
    ///
    /// La recherche part dans la boutique du PAYS du palmarès. Si cette
    /// boutique ne connaît pas l'identifiant, on retente une fois sans pays :
    /// un podcast du palmarès d'un pays peut n'être référencé qu'ailleurs.
    /// Un podcast connu mais sans flux public ne se retente pas — la réponse
    /// est déjà la bonne. Tout échec est journalisé : il ne disparaît plus en
    /// silence.
    pub async fn resolve_feed_url(
        &self,
        apple_id: &str,
        country: Option<&str>,
    ) -> Result<String, EchecResolution> {
        let retenu = pays_des_palmares()
            .lock()
            .ok()
            .and_then(|m| m.get(apple_id).cloned());
        let pays = country
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .or(retenu.as_deref());
        let mut issue = self.lookup(apple_id, pays).await;
        if pays.is_some() && issue == Err(EchecResolution::Introuvable) {
            issue = self.lookup(apple_id, None).await;
        }
        if let Err(e) = &issue {
            warn!(apple_id, country = pays.unwrap_or(""), echec = ?e, "podcast_flux_non_resolu");
        }
        issue
    }

    // ── Apple Top Podcasts ──────────────────────────────────────────

    /// L'adresse du palmarès Apple pour un pays et un genre.
    ///
    /// Le pays est un segment du CHEMIN chez Apple
    /// (`…/fr/podcasts/top/50/…`), pas un paramètre de requête : c'est le
    /// fournisseur lui-même qui filtre, le serveur ne trie rien. Extraite
    /// pour que le passage du pays soit vérifiable sans réseau (#3395).
    pub fn top_chart_url(genre: Option<u32>, country: &str) -> String {
        Self::top_chart_url_sur(APPLE_TOP_BASE, genre, country)
    }

    fn top_chart_url_sur(base: &str, genre: Option<u32>, country: &str) -> String {
        let cc = country.to_lowercase();
        match genre {
            Some(gid) => format!("{base}/{cc}/podcasts/top/50/podcasts.json?genre={gid}"),
            None => format!("{base}/{cc}/podcasts/top/50/podcasts.json"),
        }
    }

    /// #6770 — `identifiant Apple → flux` d'une réponse `lookup` groupée ;
    /// un résultat sans `feedUrl` (aucun flux public) n'y figure pas.
    fn flux_du_lookup_groupe(
        data: &serde_json::Value,
    ) -> std::collections::HashMap<String, String> {
        data["results"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter_map(|x| {
                        let id = x["collectionId"]
                            .as_u64()
                            .or_else(|| x["trackId"].as_u64())?
                            .to_string();
                        let flux = x["feedUrl"].as_str().filter(|u| !u.is_empty())?;
                        Some((id, flux.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// #6770 — UNE recherche `lookup` pour plusieurs identifiants (Apple les
    /// accepte séparés par des virgules), dans la boutique d'un pays.
    async fn lookup_groupe(
        &self,
        ids: &[&str],
        country: Option<&str>,
    ) -> std::collections::HashMap<String, String> {
        let url = Self::lookup_url(&self.lookup_base, &ids.join(","), country);
        let data = match self.client.get(&url).send().await {
            Ok(resp) => resp.json::<serde_json::Value>().await.ok(),
            Err(e) => {
                warn!(error = %e, "podcast_lookup_groupe_injoignable");
                None
            }
        };
        data.map(|d| Self::flux_du_lookup_groupe(&d))
            .unwrap_or_default()
    }

    /// #6770 — le palmarès Apple ne porte que l'identifiant : sans adresse de
    /// flux, aucun client ne pouvait ouvrir ni suivre un podcast des onglets
    /// « Populaires » et « Tous » (le web s'arrête sur un `feed_url` vide).
    /// Les flux se résolvent ici, en UNE recherche groupée dans la boutique
    /// du pays du palmarès, puis une seconde sans pays pour ceux qu'elle ne
    /// connaît pas. Un podcast sans flux public garde un `feed_url` vide ;
    /// son abonnement le dit (`EchecResolution::SansFluxPublic`).
    async fn completer_les_flux(&self, podcasts: &mut [Podcast], cc: &str) {
        let ids_manquants = |podcasts: &[Podcast]| -> Vec<String> {
            podcasts
                .iter()
                .filter(|p| p.feed_url.is_empty())
                .filter_map(|p| p.source_id.strip_prefix("apple-").map(String::from))
                .collect()
        };
        if let Ok(mut m) = pays_des_palmares().lock() {
            if m.len() > PAYS_DES_PALMARES_MAX {
                m.clear();
            }
            for id in ids_manquants(podcasts) {
                m.insert(id, cc.to_string());
            }
        }
        for pays in [Some(cc), None] {
            let ids = ids_manquants(podcasts);
            if ids.is_empty() {
                break;
            }
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            let flux = self.lookup_groupe(&refs, pays).await;
            for p in podcasts.iter_mut().filter(|p| p.feed_url.is_empty()) {
                if let Some(f) = p
                    .source_id
                    .strip_prefix("apple-")
                    .and_then(|id| flux.get(id))
                {
                    p.feed_url = f.clone();
                }
            }
        }
        let restants = ids_manquants(podcasts).len();
        if restants > 0 {
            debug!(
                country = cc,
                restants, "podcasts_du_palmares_sans_flux_public"
            );
        }
    }

    /// Fetch the top 50 podcasts from the Apple RSS feed generator.
    /// `country` is an ISO 3166-1 alpha-2 code (e.g. "fr", "us", "de", "kr").
    /// Results are cached for 1 hour per (country, genre) key.
    pub async fn top_podcasts(
        &self,
        genre: Option<u32>,
        country: &str,
    ) -> Result<Vec<Podcast>, String> {
        let cc = country.to_lowercase();
        let url = Self::top_chart_url_sur(&self.top_base, genre, &cc);
        let cache_key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&cc, &mut h);
            std::hash::Hash::hash(&genre.unwrap_or(0), &mut h);
            std::hash::Hasher::finish(&h) as u32
        };

        // Per-genre caches stored in a static map.
        type CacheEntry = (Instant, Vec<Podcast>);
        type CacheMap = std::collections::HashMap<u32, CacheEntry>;
        static CACHE: OnceLock<Mutex<CacheMap>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));

        {
            let guard = cache.lock().await;
            if let Some((ts, data)) = guard.get(&cache_key)
                && ts.elapsed() < TOP_CACHE_TTL
                && !data.is_empty()
            {
                return Ok(data.clone());
            }
        }

        debug!(url = %url, "apple_top_podcasts_fetch");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("apple top podcasts: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("apple top podcasts HTTP {}", resp.status()));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("apple top podcasts parse: {e}"))?;

        let results = body["feed"]["results"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let podcasts: Vec<Podcast> = results
            .iter()
            .filter_map(|r| {
                let apple_id = r["id"].as_str()?.to_string();
                let name = r["name"].as_str()?.to_string();
                let artist = r["artistName"].as_str().unwrap_or("").to_string();
                let artwork = r["artworkUrl100"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
                    // Upgrade to 600px artwork.
                    .replace("100x100bb", "600x600bb");
                let category = r["genres"]
                    .as_array()
                    .and_then(|g| g.first())
                    .and_then(|g| g["name"].as_str())
                    .map(String::from);
                Some(Podcast {
                    name,
                    artist,
                    feed_url: String::new(), // Apple top chart doesn't include feed URLs
                    cover_url: artwork,
                    description: String::new(),
                    episode_count: 0,
                    source_id: format!("apple-{apple_id}"),
                    category,
                })
            })
            .collect();

        debug!(count = podcasts.len(), "apple_top_podcasts_parsed");
        let mut podcasts = podcasts;
        self.completer_les_flux(&mut podcasts, &cc).await;

        // Update cache.
        {
            let mut guard = cache.lock().await;
            guard.insert(cache_key, (Instant::now(), podcasts.clone()));
        }

        Ok(podcasts)
    }

    /// Return the list of available genre filters with their IDs and labels.
    pub fn available_genres() -> Vec<serde_json::Value> {
        PodcastGenre::all()
            .iter()
            .map(|g| {
                serde_json::json!({
                    "id": *g as u32,
                    "name": g.label(),
                })
            })
            .collect()
    }
}

impl Default for PodcastService {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_rss(xml: &str, limit: usize) -> Result<Vec<PodcastEpisode>, String> {
    let mut episodes = Vec::new();
    let channel_cover = extract_channel_image(xml);
    let mut search_from = 0;
    while episodes.len() < limit {
        let item_start = match xml[search_from..].find("<item") {
            Some(pos) => search_from + pos,
            None => break,
        };
        let item_end = match xml[item_start..].find("</item>") {
            Some(pos) => item_start + pos + 7,
            None => break,
        };
        let item = &xml[item_start..item_end];
        let title = extract_tag(item, "title").unwrap_or_default();
        let audio_url = extract_attr(item, "enclosure", "url").unwrap_or_default();
        if audio_url.is_empty() {
            search_from = item_end;
            continue;
        }
        let duration_text = extract_tag(item, "itunes:duration").unwrap_or_default();
        let duration_ms = parse_duration(&duration_text);
        let description = extract_tag(item, "itunes:summary")
            .or_else(|| extract_tag(item, "description"))
            .map(|d| strip_html_tags(&d))
            .unwrap_or_default();
        let published = extract_tag(item, "pubDate").unwrap_or_default();
        let cover_url =
            extract_attr(item, "itunes:image", "href").unwrap_or_else(|| channel_cover.clone());
        episodes.push(PodcastEpisode {
            title,
            description,
            audio_url,
            duration_ms,
            published,
            cover_url,
        });
        search_from = item_end;
    }
    Ok(episodes)
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?;
    let after_open = &xml[start + open.len()..];
    let content_start = after_open.find('>')? + 1;
    let content = &after_open[content_start..];
    let end = content.find(&close)?;
    let text = content[..end].trim();
    let text = text
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(text);
    Some(decode_entities(text))
}

/// Decode XML/HTML entities (e.g. `&apos;` → `'`, `&amp;` → `&`, `&#233;` → `é`).
/// Radio France and other feeds encode apostrophes/accents outside of CDATA,
/// so the raw text would otherwise display `L&apos;intégrale`.
fn decode_entities(text: &str) -> String {
    match unescape(text) {
        Ok(decoded) => decoded.into_owned(),
        Err(_) => text.to_string(),
    }
}

fn extract_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag} ");
    let start = xml.find(&open)?;
    let tag_text = &xml[start..];
    let end = tag_text.find('>')?;
    let tag_str = &tag_text[..end];
    let attr_key = format!("{attr}=\"");
    let attr_start = tag_str.find(&attr_key)?;
    let val_start = attr_start + attr_key.len();
    let val_end = tag_str[val_start..].find('"')?;
    Some(tag_str[val_start..val_start + val_end].to_string())
}

fn extract_channel_image(xml: &str) -> String {
    if let Some(href) = extract_attr(xml, "itunes:image", "href") {
        return href;
    }
    if let Some(img) = extract_tag(xml, "image")
        && let Some(url) = extract_tag(&img, "url")
    {
        return url;
    }
    String::new()
}

fn parse_duration(text: &str) -> u64 {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    if let Ok(secs) = text.parse::<u64>() {
        return secs * 1000;
    }
    let parts: Vec<&str> = text.split(':').collect();
    match parts.len() {
        3 => {
            let h: u64 = parts[0].parse().unwrap_or(0);
            let m: u64 = parts[1].parse().unwrap_or(0);
            let s: u64 = parts[2].parse().unwrap_or(0);
            (h * 3600 + m * 60 + s) * 1000
        }
        2 => {
            let m: u64 = parts[0].parse().unwrap_or(0);
            let s: u64 = parts[1].parse().unwrap_or(0);
            (m * 60 + s) * 1000
        }
        _ => 0,
    }
}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_duration_hms() {
        assert_eq!(parse_duration("1:30:00"), 5400000);
        assert_eq!(parse_duration("45:30"), 2730000);
        assert_eq!(parse_duration("3600"), 3600000);
        assert_eq!(parse_duration(""), 0);
    }
    #[test]
    fn strip_html() {
        assert_eq!(strip_html_tags("<p>Hello <b>world</b></p>"), "Hello world");
    }
    #[test]
    fn extract_tag_basic() {
        let xml = r#"<item><title>Episode 1</title></item>"#;
        assert_eq!(extract_tag(xml, "title"), Some("Episode 1".into()));
    }
    #[test]
    fn extract_tag_cdata() {
        let xml = r#"<title><![CDATA[My Title]]></title>"#;
        assert_eq!(extract_tag(xml, "title"), Some("My Title".into()));
    }
    #[test]
    fn extract_tag_decodes_entities() {
        // Radio France encodes apostrophes/accents as entities outside CDATA.
        let xml = r#"<title>L&apos;int&#233;grale de L&apos;After Foot</title>"#;
        assert_eq!(
            extract_tag(xml, "title"),
            Some("L'intégrale de L'After Foot".into())
        );
        let amp = r#"<title>Rock &amp; Roll</title>"#;
        assert_eq!(extract_tag(amp, "title"), Some("Rock & Roll".into()));
    }
    #[test]
    fn extract_attr_basic() {
        let xml = r#"<enclosure url="http://audio.mp3" type="audio/mpeg"/>"#;
        assert_eq!(
            extract_attr(xml, "enclosure", "url"),
            Some("http://audio.mp3".into())
        );
    }
    #[test]
    fn parse_rss_basic() {
        let rss = r#"<rss><channel><itunes:image href="http://cover.jpg"/><item><title>Ep 1</title><enclosure url="http://audio1.mp3" type="audio/mpeg"/><itunes:duration>30:00</itunes:duration><pubDate>Mon, 01 Jan 2024</pubDate></item><item><title>Ep 2</title><enclosure url="http://audio2.mp3" type="audio/mpeg"/><itunes:duration>1500</itunes:duration></item></channel></rss>"#;
        let episodes = parse_rss(rss, 10).unwrap();
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].title, "Ep 1");
        assert_eq!(episodes[0].duration_ms, 1800000);
    }
    #[test]
    fn curated_french_podcasts() {
        let podcasts = PodcastService::curated_french_podcasts();
        assert!(podcasts.len() >= 10);
        // Check cover URLs are populated.
        for p in &podcasts {
            assert!(!p.feed_url.is_empty(), "empty feed_url for {}", p.name);
            assert!(!p.cover_url.is_empty(), "empty cover_url for {}", p.name);
        }
        // Check some expected shows.
        assert!(podcasts.iter().any(|p| p.name.contains("sept neuf")));
        assert!(podcasts.iter().any(|p| p.name == "Boomerang"));
        assert!(podcasts.iter().any(|p| p.name == "Club Jazzafip"));
    }
    /// #3395 — le pays atteint VRAIMENT le fournisseur.
    ///
    /// Chez Apple, le pays est un segment du chemin : deux pays donnent deux
    /// adresses. Le témoin épingle cela sans réseau — c'est ce que
    /// `/discover` ne faisait pas, en demandant « us » en dur.
    #[test]
    fn top_chart_url_porte_le_pays_3395() {
        let fr = PodcastService::top_chart_url(None, "fr");
        let us = PodcastService::top_chart_url(None, "us");
        assert_ne!(fr, us, "deux pays, deux adresses");
        assert!(fr.contains("/fr/podcasts/top/50/"), "{fr}");
        assert!(us.contains("/us/podcasts/top/50/"), "{us}");
        // Le pays est normalisé, le genre reste un paramètre de requête.
        assert_eq!(PodcastService::top_chart_url(None, "FR"), fr);
        assert!(
            PodcastService::top_chart_url(Some(1310), "fr").ends_with("podcasts.json?genre=1310"),
            "le genre reste un paramètre de requête"
        );
    }

    /// #3395 — la sélection éditoriale est française, et le dit.
    ///
    /// Elle ne se « filtre » pas : hors de France il n'y a rien à rendre, et
    /// rendre la liste française sous un autre drapeau est le défaut signalé.
    #[test]
    fn la_selection_editoriale_ne_couvre_que_la_france_3395() {
        assert_eq!(PodcastService::CURATED_COUNTRY, "fr");
        assert_eq!(
            PodcastService::curated_for_country("fr").len(),
            PodcastService::curated_french_podcasts().len(),
            "en France, la sélection complète"
        );
        assert_eq!(
            PodcastService::curated_for_country("FR").len(),
            PodcastService::curated_french_podcasts().len(),
            "la casse du code pays ne change rien"
        );
        for ailleurs in ["us", "de", "kr", ""] {
            assert!(
                PodcastService::curated_for_country(ailleurs).is_empty(),
                "aucune sélection pour {ailleurs:?} — il n'en existe pas"
            );
        }
    }

    #[test]
    fn radio_france_alias() {
        let a = PodcastService::radio_france_podcasts();
        let b = PodcastService::curated_french_podcasts();
        assert_eq!(a.len(), b.len());
    }
    #[test]
    fn genre_enum() {
        assert_eq!(PodcastGenre::from_id(1310).unwrap().label(), "Music");
        assert!(PodcastGenre::from_id(9999).is_none());
        assert!(PodcastGenre::all().len() >= 18);
    }
    #[test]
    fn available_genres_list() {
        let genres = PodcastService::available_genres();
        assert!(genres.len() >= 18);
        assert!(genres.iter().any(|g| g["name"] == "Music"));
    }

    // ── #6770 : le pays du palmarès part avec la recherche du flux ──────

    /// Faux Apple local : rend `reponse(requete)` à chaque requête et garde
    /// les lignes de requête reçues.
    async fn faux_apple(
        reponse: fn(&str) -> String,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/lookup", ecoute.local_addr().unwrap());
        let recues = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let journal = recues.clone();
        tokio::spawn(async move {
            while let Ok((mut flux, _)) = ecoute.accept().await {
                let mut tampon = vec![0u8; 4096];
                let n = flux.read(&mut tampon).await.unwrap_or(0);
                let texte = String::from_utf8_lossy(&tampon[..n]).to_string();
                let ligne = texte.lines().next().unwrap_or("").to_string();
                journal.lock().unwrap().push(ligne.clone());
                let corps = reponse(&ligne);
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = flux.write_all(http.as_bytes()).await;
            }
        });
        (base, recues)
    }

    /// Le comportement d'Apple relevé le 22/10 pour un podcast classé
    /// seulement en France : rien dans la boutique par défaut, le flux dans
    /// la boutique française.
    fn classe_seulement_en_france(ligne: &str) -> String {
        if ligne.contains("country=fr") {
            r#"{"resultCount":1,"results":[{"feedUrl":"https://flux.exemple.test/fr.xml"}]}"#.into()
        } else {
            r#"{"resultCount":0,"results":[]}"#.into()
        }
    }

    #[test]
    fn lookup_url_porte_le_pays_6770() {
        let url = PodcastService::lookup_url(ITUNES_LOOKUP_URL, "1509742508", Some("FR"));
        assert!(
            url.contains("country=fr"),
            "la recherche Apple doit partir avec le pays du palmarès : {url}"
        );
        let sans = PodcastService::lookup_url(ITUNES_LOOKUP_URL, "1509742508", None);
        assert!(!sans.contains("country="), "{sans}");
    }

    #[tokio::test]
    async fn resolve_feed_url_transmet_le_pays_6770() {
        let (base, recues) = faux_apple(classe_seulement_en_france).await;
        let svc = PodcastService::with_lookup_base(crate::http::client::shared().clone(), &base);

        let flux = svc.resolve_feed_url("1509742508", Some("fr")).await;

        assert_eq!(
            flux,
            Ok("https://flux.exemple.test/fr.xml".to_string()),
            "un podcast classé seulement en France doit se résoudre dans la boutique française ; requêtes reçues : {:?}",
            recues.lock().unwrap()
        );
        assert!(
            recues.lock().unwrap()[0].contains("country=fr"),
            "la PREMIÈRE recherche doit porter le pays : {:?}",
            recues.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn sans_flux_public_est_dit_et_pas_retente_6770() {
        fn connu_sans_flux(_: &str) -> String {
            r#"{"resultCount":1,"results":[{"collectionName":"Affaires sensibles"}]}"#.into()
        }
        let (base, recues) = faux_apple(connu_sans_flux).await;
        let svc = PodcastService::with_lookup_base(crate::http::client::shared().clone(), &base);

        let flux = svc.resolve_feed_url("912451024", Some("fr")).await;

        assert_eq!(flux, Err(EchecResolution::SansFluxPublic));
        assert_eq!(recues.lock().unwrap().len(), 1, "inutile de retenter");
    }

    #[tokio::test]
    async fn inconnu_du_pays_se_retente_sans_pays_6770() {
        fn seulement_par_defaut(ligne: &str) -> String {
            if ligne.contains("country=") {
                r#"{"resultCount":0,"results":[]}"#.into()
            } else {
                r#"{"resultCount":1,"results":[{"feedUrl":"https://flux.exemple.test/us.xml"}]}"#
                    .into()
            }
        }
        let (base, _) = faux_apple(seulement_par_defaut).await;
        let svc = PodcastService::with_lookup_base(crate::http::client::shared().clone(), &base);
        assert_eq!(
            svc.resolve_feed_url("1", Some("fr")).await,
            Ok("https://flux.exemple.test/us.xml".to_string())
        );

        fn rien(_: &str) -> String {
            r#"{"resultCount":0,"results":[]}"#.into()
        }
        let (base, _) = faux_apple(rien).await;
        let svc = PodcastService::with_lookup_base(crate::http::client::shared().clone(), &base);
        assert_eq!(
            svc.resolve_feed_url("2", Some("fr")).await,
            Err(EchecResolution::Introuvable)
        );
    }

    /// #6770 — la cause du signalement : le palmarès Apple ne porte pas
    /// d'adresse de flux, et le web s'arrête sur un `feed_url` vide. Le
    /// palmarès servi par Tune porte désormais le flux, résolu dans la
    /// boutique du PAYS du palmarès (une requête groupée), puis sans pays
    /// pour le reste ; un podcast sans flux public garde un flux vide.
    #[tokio::test]
    async fn le_palmares_porte_l_adresse_du_flux_6770() {
        fn apple(ligne: &str) -> String {
            if ligne.contains("/podcasts/top/") {
                return r#"{"feed":{"results":[
                    {"id":"111","name":"Classé en France","artistName":"A"},
                    {"id":"222","name":"Boutique par défaut","artistName":"B"},
                    {"id":"333","name":"Exclusivité Apple","artistName":"C"}]}}"#
                    .into();
            }
            if ligne.contains("id=333&") {
                // Recherche d'un seul identifiant : la boutique du pays ne le
                // connaît pas, la boutique par défaut le connaît sans flux.
                return if ligne.contains("country=") {
                    r#"{"resultCount":0,"results":[]}"#.into()
                } else {
                    r#"{"resultCount":1,"results":[{"collectionId":333}]}"#.into()
                };
            }
            if ligne.contains("country=zz") {
                r#"{"resultCount":1,"results":[
                    {"collectionId":111,"feedUrl":"https://flux.exemple.test/111.xml"}]}"#
                    .into()
            } else {
                r#"{"resultCount":2,"results":[
                    {"collectionId":222,"feedUrl":"https://flux.exemple.test/222.xml"},
                    {"collectionId":333,"collectionName":"Exclusivité Apple"}]}"#
                    .into()
            }
        }
        let (lookup, recues) = faux_apple(apple).await;
        let top = lookup.trim_end_matches("/lookup").to_string();
        let svc =
            PodcastService::with_apple_bases(crate::http::client::shared().clone(), &top, &lookup);

        // « zz » : un pays qu'aucun autre banc ne met dans le cache du palmarès.
        let palmares = svc.top_podcasts(None, "zz").await.expect("palmarès");

        let flux: Vec<&str> = palmares.iter().map(|p| p.feed_url.as_str()).collect();
        assert_eq!(
            flux,
            [
                "https://flux.exemple.test/111.xml",
                "https://flux.exemple.test/222.xml",
                ""
            ],
            "le palmarès doit porter le flux de chaque podcast qui en a un ; requêtes : {:?}",
            recues.lock().unwrap()
        );
        let lignes = recues.lock().unwrap().clone();
        assert_eq!(
            lignes.len(),
            3,
            "palmarès + deux recherches groupées : {lignes:?}"
        );
        assert!(
            lignes[1].contains("id=111,222,333") && lignes[1].contains("country=zz"),
            "UNE recherche pour tout le palmarès, dans la boutique du pays : {}",
            lignes[1]
        );
        assert!(
            lignes[2].contains("id=222,333") && !lignes[2].contains("country="),
            "la seconde, sans pays, ne porte que les restants : {}",
            lignes[2]
        );

        // Un client qui n'envoie pas de pays (aucun n'en envoie) : la
        // résolution part dans la boutique du palmarès d'où vient l'id.
        assert_eq!(
            svc.resolve_feed_url("333", None).await,
            Err(EchecResolution::SansFluxPublic),
            "connu sans flux public : dit comme tel"
        );
        let premiere = recues.lock().unwrap().get(3).cloned().unwrap_or_default();
        assert!(
            premiere.contains("id=333&") && premiere.contains("country=zz"),
            "sans pays du client, le pays retenu du palmarès doit partir : {premiere}"
        );
    }
}
