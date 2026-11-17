//! #8935 — TIDAL « Pour vous » et titres recommandés sous une playlist.
//!
//! Deux lectures de l'API v1 que Tune emploie déjà (`api.tidal.com/v1`) :
//!
//! - la page `pages/for_you` (la page « For You » de TIDAL, celle que
//!   python-tidal lit par `Session.for_you()`), ses rangées de modules
//!   `ALBUM_LIST`, `PLAYLIST_LIST`, `MIX_LIST`, `ARTIST_LIST`, `TRACK_LIST` et
//!   `MIXED_TYPES_LIST`. Une page vide ou refusée retombe sur `pages/home`
//!   (l'accueil v1, personnalisé lui aussi), et la réponse dit laquelle a
//!   servi. Les modules qu'on ne sait ni lire ni ouvrir (vidéos, promotions,
//!   liens de pages, articles) sont écartés, jamais une erreur.
//! - `playlists/{uuid}/recommendations/items?limit=&offset=` : les titres que
//!   TIDAL propose sous une playlist, enveloppés `{type: "track", item}`
//!   comme les éléments d'une playlist, une centaine au plus.
//!
//! Un module « paresseux » arrive sans éléments, sa liste à
//! `pagedList.dataApiPath` : elle est relue, bornée à
//! [`MODULES_PARESSEUX_MAX`] lectures, et seulement pour un chemin `pages/…`
//! sans rien d'autre qu'un chemin d'API.

use super::*;

/// Durée de vie de la page « Pour vous » : dix minutes. TIDAL la renouvelle
/// au mieux quelques fois par jour ; l'écran se rouvre souvent.
pub(super) const TTL_POUR_VOUS: Duration = Duration::from_secs(600);

/// Lectures de modules paresseux au plus, par page.
pub(super) const MODULES_PARESSEUX_MAX: usize = 6;

/// Titres recommandés par page, au plus : la borne de l'API v1.
pub(super) const RECOMMANDES_PAR_PAGE_MAX: usize = 100;

/// La page « Pour vous » en cache, pour un compte.
pub(super) struct PourVousCache {
    pub(super) compte: Option<u64>,
    pub(super) page: PagePersonnalisee,
    pub(super) fetched_at: Instant,
}

impl PourVousCache {
    pub(super) fn frais_pour(&self, compte: Option<u64>) -> bool {
        self.compte == compte && self.fetched_at.elapsed() < TTL_POUR_VOUS
    }
}

/// Un module à relire : rang dans la liste des rangées, chemin de ses
/// éléments, type du module.
type ModuleParesseux = (usize, String, String);

impl TidalService {
    /// Un chemin `dataApiPath` de TIDAL, ramené à un chemin d'API relatif
    /// (`/pages/…`). `None` pour tout autre chose : une adresse absolue, une
    /// remontée, un caractère hors d'un chemin d'API.
    pub(super) fn chemin_de_module(brut: &str) -> Option<String> {
        let chemin = brut.trim_start_matches('/');
        let chemin = chemin.strip_prefix("v1/").unwrap_or(chemin);
        let sain = chemin.starts_with("pages/")
            && !chemin.contains("..")
            && chemin
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/_-?=&.".contains(c));
        sain.then(|| format!("/{chemin}"))
    }

    /// Range un élément d'une rangée dans la bonne liste. `nature` : le type
    /// de l'élément (`ALBUM`, `PLAYLIST`, `MIX`, `ARTIST`, `TRACK`), déduit du
    /// module pour une liste homogène, lu sur l'enveloppe pour une liste
    /// mêlée. Ce qui ne s'affiche ni ne se lit (sans identifiant, vidéo) est
    /// écarté.
    fn ranger(rangee: &mut RangeePersonnalisee, nature: &str, item: &serde_json::Value) {
        match nature.to_ascii_uppercase().as_str() {
            "ALBUM" if item["id"].as_u64().is_some() => rangee.albums.push(Self::map_album(item)),
            "PLAYLIST" => {
                let p = Self::map_playlist(item);
                if !p.id.is_empty() && !rangee.playlists.iter().any(|d| d.id == p.id) {
                    rangee.playlists.push(p);
                }
            }
            "MIX" => {
                if let Some(m) = Self::map_mix(item)
                    && !rangee.playlists.iter().any(|d| d.id == m.id)
                {
                    rangee.playlists.push(m);
                }
            }
            "ARTIST" if item["id"].as_u64().is_some() => {
                rangee.artists.push(Self::map_artist(item))
            }
            "TRACK" if item["id"].as_u64().is_some() => rangee.tracks.push(Self::map_track(item)),
            _ => {}
        }
    }

    /// La nature des éléments d'un module homogène ; `None` pour un module
    /// mêlé (`MIXED_TYPES_LIST`) ou qu'on ne lit pas.
    fn nature_du_module(type_module: &str) -> Option<&'static str> {
        match type_module {
            "ALBUM_LIST" => Some("ALBUM"),
            "PLAYLIST_LIST" => Some("PLAYLIST"),
            "MIX_LIST" => Some("MIX"),
            "ARTIST_LIST" => Some("ARTIST"),
            "TRACK_LIST" => Some("TRACK"),
            _ => None,
        }
    }

    /// Remplit `rangee` avec les éléments d'un module de type `type_module`.
    pub(super) fn remplir_la_rangee(
        rangee: &mut RangeePersonnalisee,
        type_module: &str,
        items: &[serde_json::Value],
    ) {
        for item in items {
            match Self::nature_du_module(type_module) {
                Some(nature) => Self::ranger(rangee, nature, item),
                None if type_module == "MIXED_TYPES_LIST" && item["item"].is_object() => {
                    Self::ranger(rangee, item["type"].as_str().unwrap_or(""), &item["item"])
                }
                None => {}
            }
        }
    }

    /// Les rangées d'une page v1 (`rows[].modules[]`), et les modules dont
    /// les éléments restent à lire. Une page sans `rows` (refus de TIDAL en
    /// JSON) ne rend rien.
    pub(super) fn rangees_de_la_page(
        data: &serde_json::Value,
    ) -> (Vec<RangeePersonnalisee>, Vec<ModuleParesseux>) {
        let mut rangees = Vec::new();
        let mut paresseux = Vec::new();
        for (i, row) in data["rows"].as_array().into_iter().flatten().enumerate() {
            for (j, module) in row["modules"].as_array().into_iter().flatten().enumerate() {
                let type_module = module["type"].as_str().unwrap_or("");
                if Self::nature_du_module(type_module).is_none()
                    && type_module != "MIXED_TYPES_LIST"
                {
                    continue;
                }
                let titre = module["title"].as_str().map(str::trim).unwrap_or("");
                if titre.is_empty() {
                    continue;
                }
                let mut rangee = RangeePersonnalisee {
                    id: module["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| format!("for-you-{i}-{j}")),
                    title: titre.into(),
                    subtitle: module["description"]
                        .as_str()
                        .map(str::trim)
                        .filter(|d| !d.is_empty())
                        .map(String::from),
                    ..Default::default()
                };
                let items = module["pagedList"]["items"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                Self::remplir_la_rangee(&mut rangee, type_module, items);
                if items.is_empty()
                    && let Some(chemin) = module["pagedList"]["dataApiPath"]
                        .as_str()
                        .and_then(Self::chemin_de_module)
                {
                    paresseux.push((rangees.len(), chemin, type_module.to_string()));
                    rangees.push(rangee);
                    continue;
                }
                if !rangee.est_vide() {
                    rangees.push(rangee);
                }
            }
        }
        (rangees, paresseux)
    }

    /// Les éléments d'un module paresseux : une liste paginée (`items`) ou
    /// une page dont le premier module porte la liste.
    pub(super) fn elements_d_un_module(data: &serde_json::Value) -> Vec<serde_json::Value> {
        if let Some(items) = data["items"].as_array() {
            return items.clone();
        }
        data["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|row| row["modules"].as_array().into_iter().flatten())
            .find_map(|m| m["pagedList"]["items"].as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// Lit une page v1 (`/pages/for_you`, `/pages/home`) et ses modules
    /// paresseux, et rend ses rangées non vides.
    async fn lire_une_page_personnalisee(
        &self,
        chemin: &str,
    ) -> Result<Vec<RangeePersonnalisee>, String> {
        let data = self.api_get(chemin).await?;
        let (mut rangees, paresseux) = Self::rangees_de_la_page(&data);
        for (rang, chemin_module, type_module) in paresseux.into_iter().take(MODULES_PARESSEUX_MAX)
        {
            let separateur = if chemin_module.contains('?') {
                '&'
            } else {
                '?'
            };
            match self
                .api_get(&format!("{chemin_module}{separateur}deviceType=BROWSER"))
                .await
            {
                Ok(module) => {
                    let items = Self::elements_d_un_module(&module);
                    if let Some(rangee) = rangees.get_mut(rang) {
                        Self::remplir_la_rangee(rangee, &type_module, &items);
                    }
                }
                // Le chemin n'est pas journalisé : seul le motif l'est.
                Err(e) => debug!(error = %e, "tidal_pour_vous_module_illisible"),
            }
        }
        rangees.retain(|r| !r.est_vide());
        Ok(rangees)
    }

    /// La page « Pour vous » du compte : `pages/for_you`, et à défaut
    /// (refus, page vide) `pages/home`.
    pub(super) async fn lire_la_page_pour_vous(&self) -> Result<PagePersonnalisee, TuneError> {
        match self
            .lire_une_page_personnalisee("/pages/for_you?deviceType=BROWSER")
            .await
        {
            Ok(rows) if !rows.is_empty() => {
                return Ok(PagePersonnalisee {
                    source: "for_you".into(),
                    rows,
                });
            }
            Ok(_) => debug!("tidal_pour_vous_page_vide_repli_accueil"),
            Err(e) => warn!(error = %e, "tidal_pour_vous_page_illisible_repli_accueil"),
        }
        let rows = self
            .lire_une_page_personnalisee("/pages/home?deviceType=BROWSER")
            .await?;
        Ok(PagePersonnalisee {
            source: "home".into(),
            rows,
        })
    }

    pub(super) fn pour_vous_depuis_le_cache(&self) -> Option<PagePersonnalisee> {
        let cache = self.pour_vous_cache.lock().ok()?;
        let entree = cache.as_ref()?;
        entree.frais_pour(self.user_id).then(|| entree.page.clone())
    }

    pub(super) fn vider_le_cache_pour_vous(&self) {
        if let Ok(mut cache) = self.pour_vous_cache.lock() {
            *cache = None;
        }
    }

    /// Un identifiant de playlist TIDAL (uuid) : chiffres, lettres, tirets.
    pub(super) fn identifiant_de_playlist_sain(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }

    /// Les titres recommandés d'une réponse `recommendations/items`.
    pub(super) fn titres_recommandes(data: &serde_json::Value, offset: usize) -> TitresRecommandes {
        let tracks: Vec<StreamTrack> = data["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|el| {
                let (nature, piste) = if el["item"].is_object() {
                    (el["type"].as_str().unwrap_or("track"), &el["item"])
                } else {
                    ("track", el)
                };
                (nature.eq_ignore_ascii_case("track") && piste["id"].as_u64().is_some())
                    .then(|| Self::map_track(piste))
            })
            .collect();
        let total = data["totalNumberOfItems"]
            .as_u64()
            .map(|t| t as usize)
            .unwrap_or(offset + tracks.len());
        TitresRecommandes {
            tracks,
            total,
            offset,
        }
    }

    pub(super) async fn lire_les_recommandations(
        &self,
        playlist_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<TitresRecommandes, TuneError> {
        // Un mix (#7822) n'est pas une playlist chez TIDAL : rien à proposer.
        if playlist_id.starts_with(PREFIXE_MIX) {
            return Ok(TitresRecommandes {
                offset,
                ..Default::default()
            });
        }
        if !Self::identifiant_de_playlist_sain(playlist_id) {
            return Err(TuneError::NotFound("tidal playlist not found".into()));
        }
        let limite = limit.clamp(1, RECOMMANDES_PAR_PAGE_MAX);
        let data = self
            .api_get(&format!(
                "/playlists/{playlist_id}/recommendations/items?limit={limite}&offset={offset}"
            ))
            .await?;
        match data["status"].as_u64() {
            Some(404) => return Err(TuneError::NotFound("tidal playlist not found".into())),
            Some(429) => return Err(TuneError::from("tidal rate limited".to_string())),
            Some(status) if status >= 400 => {
                return Err(TuneError::from(format!(
                    "tidal recommendations {status}: {}",
                    data["userMessage"].as_str().unwrap_or("")
                )));
            }
            _ => {}
        }
        Ok(Self::titres_recommandes(&data, offset))
    }
}
