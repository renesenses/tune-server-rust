use serde::{Deserialize, Serialize};

use crate::TuneError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTrack {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub title: String,
    #[serde(rename(serialize = "artist_name"), alias = "artist_name")]
    pub artist: String,
    #[serde(rename(serialize = "album_title"), alias = "album_title")]
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub duration_ms: u64,
    pub cover_path: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub explicit: bool,
    pub quality: Option<StreamQuality>,
    /// International Standard Recording Code, when the service exposes it. Enables
    /// exact cross-service matching (see `streaming::matching::best_stream_match`).
    /// `#[serde(default)]` so older serialized results without the field still load.
    #[serde(default)]
    pub isrc: Option<String>,
    /// Compositeur de la piste, quand le service l'expose (Qobuz : `composer.name`).
    /// Champ propre : il ne sert JAMAIS de valeur d'« artist » — en classique,
    /// compositeur ≠ interprète (#1407). `#[serde(default)]` comme `isrc`, pour
    /// que les résultats sérialisés antérieurs se chargent encore.
    #[serde(default)]
    pub composer: Option<String>,
    /// Identifiant, SUR LE SERVICE, de l'artiste nommé par `artist` — jamais
    /// d'un autre.
    ///
    /// `album_id` existait déjà ; son pendant artiste, non. Une piste Qobuz en
    /// « Lecture en cours » n'offrait donc aucun moyen d'ouvrir la fiche de
    /// l'artiste autrement qu'en RECHERCHANT son nom, c'est-à-dire en devinant
    /// — le geste que #1284 avait déjà condamné pour l'album (« Entreat »
    /// ouvrait la page de The Cure). Cyrille Moutia le redemande depuis le
    /// 30/06 (#1361) : titre d'album et nom d'artiste cliquables.
    ///
    /// Prendre l'artiste de l'ALBUM à la place n'est pas un repli acceptable :
    /// en classique il désigne couramment le compositeur quand la piste, elle,
    /// affiche l'interprète (#1407). Un identifiant qui ne correspond pas au
    /// nom affiché envoie l'auditeur sur la mauvaise fiche.
    ///
    /// `None` quand le service ne le donne pas, ou quand le nom retenu ne vient
    /// d'aucun nœud porteur d'identifiant (Qobuz : nom extrait de la chaîne de
    /// rôles `performers`). Une absence se dit ; elle ne s'invente pas.
    /// `#[serde(default)]` comme `isrc` et `composer` : les résultats
    /// sérialisés antérieurs se chargent encore.
    #[serde(default)]
    pub artist_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamAlbum {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub title: String,
    #[serde(rename(serialize = "artist_name"), alias = "artist_name")]
    pub artist: String,
    pub artist_id: Option<String>,
    pub cover_path: Option<String>,
    pub year: Option<u32>,
    pub track_count: u32,
    pub quality: Option<StreamQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamArtist {
    pub id: String,
    pub name: String,
    pub image_path: Option<String>,
    /// Artist biography as plain text. Currently populated by Qobuz editorial;
    /// other services leave it None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlaylist {
    #[serde(rename(serialize = "source_id"), alias = "source_id")]
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub cover_path: Option<String>,
    pub track_count: u32,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamQuality {
    pub codec: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub bitrate: Option<u32>,
    #[serde(default = "default_channels")]
    pub channels: u16,
}

fn default_channels() -> u16 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamUrl {
    pub url: String,
    pub mime_type: String,
    pub quality: StreamQuality,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    pub tracks: Vec<StreamTrack>,
    pub albums: Vec<StreamAlbum>,
    pub artists: Vec<StreamArtist>,
    pub playlists: Vec<StreamPlaylist>,
}

/// Ce qu'un service annonce POSSÉDER pour une requête, par catégorie.
///
/// Distinct de ce qu'une page rend : c'est la borne haute d'un « Charger plus ».
/// `0` quand le service n'annonce rien — l'absence de total ne s'invente pas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchTotals {
    pub tracks: usize,
    pub albums: usize,
    pub artists: usize,
    pub playlists: usize,
}

/// Une page de résultats de recherche, plus de quoi en demander la suite (#2160).
///
/// `#[serde(flatten)]` : le corps JSON garde EXACTEMENT les quatre clés
/// `tracks` / `albums` / `artists` / `playlists` qu'il portait avant ; les clés
/// de pagination s'ajoutent à côté. Un client antérieur, qui lit `.albums` et
/// ignore le reste, ne voit aucune différence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPage {
    #[serde(flatten)]
    pub results: SearchResults,
    /// Décalage, par catégorie, du premier élément rendu. `0` pour la première
    /// page. C'est le curseur que le client renvoie en `?offset=`.
    pub offset: usize,
    /// Ce que le service annonce en tout, par catégorie.
    pub totals: SearchTotals,
    /// Reste-t-il, dans AU MOINS une catégorie, des éléments au-delà de cette
    /// page ? C'est la condition d'affichage d'un « Charger plus ».
    pub has_more: bool,
    /// La page a-t-elle été écourtée par le plafond du SERVEUR plutôt que par
    /// l'épuisement du catalogue ? « Tous » est borné — sans ce drapeau, un
    /// écran croirait tenir tout ce que Qobuz possède.
    pub truncated: bool,
}

impl SearchPage {
    /// Page unique d'un service qui ne sait pas paginer sa recherche.
    ///
    /// Les totaux valent ce qui est rendu : c'est tout ce que le service dit
    /// savoir. On n'annonce donc jamais une suite qu'on serait incapable de
    /// servir.
    pub fn page_unique(results: SearchResults) -> Self {
        let totals = SearchTotals {
            tracks: results.tracks.len(),
            albums: results.albums.len(),
            artists: results.artists.len(),
            playlists: results.playlists.len(),
        };
        Self {
            results,
            offset: 0,
            totals,
            has_more: false,
            truncated: false,
        }
    }

    /// Page vide au-delà de ce qu'un service sait servir.
    pub fn au_dela(offset: usize) -> Self {
        Self {
            results: SearchResults {
                tracks: Vec::new(),
                albums: Vec::new(),
                artists: Vec::new(),
                playlists: Vec::new(),
            },
            offset,
            totals: SearchTotals::default(),
            has_more: false,
            truncated: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamGenre {
    pub id: String,
    pub name: String,
    pub has_children: bool,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturedSection {
    pub id: String,
    pub name: String,
}

/// A record label with (a page of) its albums. Qobuz `label/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelInfo {
    pub id: String,
    pub name: String,
    pub albums: Vec<StreamAlbum>,
}

/// An editorial playlist tag/category (Qobuz `playlist/getTags`): moods,
/// "Focus", genres… Used to browse curated playlists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistTag {
    pub id: String,
    pub name: String,
}

/// Une catégorie de playlists éditoriales avec sa rangée de playlists — la
/// structure qu'affichent Qobuz et Roon (« Humeurs », « Focus », …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistTagGroup {
    pub id: String,
    pub name: String,
    pub playlists: Vec<StreamPlaylist>,
}

/// Discovery context of an album/track: its genre and record label. Lets a
/// client jump from the now-playing track to the genre's expert playlists or
/// the label's catalogue, without bloating the shared `StreamAlbum` model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlbumContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub username: Option<String>,
    pub subscription: Option<String>,
    pub expires_at: Option<String>,
    pub verification_url: Option<String>,
    pub user_code: Option<String>,
    pub device_code: Option<String>,
    pub expires_in: Option<u64>,
}

#[async_trait::async_trait]
pub trait StreamingService: Send + Sync {
    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
    fn name(&self) -> &str;
    fn enabled(&self) -> bool;
    fn set_enabled(&mut self, enabled: bool);

    async fn authenticate(
        &mut self,
        credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError>;
    async fn auth_status(&self) -> AuthStatus;
    async fn logout(&mut self) -> Result<(), TuneError>;

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError>;

    /// Une PAGE de recherche : `limit` éléments par catégorie à partir de
    /// `offset`, avec de quoi savoir s'il en reste (#2160).
    ///
    /// Défaut : le service ne sait pas paginer sa recherche. À `offset = 0` il
    /// rend sa page unique ; au-delà il rend du vide, car recycler la première
    /// page en prétendant que c'est la seconde ferait afficher deux fois les
    /// mêmes titres.
    async fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchPage, TuneError> {
        if offset > 0 {
            return Ok(SearchPage::au_dela(offset));
        }
        Ok(SearchPage::page_unique(self.search(query, limit).await?))
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError>;
    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError>;
    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError>;
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError>;
    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError>;
    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let _ = artist_id;
        Ok(vec![])
    }
    /// Une PAGE de la discographie, pour un « voir plus ».
    ///
    /// Ajoutée à côté de [`Self::get_artist_albums`] plutôt qu'en remplacement :
    /// six services l'implémentent, et tous ne savent pas paginer. Le repli par
    /// défaut rend la liste unique à `offset = 0` et **rien** au-delà — ce qui
    /// arrête proprement le « voir plus » au lieu de renvoyer indéfiniment la
    /// même première page.
    ///
    /// Un service qui sait paginer surcharge cette méthode ; les autres n'ont
    /// rien à changer et ne régressent pas.
    async fn get_artist_albums_page(
        &self,
        artist_id: &str,
        offset: u32,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        if offset == 0 {
            self.get_artist_albums(artist_id).await
        } else {
            Ok(vec![])
        }
    }
    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let _ = artist_id;
        Ok(vec![])
    }
    /// Artistes similaires SELON LE SERVICE, pour la radio d'autoplay (#1553).
    ///
    /// Jusqu'ici « qui ressemble à qui » n'avait qu'une seule reponse possible :
    /// l'API d'enrichissement mozaiklabs, interrogeable par MBID seulement.
    /// Or ~10 % des artistes en ont un, et une piste Qobuz n'en transporte
    /// aucun : la radio streaming n'avait donc jamais de candidats, et la file
    /// s'arretait en silence (Sandro, 0.9.75).
    ///
    /// Le service qui diffuse la piste connait, lui, son propre catalogue.
    /// Defaut vide : un service sans notion de similarite ne bloque rien, le
    /// couple appelant/repli reste responsable de la suite.
    async fn get_similar_artists(
        &self,
        artist_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamArtist>, TuneError> {
        let _ = (artist_id, limit);
        Ok(vec![])
    }
    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, TuneError>;
    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError>;

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError>;
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError>;
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError>;

    async fn create_playlist(
        &self,
        _name: &str,
        _description: Option<&str>,
    ) -> Result<String, TuneError> {
        Err("create_playlist not supported by this service".into())
    }
    async fn add_tracks_to_playlist(
        &self,
        _playlist_id: &str,
        _track_ids: &[String],
    ) -> Result<usize, TuneError> {
        Err("add_tracks_to_playlist not supported by this service".into())
    }
    async fn delete_playlist(&self, _playlist_id: &str) -> Result<(), TuneError> {
        Err("delete_playlist not supported by this service".into())
    }
    async fn remove_tracks_from_playlist(
        &self,
        _playlist_id: &str,
        _track_ids: &[String],
    ) -> Result<usize, TuneError> {
        Err("remove_tracks_from_playlist not supported by this service".into())
    }
    fn supports_write(&self) -> bool {
        false
    }

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }

    async fn get_genres(&self, parent_id: Option<&str>) -> Result<Vec<StreamGenre>, TuneError> {
        let _ = parent_id;
        Ok(vec![])
    }
    async fn get_genre_albums(
        &self,
        genre_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        let _ = (genre_id, limit);
        Ok(vec![])
    }
    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, TuneError> {
        Ok(vec![])
    }
    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let _ = section_id;
        Ok(vec![])
    }
    /// Browse the record label of an album: resolves the album's label and
    /// returns it with its full catalogue. Album-based so the shared
    /// `StreamAlbum` model need not carry a label id.
    async fn get_album_label(&self, _album_id: &str) -> Result<LabelInfo, TuneError> {
        Err("labels not supported for this service".into())
    }
    /// Editorial playlist tags/categories (moods, "Focus", genres…).
    async fn get_playlist_tags(&self) -> Result<Vec<PlaylistTag>, TuneError> {
        Ok(vec![])
    }
    /// Curated/editorial ("expert") playlists, optionally filtered by a tag id
    /// and/or a genre id.
    async fn get_featured_playlists(
        &self,
        _tag: Option<&str>,
        _genre: Option<&str>,
    ) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    /// Les playlists éditoriales **rangées par catégorie**, une rangée par tag,
    /// comme le fait le service lui-même (Qobuz : « Artistes Qobuz »,
    /// « Humeurs », « Focus », « Histoires de labels »…).
    ///
    /// Un seul aller-retour pour le client, qui n'a pas à découvrir les tags
    /// puis à lancer une requête par tag. Optionnellement narrowé par genre,
    /// comme le sélecteur « Tous les genres » du service.
    async fn get_featured_playlists_by_tag(
        &self,
        _genre: Option<&str>,
    ) -> Result<Vec<PlaylistTagGroup>, TuneError> {
        Ok(vec![])
    }
    /// Discovery context (genre + label) of an album, resolved from the album.
    async fn get_album_context(&self, _album_id: &str) -> Result<AlbumContext, TuneError> {
        Err("album context not supported for this service".into())
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![])
    }
    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let _ = (fav_type, item_id);
        Err("not supported".into())
    }
    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let _ = (fav_type, item_id);
        Err("not supported".into())
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        None
    }
    fn restore_tokens(&mut self, _tokens: &serde_json::Value) -> bool {
        false
    }

    /// Whether the blob just restored is a stale shape that must be rewritten.
    ///
    /// Set by a service when `restore_tokens` reads a row written by an older
    /// build that persisted something it no longer should — today, the Qobuz
    /// plaintext password. Dropping the field from `save_tokens` alone is not
    /// enough: the old value sits in `settings` until something happens to
    /// overwrite the row, which for a working token may be never. The registry
    /// checks this right after restoring and rewrites the row on the spot.
    fn tokens_need_rewrite(&self) -> bool {
        false
    }

    /// Whether the service has established that its session is over and the
    /// persisted row is worthless — the token was rejected and could not be
    /// renewed. The caller deletes the row so a restart does not reload a
    /// credential the provider has already refused.
    ///
    /// Distinct from `save_tokens() == None`, which means "nothing to save
    /// right now" and must leave any existing row alone — `TidalService`
    /// returns exactly that when its mutex is held.
    fn session_expired(&self) -> bool {
        false
    }

    async fn post_restore(&mut self) {}

    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_track_serialization() {
        let track = StreamTrack {
            id: "123".into(),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            album: Some("Kind of Blue".into()),
            album_id: Some("456".into()),
            duration_ms: 562000,
            cover_path: Some("http://example.com/cover.jpg".into()),
            track_number: Some(1),
            disc_number: Some(1),
            explicit: false,
            isrc: Some("USSM19900001".into()),
            composer: None,
            artist_id: None,
            quality: Some(StreamQuality {
                codec: "FLAC".into(),
                sample_rate: 96000,
                bit_depth: 24,
                bitrate: None,
                channels: 2,
            }),
        };
        let json = serde_json::to_value(&track).unwrap();
        // id should serialize as "source_id" due to rename
        assert_eq!(json["source_id"], "123");
        assert_eq!(json["title"], "So What");
        // artist serializes as "artist_name" for web client compatibility
        assert_eq!(json["artist_name"], "Miles Davis");
        // album serializes as "album_title" for web client compatibility
        assert_eq!(json["album_title"], "Kind of Blue");
        assert_eq!(json["cover_path"], "http://example.com/cover.jpg");
        assert_eq!(json["duration_ms"], 562000);
    }

    #[test]
    fn stream_track_deserialization_with_source_id() {
        let json = serde_json::json!({
            "source_id": "abc",
            "title": "Test",
            "artist": "Test Artist",
            "duration_ms": 1000,
            "explicit": false,
        });
        let track: StreamTrack = serde_json::from_value(json).unwrap();
        assert_eq!(track.id, "abc");
        assert_eq!(track.title, "Test");
    }

    #[test]
    fn stream_album_serialization() {
        let album = StreamAlbum {
            id: "789".into(),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            artist_id: Some("42".into()),
            cover_path: Some("http://cover.jpg".into()),
            year: Some(1959),
            track_count: 5,
            quality: None,
        };
        let json = serde_json::to_value(&album).unwrap();
        assert_eq!(json["source_id"], "789");
        assert_eq!(json["title"], "Kind of Blue");
        // artist serializes as "artist_name" for web client compatibility
        assert_eq!(json["artist_name"], "Miles Davis");
        assert_eq!(json["year"], 1959);
        assert_eq!(json["track_count"], 5);
    }

    #[test]
    fn stream_artist_serialization() {
        let artist = StreamArtist {
            id: "42".into(),
            name: "Miles Davis".into(),
            image_path: Some("http://img.jpg".into()),
            bio: None,
        };
        let json = serde_json::to_value(&artist).unwrap();
        assert_eq!(json["id"], "42");
        assert_eq!(json["name"], "Miles Davis");
        assert_eq!(json["image_path"], "http://img.jpg");
    }

    #[test]
    fn stream_playlist_serialization() {
        let playlist = StreamPlaylist {
            id: "pl-1".into(),
            name: "My Playlist".into(),
            description: Some("A great playlist".into()),
            cover_path: None,
            track_count: 10,
            owner: Some("testuser".into()),
        };
        let json = serde_json::to_value(&playlist).unwrap();
        assert_eq!(json["source_id"], "pl-1");
        assert_eq!(json["name"], "My Playlist");
        assert_eq!(json["track_count"], 10);
        assert!(json["cover_path"].is_null());
    }

    #[test]
    fn stream_quality_serialization() {
        let quality = StreamQuality {
            codec: "FLAC".into(),
            sample_rate: 192000,
            bit_depth: 24,
            bitrate: Some(9216),
            channels: 2,
        };
        let json = serde_json::to_value(&quality).unwrap();
        assert_eq!(json["codec"], "FLAC");
        assert_eq!(json["sample_rate"], 192000);
        assert_eq!(json["bit_depth"], 24);
        assert_eq!(json["bitrate"], 9216);
    }

    #[test]
    fn stream_url_serialization() {
        let url = StreamUrl {
            url: "https://stream.example.com/track.flac".into(),
            mime_type: "audio/flac".into(),
            quality: StreamQuality {
                codec: "FLAC".into(),
                sample_rate: 44100,
                bit_depth: 16,
                bitrate: None,
                channels: 2,
            },
            expires_at: Some(1700000000),
        };
        let json = serde_json::to_value(&url).unwrap();
        assert_eq!(json["url"], "https://stream.example.com/track.flac");
        assert_eq!(json["mime_type"], "audio/flac");
        assert_eq!(json["expires_at"], 1700000000);
    }

    #[test]
    fn search_results_serialization() {
        let results = SearchResults {
            tracks: vec![],
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        };
        let json = serde_json::to_value(&results).unwrap();
        assert!(json["tracks"].as_array().unwrap().is_empty());
        assert!(json["albums"].as_array().unwrap().is_empty());
    }

    /// #2160 — la pagination ne doit pas déplacer les quatre clés existantes.
    ///
    /// C'est la garantie de non-régression qui compte pour les clients déjà
    /// installés : ils lisent `réponse.albums`. Si le `#[serde(flatten)]`
    /// disparaissait, la recherche Qobuz deviendrait un écran vide sur toutes
    /// les versions antérieures du client.
    #[test]
    fn une_page_de_recherche_garde_les_quatre_cles_a_la_racine() {
        let page = SearchPage::page_unique(SearchResults {
            tracks: vec![],
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        });
        let json = serde_json::to_value(&page).unwrap();

        for cle in ["tracks", "albums", "artists", "playlists"] {
            assert!(
                json[cle].is_array(),
                "`{cle}` doit rester un tableau À LA RACINE, pas sous `results`"
            );
        }
        assert!(json.get("results").is_none(), "aucune enveloppe `results`");
        assert_eq!(json["offset"], 0);
        assert_eq!(json["has_more"], false);
        assert_eq!(json["truncated"], false);
        assert_eq!(json["totals"]["tracks"], 0);
    }

    /// Un service qui ne sait pas paginer sa recherche annonce des totaux
    /// égaux à ce qu'il rend : il ne promet pas une suite qu'il ne servirait
    /// pas.
    #[test]
    fn une_page_unique_n_annonce_jamais_de_suite() {
        let page = SearchPage::page_unique(SearchResults {
            tracks: vec![],
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        });
        assert!(!page.has_more);
        assert!(!page.truncated);
        assert_eq!(page.offset, 0);
        assert_eq!(page.totals, SearchTotals::default());
    }

    /// Au-delà de ce qu'un service sait servir, on rend du VIDE — jamais la
    /// première page recyclée, qui ferait afficher deux fois les mêmes titres.
    #[test]
    fn au_dela_rend_du_vide_et_se_nomme() {
        let page = SearchPage::au_dela(200);
        assert_eq!(page.offset, 200);
        assert!(page.results.tracks.is_empty());
        assert!(page.results.albums.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn auth_status_default() {
        let status = AuthStatus::default();
        assert!(!status.authenticated);
        assert!(status.username.is_none());
        assert!(status.subscription.is_none());
        assert!(status.verification_url.is_none());
        assert!(status.user_code.is_none());
    }

    #[test]
    fn auth_status_serialization() {
        let status = AuthStatus {
            authenticated: true,
            username: Some("testuser".into()),
            subscription: Some("Premium".into()),
            expires_at: Some("3600s".into()),
            verification_url: None,
            user_code: None,
            ..Default::default()
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["authenticated"], true);
        assert_eq!(json["username"], "testuser");
        assert_eq!(json["subscription"], "Premium");
    }

    #[test]
    fn stream_genre_serialization() {
        let genre = StreamGenre {
            id: "jazz".into(),
            name: "Jazz".into(),
            has_children: true,
            image_url: Some("http://img.jpg".into()),
        };
        let json = serde_json::to_value(&genre).unwrap();
        assert_eq!(json["id"], "jazz");
        assert_eq!(json["has_children"], true);
    }

    #[test]
    fn featured_section_serialization() {
        let section = FeaturedSection {
            id: "new-releases".into(),
            name: "New Releases".into(),
        };
        let json = serde_json::to_value(&section).unwrap();
        assert_eq!(json["id"], "new-releases");
        assert_eq!(json["name"], "New Releases");
    }
}
