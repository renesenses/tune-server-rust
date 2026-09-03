//! Bandcamp vu par le REGISTRE des services de streaming (#2702, #2778).
//!
//! # Le défaut
//!
//! Bandcamp était un greffon et rien d'autre : ses routes vivaient sous
//! `/api/v1/ext/bandcamp/…`, et il n'existait nulle part dans
//! `AppState::services`. Or les deux seules routes qui savent construire une
//! file COMPLÈTE — `POST /zones/{id}/play` avec `streaming_album_id` ou
//! `streaming_playlist_id` — commencent par `registry.get(source)`. Pour
//! `source = "bandcamp"` elles répondaient donc **400 `unknown service:
//! bandcamp`**, et il ne restait au client que le chemin « piste distante
//! seule », qui finit par `update_queue_info(zone, 0, 1)` : une file d'EXACTEMENT
//! une piste. C'est le défaut de Sevy Tabroc — « les morceaux ne s'enchaînent
//! pas » (#2702) : il n'y a jamais eu de piste suivante à enchaîner.
//!
//! Symétriquement, l'état de liaison du lot 2 n'était lisible par AUCUNE
//! route : le greffon écrit `bandcamp_username` et `bandcamp_fan_id` et ne les
//! rend jamais. Un pseudo qui ne s'était pas enregistré restait donc invisible
//! jusqu'au `GET /collection` suivant, qui répondait « aucun compte lié » sans
//! dire pourquoi — le « identifiant perdu » de FabienM (#2778).
//!
//! # Ce que l'adaptateur change, et ce qu'il NE change pas
//!
//! Il ne touche pas au chemin de LECTURE. `resolve_stream` route déjà
//! `source == "bandcamp"` vers `resolve_direct_url`, parce qu'un `source_id`
//! Bandcamp EST une URL mp3-128 directement jouable. C'est la raison pour
//! laquelle [`StreamTrack::id`] porte ici l'URL de flux et non l'identifiant
//! numérique de Bandcamp : la file écrite par la route de lecture doit rester
//! lisible par le chemin qui existe. Un `track_id` numérique aurait donné une
//! file de plusieurs pistes… dont aucune n'aurait joué.
//!
//! # Ce que Bandcamp NE PEUT PAS tenir du contrat
//!
//! Le trait exige dix-neuf méthodes. Bandcamp en honore une partie seulement,
//! et les autres **le disent** plutôt que de rendre du vide qui passerait pour
//! une réponse :
//!
//! - **Playlists** : Bandcamp n'en a pas. `get_playlist`, `get_playlist_tracks`
//!   et `get_user_playlists` rendent une erreur qui le nomme. C'est aussi
//!   pourquoi `POST /zones/{id}/play` avec `streaming_playlist_id` +
//!   `source: "bandcamp"` répondra 502 « Bandcamp n'a pas de playlists » au
//!   lieu du 400 `unknown service` d'avant : l'échec est NOMMÉ.
//! - **Pistes de recherche** : `autocomplete_elastic` ne rend aucune URL de
//!   flux. Une `StreamTrack` bâtie dessus aurait un `id` injouable ;
//!   [`BandcampService::search`] ne rend donc que des albums et des artistes,
//!   dont l'`id` (l'adresse de la page) EST résoluble par
//!   [`BandcampService::get_album_tracks`].
//! - **Qualité** : mp3-128, seule qualité servie sans session d'achat — voir la
//!   note de portée du module parent.
//! - **`get_track` / `get_track_url`** : Bandcamp n'expose pas une piste seule
//!   par son URL de flux. `get_track_url` rend l'URL telle quelle (c'est déjà
//!   la ressource jouable) ; `get_track` refuse, faute de pouvoir remplir un
//!   titre et un artiste sans mentir.
//! - **`get_artist_albums`** : la discographie EXISTE côté greffon
//!   ([`crate::parutions_discographie`]) mais son format n'est pas celui du
//!   trait ; le défaut du trait (liste vide) est conservé plutôt que d'être
//!   mal rempli. Voir « ce qui reste » dans la PR.
//!
//! L'authentification, elle, n'est pas un manque : `authenticate` accepte
//! `{"username": "…"}` et fait EXACTEMENT ce que fait `POST /collection/link`,
//! par le même corps ([`crate::lier_compte`]). C'est ce qui rend enfin l'état
//! de liaison lisible, par `GET /api/v1/streaming/bandcamp/status`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tune_core::TuneError;
use tune_core::db::backend::DbBackend;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};

use crate::{
    BC_SEARCH_API, EchecLiaison, album_depuis_url, chercher_une_categorie, compte_lie,
    delier_compte, lier_compte, page_de_collection, pochette,
};

/// Le jeton de première page de la collection — même convention que le greffon.
const BC_JETON_DEBUT: &str = "9999999999::a::";

/// Combien d'articles de collection `get_user_albums` rapporte.
///
/// Le plafond d'une page Bandcamp est 100 ; la route du trait ne pagine pas, on
/// prend donc la page pleine et on s'arrête là. Le greffon garde la pagination
/// complète sous `GET /collection?older_than_token=…`.
const BC_COLLECTION_PAGE: u32 = 100;

/// Ce que Bandcamp sert à qui n'a pas de session d'achat.
fn qualite_bandcamp() -> StreamQuality {
    StreamQuality {
        codec: "MP3".into(),
        sample_rate: 44100,
        bit_depth: 16,
        bitrate: Some(128_000),
        channels: 2,
    }
}

/// Bandcamp dans le registre des services de streaming.
pub struct BandcampService {
    backend: Arc<dyn DbBackend>,
    enabled: bool,
}

impl BandcampService {
    pub fn new(backend: Arc<dyn DbBackend>) -> Self {
        Self {
            backend,
            // Comme le greffon : opt-in. `enabled` ne garde AUCUNE des routes
            // utilisées ici — ni `registry.get`, ni la route de file — il ne
            // décide que de l'affichage dans le gestionnaire de services.
            enabled: false,
        }
    }
}

/// Les pistes d'un album, telles que le registre les attend.
///
/// Prend la sortie de [`crate::album_jouable`] — donc le MÊME extracteur que
/// `GET /ext/bandcamp/album?url=…`, jamais un second. Pure : c'est elle que
/// l'épreuve de la file interroge, sans réseau.
pub(crate) fn pistes_depuis_album(album: &Value) -> Vec<StreamTrack> {
    let album_url = album["url"].as_str().map(str::to_string);
    let album_titre = album["title"].as_str().map(str::to_string);
    let artiste_album = album["artist"].as_str().unwrap_or_default();
    let pochette_album = album["pochette"].as_str().map(str::to_string);
    album["tracks"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            // 🔴 L'`id` d'une piste Bandcamp est son URL de flux, et rien
            // d'autre : `resolve_direct_url` la joue telle quelle. Une piste
            // sans URL n'entre pas dans la file — elle y échouerait.
            let url = t["stream_url"].as_str()?;
            if url.is_empty() {
                return None;
            }
            Some(StreamTrack {
                id: url.to_string(),
                title: t["title"].as_str().unwrap_or_default().to_string(),
                artist: t["artist"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                    .unwrap_or(artiste_album)
                    .to_string(),
                album: album_titre.clone(),
                album_id: album_url.clone(),
                duration_ms: t["duration_s"]
                    .as_f64()
                    .filter(|d| *d > 0.0)
                    .map(|d| (d * 1000.0) as u64)
                    .unwrap_or(0),
                cover_path: pochette_album.clone(),
                track_number: t["num"].as_u64().map(|n| n as u32),
                disc_number: None,
                explicit: false,
                quality: Some(qualite_bandcamp()),
                isrc: None,
                composer: None,
                artist_id: None,
            })
        })
        .collect()
}

/// L'album lui-même, tel que le registre l'attend.
pub(crate) fn album_depuis_json(album: &Value) -> StreamAlbum {
    StreamAlbum {
        id: album["url"].as_str().unwrap_or_default().to_string(),
        title: album["title"].as_str().unwrap_or_default().to_string(),
        artist: album["artist"].as_str().unwrap_or_default().to_string(),
        artist_id: None,
        cover_path: album["pochette"].as_str().map(str::to_string),
        year: None,
        track_count: album["track_count"].as_u64().unwrap_or(0) as u32,
        quality: Some(qualite_bandcamp()),
    }
}

/// Les albums d'une page de collection, tels que le registre les attend.
///
/// Ne garde que les articles dont l'`item_url` est une adresse Bandcamp : c'est
/// le seul `id` que [`BandcampService::get_album_tracks`] saura rouvrir. Un
/// article sans adresse serait un album qu'on affiche et qui ne joue pas —
/// exactement le reproche de FabienM (#2778).
pub(crate) fn albums_de_collection(brut: &Value) -> Vec<StreamAlbum> {
    brut["items"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|it| {
            let url = it["item_url"].as_str()?;
            if !url.starts_with("https://") || !url.contains("bandcamp.com") {
                return None;
            }
            Some(StreamAlbum {
                id: url.to_string(),
                title: it["item_title"].as_str().unwrap_or_default().to_string(),
                artist: it["band_name"].as_str().unwrap_or_default().to_string(),
                artist_id: None,
                cover_path: pochette(it.get("item_art_id")),
                year: None,
                track_count: 0,
                quality: Some(qualite_bandcamp()),
            })
        })
        .collect()
}

/// Les albums d'une page de recherche, tels que le registre les attend.
pub(crate) fn albums_de_recherche(resultats: &[Value]) -> Vec<StreamAlbum> {
    resultats
        .iter()
        .filter_map(|r| {
            let url = r["url"].as_str()?;
            Some(StreamAlbum {
                id: url.to_string(),
                title: r["titre"].as_str().unwrap_or_default().to_string(),
                artist: r["artiste"].as_str().unwrap_or_default().to_string(),
                artist_id: None,
                cover_path: r["pochette"].as_str().map(str::to_string),
                year: None,
                track_count: 0,
                quality: Some(qualite_bandcamp()),
            })
        })
        .collect()
}

/// Les artistes d'une page de recherche, tels que le registre les attend.
pub(crate) fn artistes_de_recherche(resultats: &[Value]) -> Vec<StreamArtist> {
    resultats
        .iter()
        .filter_map(|r| {
            let url = r["url"].as_str()?;
            Some(StreamArtist {
                id: url.to_string(),
                name: r["titre"].as_str().unwrap_or_default().to_string(),
                image_path: r["pochette"].as_str().map(str::to_string),
                bio: None,
            })
        })
        .collect()
}

/// Ce que rend une méthode que Bandcamp ne peut pas tenir.
///
/// Une fonction et non un `Err("…")` répété : le refus doit se lire pareil
/// partout, et surtout ne jamais être confondu avec « le service a répondu et
/// n'a rien ».
fn hors_portee(quoi: &str) -> TuneError {
    TuneError::from(format!("Bandcamp ne fournit pas {quoi}"))
}

/// L'échec de résolution d'un album, dit au registre.
///
/// Une fonction plutôt qu'un `impl From` : [`crate::EchecAlbum`] est
/// `pub(crate)` et n'a pas à apparaître dans une implémentation publique.
fn echec_album(e: crate::EchecAlbum) -> TuneError {
    TuneError::from(e.message().to_string())
}

#[async_trait]
impl StreamingService for BandcampService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn name(&self) -> &str {
        "bandcamp"
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Lier un compte : `{"username": "<pseudo>"}`.
    ///
    /// Aucun mot de passe — la page de profil est publique. Un corps SANS
    /// `username` (celui que `service_status` envoie pour sonder un flot par
    /// code d'appareil) ne déclenche aucun appel sortant : il rend simplement
    /// l'état courant.
    async fn authenticate(&mut self, credentials: &Value) -> Result<AuthStatus, TuneError> {
        let Some(pseudo) = credentials.get("username").and_then(|v| v.as_str()) else {
            return Ok(self.auth_status().await);
        };
        match lier_compte(&self.backend, pseudo).await {
            Ok(compte) => Ok(AuthStatus {
                authenticated: true,
                username: Some(compte.pseudo),
                ..Default::default()
            }),
            Err(EchecLiaison::PseudoInvalide) => Err(TuneError::from("username invalide")),
            Err(EchecLiaison::ProfilIntrouvable(m) | EchecLiaison::Passerelle(m)) => {
                Err(TuneError::from(m))
            }
            // 🔴 #2778 — l'écriture qui échoue REMONTE au lieu d'être jetée.
            Err(EchecLiaison::Ecriture(m)) => Err(TuneError::from(m)),
        }
    }

    /// L'état de liaison, enfin lisible par une route (#2778).
    ///
    /// Une base illisible n'est PAS « aucun compte lié » : elle se trace et
    /// rend `authenticated: false` en le disant au journal, faute de pouvoir
    /// porter une erreur dans `AuthStatus`.
    async fn auth_status(&self) -> AuthStatus {
        match compte_lie(&self.backend) {
            Ok(Some(compte)) => AuthStatus {
                authenticated: true,
                username: Some(compte.pseudo),
                ..Default::default()
            },
            Ok(None) => AuthStatus::default(),
            Err(e) => {
                tracing::error!(erreur = %e, "bandcamp_auth_status_reglages_illisibles");
                AuthStatus::default()
            }
        }
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        delier_compte(&self.backend).map_err(TuneError::from)
    }

    /// Albums et artistes. Pas de pistes — voir la note de portée du module.
    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        let (albums, artistes) = tokio::join!(
            chercher_une_categorie(BC_SEARCH_API, query, "a"),
            chercher_une_categorie(BC_SEARCH_API, query, "b"),
        );
        let albums = albums.map_err(TuneError::from)?;
        // Un onglet artiste en échec ne fait pas tomber les albums : même
        // dégradation que `recherche_repartie`, et elle se trace.
        let artistes = artistes.unwrap_or_else(|e| {
            tracing::warn!(erreur = %e, "bandcamp_recherche_artistes_en_echec");
            Vec::new()
        });
        let mut albums = albums_de_recherche(&albums);
        albums.truncate(limit);
        let mut artists = artistes_de_recherche(&artistes);
        artists.truncate(limit);
        Ok(SearchResults {
            tracks: Vec::new(),
            albums,
            artists,
            playlists: Vec::new(),
        })
    }

    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, TuneError> {
        Err(hors_portee(
            "de fiche pour une piste seule — passer par son album",
        ))
    }

    /// L'identifiant d'une piste Bandcamp EST son URL de flux : la rendre telle
    /// quelle n'est pas un raccourci, c'est la ressource.
    async fn get_track_url(
        &self,
        track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        if !track_id.starts_with("http") {
            return Err(TuneError::from(
                "identifiant de piste Bandcamp attendu sous forme d'URL de flux",
            ));
        }
        Ok(StreamUrl {
            url: track_id.to_string(),
            mime_type: "audio/mpeg".into(),
            quality: qualite_bandcamp(),
            expires_at: None,
        })
    }

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError> {
        let album = album_depuis_url(album_id).await.map_err(echec_album)?;
        Ok(album_depuis_json(&album))
    }

    /// 🔴 #2702 — c'est CETTE méthode que `POST /zones/{id}/play` appelle avec
    /// `streaming_album_id`, et son absence est tout le défaut : la route
    /// s'arrêtait avant, sur `unknown service: bandcamp`.
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let album = album_depuis_url(album_id).await.map_err(echec_album)?;
        let pistes = pistes_depuis_album(&album);
        if pistes.is_empty() {
            return Err(TuneError::from(
                "aucune piste jouable sur cette page Bandcamp (précommande, ou album non encodé)",
            ));
        }
        Ok(pistes)
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError> {
        if !artist_id.starts_with("https://") || !artist_id.contains("bandcamp.com") {
            return Err(TuneError::from(
                "identifiant d'artiste Bandcamp attendu sous forme d'adresse https bandcamp.com",
            ));
        }
        Ok(StreamArtist {
            id: artist_id.to_string(),
            // Bandcamp ne sert pas de fiche artiste par API ; le nom vit dans
            // la page, que `GET /ext/bandcamp/artist?url=` sait déjà lire. Ne
            // rien inventer ici : l'identifiant suffit à rouvrir la page.
            name: String::new(),
            image_path: None,
            bio: None,
        })
    }

    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(hors_portee("de playlists"))
    }

    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(hors_portee("de playlists"))
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Err(hors_portee("de playlists"))
    }

    /// La collection de l'acheteur — « Ma collection » de FabienM (#2778).
    ///
    /// Chaque album rendu porte pour `id` son adresse Bandcamp, donc
    /// [`Self::get_album_tracks`] sait la rouvrir et la route de file sait
    /// l'enfiler. C'est ce qui rend la collection JOUABLE, et pas seulement
    /// affichable.
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        let compte = compte_lie(&self.backend)
            .map_err(TuneError::from)?
            .ok_or_else(|| {
                TuneError::from(
                    "aucun compte Bandcamp lié — POST /api/v1/streaming/bandcamp/auth \
                     avec {\"username\": \"…\"} d'abord",
                )
            })?;
        let brut = page_de_collection(compte.fan_id, BC_JETON_DEBUT, BC_COLLECTION_PAGE)
            .await
            .map_err(TuneError::from)?;
        Ok(albums_de_collection(&brut))
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Err(hors_portee(
            "de liste d'artistes suivis exploitable sans session d'achat",
        ))
    }

    /// Rien à mémoriser : le compte lié vit dans les réglages, écrit par
    /// [`crate::lier_compte`], et pas dans un jeton que le registre saurait
    /// sauver. Rendre `None` évite qu'une ligne `auth_tokens_bandcamp` vide
    /// soit posée à chaque `save_all_tokens`.
    fn save_tokens(&self) -> Option<Value> {
        None
    }

    fn restore_tokens(&mut self, _tokens: &Value) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::album_jouable;
    use serde_json::json;

    /// Un `data-tralbum` réduit à ce que l'extracteur lit, avec TROIS pistes.
    fn tralbum_trois_pistes() -> Value {
        json!({
            "url": "https://artiste.bandcamp.com/album/disque",
            "artist": "Artiste",
            "art_id": 4029072179i64,
            "current": { "title": "Disque" },
            "trackinfo": [
                {
                    "track_id": 1, "track_num": 1, "title": "Une",
                    "artist": null, "duration": 121.5, "streaming": 1,
                    "file": { "mp3-128": "https://t4.bcbits.com/stream/aaa?token=1" }
                },
                {
                    "track_id": 2, "track_num": 2, "title": "Deux",
                    "artist": "Invité", "duration": 200.0, "streaming": 1,
                    "file": { "mp3-128": "https://t4.bcbits.com/stream/bbb?token=2" }
                },
                {
                    "track_id": 3, "track_num": 3, "title": "Trois",
                    "artist": null, "duration": 60.0, "streaming": 1,
                    "file": { "mp3-128": "https://t4.bcbits.com/stream/ccc?token=3" }
                },
                // Précommande : pas de flux, donc pas dans la file.
                {
                    "track_id": 4, "track_num": 4, "title": "Quatre",
                    "artist": null, "duration": 90.0, "streaming": 0,
                    "file": { "mp3-128": "https://t4.bcbits.com/stream/ddd" }
                }
            ]
        })
    }

    /// 🔴 #2702 — l'épreuve qui tranche : un album Bandcamp donne une file de
    /// PLUSIEURS pistes, pas une seule.
    #[test]
    fn un_album_bandcamp_donne_plusieurs_pistes() {
        let pistes = pistes_depuis_album(&album_jouable(&tralbum_trois_pistes()));
        assert!(
            pistes.len() > 1,
            "une file d'une seule piste ne s'enchaîne pas (#2702) — obtenu {}",
            pistes.len()
        );
        assert_eq!(pistes.len(), 3, "la piste en précommande n'entre pas");
        assert_eq!(
            pistes.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(),
            ["Une", "Deux", "Trois"]
        );
        assert_eq!(
            pistes.iter().map(|p| p.track_number).collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3)]
        );
    }

    /// L'`id` d'une piste DOIT être son URL de flux : c'est ce que
    /// `resolve_direct_url` joue. Un identifiant numérique donnerait une file
    /// de plusieurs pistes dont aucune ne jouerait.
    #[test]
    fn l_identifiant_d_une_piste_est_son_url_de_flux() {
        let pistes = pistes_depuis_album(&album_jouable(&tralbum_trois_pistes()));
        for p in &pistes {
            assert!(
                p.id.starts_with("https://") && p.id.contains("bcbits.com"),
                "id injouable par resolve_direct_url : {}",
                p.id
            );
        }
    }

    /// L'artiste de la piste prime, l'artiste de l'album comble.
    #[test]
    fn l_artiste_de_l_album_comble_une_piste_sans_artiste() {
        let pistes = pistes_depuis_album(&album_jouable(&tralbum_trois_pistes()));
        assert_eq!(pistes[0].artist, "Artiste");
        assert_eq!(pistes[1].artist, "Invité");
    }

    /// La durée passe en millisecondes, la pochette et l'album descendent sur
    /// chaque piste — sans quoi la file s'affiche nue et le DIDL du renderer
    /// part sans image.
    #[test]
    fn chaque_piste_porte_album_pochette_et_duree() {
        let pistes = pistes_depuis_album(&album_jouable(&tralbum_trois_pistes()));
        assert_eq!(pistes[0].duration_ms, 121_500);
        assert_eq!(pistes[0].album.as_deref(), Some("Disque"));
        assert_eq!(
            pistes[0].album_id.as_deref(),
            Some("https://artiste.bandcamp.com/album/disque")
        );
        assert!(
            pistes[0]
                .cover_path
                .as_deref()
                .is_some_and(|u| u.contains("/img/a4029072179")),
            "pochette absente ou sans le préfixe `a` : {:?}",
            pistes[0].cover_path
        );
    }

    /// Une page sans aucune piste jouable ne rend pas une file vide en silence.
    #[test]
    fn une_page_sans_piste_jouable_ne_rend_rien() {
        let vide = json!({ "url": "u", "artist": "a", "current": {"title": "t"},
                           "trackinfo": [] });
        assert!(pistes_depuis_album(&album_jouable(&vide)).is_empty());
    }

    #[test]
    fn l_album_porte_son_adresse_pour_identifiant() {
        let album = album_depuis_json(&album_jouable(&tralbum_trois_pistes()));
        assert_eq!(album.id, "https://artiste.bandcamp.com/album/disque");
        assert_eq!(album.title, "Disque");
        assert_eq!(album.artist, "Artiste");
        assert_eq!(album.track_count, 3);
    }

    /// Un article de collection sans adresse Bandcamp est écarté : l'afficher
    /// donnerait un album qui ne joue pas (#2778).
    #[test]
    fn la_collection_n_garde_que_les_articles_rouvrables() {
        let brut = json!({
            "items": [
                { "item_url": "https://a.bandcamp.com/album/un",
                  "item_title": "Un", "band_name": "A", "item_art_id": 12 },
                { "item_url": null, "item_title": "Sans adresse", "band_name": "B" },
                { "item_url": "http://ailleurs.example/x", "item_title": "Hors site",
                  "band_name": "C" }
            ]
        });
        let albums = albums_de_collection(&brut);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].id, "https://a.bandcamp.com/album/un");
        assert!(
            albums[0]
                .cover_path
                .as_deref()
                .is_some_and(|u| u.contains("/img/a12")),
        );
    }

    #[tokio::test]
    async fn une_url_de_flux_se_rend_telle_quelle() {
        let svc = service_de_test();
        let url = "https://t4.bcbits.com/stream/aaa?token=1";
        let rendu = svc.get_track_url(url, None).await.unwrap();
        assert_eq!(rendu.url, url);
        assert_eq!(rendu.mime_type, "audio/mpeg");
    }

    #[tokio::test]
    async fn une_playlist_bandcamp_se_refuse_en_le_disant() {
        let e = service_de_test()
            .get_playlist_tracks("x")
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("playlists"),
            "le refus doit NOMMER ce qui manque : {e}"
        );
    }

    /// 🔴 #2778 — l'état de liaison est LISIBLE, et il distingue « pas de
    /// compte » de « compte lié ». C'est ce que `GET /streaming/bandcamp/status`
    /// sert, et il n'existait aucune route pour l'obtenir.
    #[tokio::test]
    async fn l_etat_de_liaison_se_lit() {
        let svc = service_de_test();
        assert!(
            !svc.auth_status().await.authenticated,
            "aucun compte lié au départ"
        );
        let reglages =
            tune_core::db::settings_repo::SettingsRepo::with_backend(svc.backend.clone());
        reglages.set(crate::CLE_PSEUDO, "fabienm").unwrap();
        reglages.set(crate::CLE_FAN_ID, "897100").unwrap();
        let etat = svc.auth_status().await;
        assert!(etat.authenticated);
        assert_eq!(etat.username.as_deref(), Some("fabienm"));
    }

    /// `logout` oublie le compte, et son échec remonterait — il n'est plus jeté.
    #[tokio::test]
    async fn delier_oublie_le_compte() {
        let mut svc = service_de_test();
        let reglages =
            tune_core::db::settings_repo::SettingsRepo::with_backend(svc.backend.clone());
        reglages.set(crate::CLE_PSEUDO, "fabienm").unwrap();
        reglages.set(crate::CLE_FAN_ID, "897100").unwrap();
        svc.logout().await.unwrap();
        assert!(!svc.auth_status().await.authenticated);
    }

    /// Un corps d'authentification sans `username` — celui que `service_status`
    /// envoie pour sonder un flot par code d'appareil — ne déclenche AUCUN
    /// appel sortant : il rend l'état courant.
    #[tokio::test]
    async fn un_sondage_sans_pseudo_ne_sort_pas_sur_le_reseau() {
        let mut svc = service_de_test();
        let etat = svc.authenticate(&json!({"poll": true})).await.unwrap();
        assert!(!etat.authenticated);
    }

    /// Une base en mémoire AVEC son schéma : la table `settings` naît d'une
    /// MIGRATION, pas d'`init_schema`. Sans `run_migrations` toute écriture de
    /// réglage échoue, et ces épreuves seraient vertes contre rien.
    fn service_de_test() -> BandcampService {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        BandcampService::new(Arc::new(db))
    }
}
