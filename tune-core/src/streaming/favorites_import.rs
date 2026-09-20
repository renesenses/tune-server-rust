//! Reprise dans Tune des favoris posés CHEZ le service (#3419).
//!
//! # Deux magasins qui ne se parlaient pas
//!
//! Un favori de streaming existait à deux endroits, sans passerelle :
//!
//! * **chez le service** — lu en direct par
//!   `GET /streaming/{service}/favorites/{type}`, jamais écrit nulle part ;
//! * **dans Tune** — la table `streaming_favorites`, alimentée UNIQUEMENT par
//!   le cœur cliqué dans Tune (`POST /profiles/{id}/favorites/streaming/add`).
//!
//! Mesuré sur le .18 le 05/09/2026 : 14 pistes et 3 albums en favori chez
//! Qobuz, 2 lignes dans `streaming_favorites`. Les 14 pistes mises en favori
//! depuis l'application Qobuz n'étaient nulle part dans Tune.
//!
//! # Ce que cela cassait, au-delà de l'affichage
//!
//! `tune-smart-http`, `track_favorites_sub` : une règle de collection
//! intelligente « Favori · est · Piste » réunit les favoris locaux et **les
//! pistes locales dont le jumeau distant est en favori**, par rapprochement
//! `lower(trim(titre))` + `lower(trim(artiste))`. L'intention est la bonne ;
//! elle joint `streaming_favorites`, donc elle ne voyait que la moitié la
//! moins fréquente des favoris — on met ses favoris dans l'application du
//! service bien plus souvent que dans Tune. D'où « j'ai 3 pistes Qobuz en
//! favori !! » et une règle qui rend 0 album.
//!
//! # Ce que fait cette reprise, et ce qu'elle ne fait PAS
//!
//! Elle **ajoute**, elle ne retire jamais. `StreamingFavoritesRepo::add` porte
//! déjà `ON CONFLICT … DO NOTHING` sur `(profile_id, item_type, service,
//! service_id)` : la repasser est sans effet, et elle n'écrase aucun libellé
//! posé par Tune.
//!
//! Ne pas retirer est un choix, pas un oubli. Une réconciliation — « ce que le
//! service dit fait foi » — effacerait les favoris posés dans Tune sur des
//! objets que le service ne connaît pas, et la table ne distingue pas
//! aujourd'hui les deux origines. Trancher cela demande une colonne d'origine
//! et un arbitrage produit ; ajouter n'en demande aucun.
//!
//! Enfin la **date** : `add` inscrit l'instant de la reprise, car les routes de
//! favoris des services ne transportent aucune date (#3489, mesuré le
//! 06/09/2026 sur les trois listes). C'est la date à laquelle Tune l'a appris,
//! et non celle où l'auditeur a cliqué le cœur chez Qobuz. Tant que #3489 n'est
//! pas réglé, il n'y a rien de plus juste à écrire — et inventer une date
//! serait pire que de dire laquelle on a.

use std::sync::Arc;

use serde::Serialize;
use tracing::{info, warn};

use super::traits::StreamingService;
use crate::db::backend::DbBackend;
use crate::db::streaming_favorites_repo::StreamingFavoritesRepo;

/// Le compte d'un passage, par type et au total.
///
/// `echecs` compte les types que le service n'a pas su rendre : un service qui
/// échoue sur les pistes ne doit pas faire perdre ses albums, et un passage
/// qui rend `lus: 0, echecs: 3` ne se lit pas du tout comme un compte
/// « aucun favori ».
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RepriseFavoris {
    /// Favoris lus chez le service.
    pub lus: usize,
    /// Favoris qui n'étaient pas encore dans la table de Tune.
    pub ajoutes: usize,
    /// Favoris déjà connus de Tune — la reprise est idempotente.
    pub deja_presents: usize,
    /// Types que le service n'a pas rendus (jeton expiré, panne réseau…).
    pub echecs: usize,
    /// Favoris déjà connus dont la date a été REMISE à celle du service —
    /// ceux qu'une reprise d'avant avait datés au même instant (fil 1780).
    #[serde(default)]
    pub redates: usize,
}

impl RepriseFavoris {
    fn cumuler(&mut self, autre: RepriseFavoris) {
        self.lus += autre.lus;
        self.ajoutes += autre.ajoutes;
        self.deja_presents += autre.deja_presents;
        self.echecs += autre.echecs;
        self.redates += autre.redates;
    }
}

/// Un favori du service, réduit à ce que la table de Tune mémorise.
struct Entree {
    item_type: &'static str,
    service_id: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    cover_url: Option<String>,
    /// La date de mise en favori CHEZ LE SERVICE (ISO 8601), quand il la
    /// donne. `None` = la reprise datera au « maintenant » du moteur.
    created_at: Option<String>,
}

/// Les entrées DATÉES d'un type, par `get_user_favorites_dated` (#3489) —
/// `None` quand le service ne date pas, et l'appelant retombe alors sur la
/// lecture typée d'avant.
///
/// 🔴 Fabien, fil 1780 (16/09/2026) : « Favoris Qobuz : tri par ajout récent
/// ne fonctionne pas, c'est l'ordre alphabétique, juste le dernier titre
/// ajouté remonte en premier ». `get_user_favorites_dated` existait depuis
/// #3489 et la route Streaming s'en servait — mais PAS cette reprise, qui
/// posait `created_at = maintenant` sur tout ce qu'elle importait d'un coup.
/// Cent favoris à la même seconde ⇒ égalité ⇒ départage alphabétique ; seul
/// le favori ajouté APRÈS, daté à part, ressortait en tête. Exactement ce
/// qu'il décrit.
async fn entrees_datees(svc: &dyn StreamingService, fav_type: &str) -> Option<Vec<Entree>> {
    let items = svc
        .get_user_favorites_dated(fav_type)
        .await
        .ok()
        .flatten()?;
    let item_type: &'static str = match fav_type {
        "tracks" => "track",
        "albums" => "album",
        _ => "artist",
    };
    // 🔴 #4552 — les clés sont celles que la projection SÉRIALISE, pas les
    // noms de champs Rust. `StreamTrack` / `StreamAlbum` sortent `id` sous
    // `source_id`, `artist` sous `artist_name`, `album` sous `album_title`
    // (`traits.rs`, `rename(serialize = …)`) ; `StreamArtist` garde `id` et
    // `name`. Lire `id` / `artist` / `album` donnait un `service_id` VIDE à
    // chaque entrée Qobuz et Tidal, que `enregistrer` écarte : la reprise
    // répondait `lus: 0` (.18, 19/09 : 36 pistes datées chez Qobuz). La
    // première clé est la forme réelle, les suivantes les formes anciennes.
    let texte = |v: &serde_json::Value, cles: &[&str]| {
        cles.iter()
            .find_map(|c| v.get(*c).and_then(|x| x.as_str()))
            .map(str::to_string)
    };
    let id = |v: &serde_json::Value| {
        ["source_id", "id"]
            .iter()
            .find_map(|c| match v.get(*c) {
                Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
                Some(serde_json::Value::Number(n)) => Some(n.to_string()),
                _ => None,
            })
            .unwrap_or_default()
    };
    Some(
        items
            .iter()
            .map(|v| match item_type {
                "track" => Entree {
                    item_type,
                    service_id: id(v),
                    title: texte(v, &["title"]),
                    artist: texte(v, &["artist_name", "artist"]),
                    album: texte(v, &["album_title", "album"]),
                    cover_url: texte(v, &["cover_path"]),
                    created_at: texte(v, &["created_at"]),
                },
                "album" => Entree {
                    item_type,
                    service_id: id(v),
                    title: texte(v, &["title"]),
                    artist: texte(v, &["artist_name", "artist"]),
                    album: None,
                    cover_url: texte(v, &["cover_path"]),
                    created_at: texte(v, &["created_at"]),
                },
                _ => Entree {
                    item_type,
                    service_id: id(v),
                    title: texte(v, &["name"]),
                    artist: None,
                    album: None,
                    cover_url: texte(v, &["image_path"]),
                    created_at: texte(v, &["created_at"]),
                },
            })
            .collect(),
    )
}

/// Reprend les favoris d'UN service dans le profil `profile_id`.
///
/// Les trois types sont lus indépendamment : l'échec de l'un n'emporte pas les
/// autres. Les playlists sont volontairement hors du lot — la table les
/// accepterait, mais aucun écran ni aucune règle ne les lit aujourd'hui, et on
/// n'écrit pas des lignes que personne ne consomme.
pub async fn reprendre_les_favoris_du_service(
    svc: &dyn StreamingService,
    profile_id: i64,
    backend: &Arc<dyn DbBackend>,
) -> RepriseFavoris {
    let service = svc.name().to_string();
    let repo = StreamingFavoritesRepo::with_backend(backend.clone());
    let mut total = RepriseFavoris::default();

    let pistes = if let Some(datees) = entrees_datees(svc, "tracks").await {
        datees
    } else {
        match svc.get_user_tracks().await {
            Ok(items) => items
                .into_iter()
                .map(|t| Entree {
                    item_type: "track",
                    service_id: t.id,
                    title: Some(t.title),
                    artist: Some(t.artist),
                    album: t.album,
                    cover_url: t.cover_path,
                    created_at: None,
                })
                .collect(),
            Err(e) => {
                warn!(service = %service, r#type = "tracks", erreur = %e, "reprise_favoris_service_illisible");
                total.echecs += 1;
                Vec::new()
            }
        }
    };
    total.cumuler(enregistrer(&repo, profile_id, &service, pistes));

    let albums = if let Some(datees) = entrees_datees(svc, "albums").await {
        datees
    } else {
        match svc.get_user_albums().await {
            Ok(items) => items
                .into_iter()
                .map(|a| Entree {
                    item_type: "album",
                    service_id: a.id,
                    // `title` porte le titre de l'album, `album` reste vide :
                    // c'est la forme qu'écrit déjà le cœur cliqué dans Tune, et la
                    // liste des favoris affiche `title`.
                    title: Some(a.title),
                    artist: Some(a.artist),
                    album: None,
                    cover_url: a.cover_path,
                    created_at: None,
                })
                .collect(),
            Err(e) => {
                warn!(service = %service, r#type = "albums", erreur = %e, "reprise_favoris_service_illisible");
                total.echecs += 1;
                Vec::new()
            }
        }
    };
    total.cumuler(enregistrer(&repo, profile_id, &service, albums));

    let artistes = if let Some(datees) = entrees_datees(svc, "artists").await {
        datees
    } else {
        match svc.get_user_artists().await {
            Ok(items) => items
                .into_iter()
                .map(|a| Entree {
                    item_type: "artist",
                    service_id: a.id,
                    title: Some(a.name),
                    artist: None,
                    album: None,
                    cover_url: a.image_path,
                    created_at: None,
                })
                .collect(),
            Err(e) => {
                warn!(service = %service, r#type = "artists", erreur = %e, "reprise_favoris_service_illisible");
                total.echecs += 1;
                Vec::new()
            }
        }
    };
    total.cumuler(enregistrer(&repo, profile_id, &service, artistes));

    if total.ajoutes > 0 || total.echecs > 0 {
        info!(
            service = %service,
            profile_id,
            lus = total.lus,
            ajoutes = total.ajoutes,
            deja_presents = total.deja_presents,
            echecs = total.echecs,
            "reprise_favoris_service"
        );
    }
    total
}

/// Écrit un lot dans la table, en distinguant l'ajout du déjà-connu.
///
/// Le `is_favorite` préalable ne sert QU'À compter : `add` est idempotent par
/// sa contrainte d'unicité, mais son `execute` ne dit pas combien de lignes il
/// a réellement posées selon le moteur. Sans cette lecture, un passage
/// annoncerait « 14 ajoutés » à chaque fois, y compris quand il n'a rien fait —
/// et le journal cesserait d'être une mesure.
fn enregistrer(
    repo: &StreamingFavoritesRepo,
    profile_id: i64,
    service: &str,
    entrees: Vec<Entree>,
) -> RepriseFavoris {
    let mut stats = RepriseFavoris::default();
    for entree in entrees {
        if entree.service_id.trim().is_empty() {
            // Sans identifiant de service, la ligne ne désigne rien et la
            // contrainte d'unicité la confondrait avec la suivante.
            continue;
        }
        stats.lus += 1;
        match repo.is_favorite(profile_id, entree.item_type, service, &entree.service_id) {
            Ok(true) => {
                stats.deja_presents += 1;
                // Déjà là, mais peut-être daté au « maintenant » d'une reprise
                // d'avant : on lui rend la date du service. Idempotent.
                if let Some(date) = entree.created_at.as_deref()
                    && let Ok(true) = repo.dater(
                        profile_id,
                        entree.item_type,
                        service,
                        &entree.service_id,
                        date,
                    )
                {
                    stats.redates += 1;
                }
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                warn!(service = %service, erreur = %e, "reprise_favoris_lecture_impossible");
                stats.echecs += 1;
                continue;
            }
        }
        match repo.add_date(
            profile_id,
            entree.item_type,
            service,
            &entree.service_id,
            entree.title.as_deref(),
            entree.artist.as_deref(),
            entree.album.as_deref(),
            entree.cover_url.as_deref(),
            entree.created_at.as_deref(),
        ) {
            Ok(()) => stats.ajoutes += 1,
            Err(e) => {
                warn!(service = %service, erreur = %e, "reprise_favoris_ecriture_impossible");
                stats.echecs += 1;
            }
        }
    }
    stats
}

#[cfg(test)]
mod tests_dates {
    use super::*;
    use crate::db::sqlite::SqliteDb;
    use crate::streaming::traits::{StreamAlbum, StreamArtist, StreamTrack};
    use async_trait::async_trait;
    use serde_json::json;

    /// Un service qui date ses favoris : deux pistes reprises d'un coup, à des
    /// dates DIFFÉRENTES chez lui.
    ///
    /// 🔴 #4552 — la charge n'est PLUS écrite à la main. Elle sort de la
    /// projection RÉELLE du connecteur (`QobuzService::favori_date`,
    /// `TidalService::favori_date`) appliquée à ce que l'API du service rend.
    /// L'ancien témoin fabriquait `{"id", "artist", "album"}` — la forme que
    /// le code attendait, pas celle que Qobuz rend (`source_id`,
    /// `artist_name`, `album_title`) — et restait vert pendant que la reprise
    /// répondait `lus: 0` sur le .18.
    struct ServiceDate {
        service: &'static str,
        /// Albums et artistes aussi, ou les pistes seules.
        trois_types: bool,
    }

    const QOBUZ: ServiceDate = ServiceDate {
        service: "qobuz",
        trois_types: false,
    };

    /// Ce que `/favorite/getUserFavorites` rend chez Qobuz (brut), passé
    /// par la projection datée du connecteur.
    fn favoris_qobuz(fav_type: &str, trois_types: bool) -> Vec<serde_json::Value> {
        use crate::streaming::qobuz::QobuzService;
        let bruts = match fav_type {
            "tracks" => vec![
                json!({"id": 1, "title": "Ancienne", "performer": {"name": "A"},
                       "album": {"title": "X"}, "duration": 100,
                       "favorited_at": 1_735_689_600}),
                json!({"id": 2, "title": "Recente", "performer": {"name": "B"},
                       "album": {"title": "Y"}, "duration": 200,
                       "favorited_at": 1_780_272_000}),
            ],
            "albums" if trois_types => vec![json!({"id": 999, "title": "Time Out",
                "artist": {"name": "Dave Brubeck", "id": 42}, "tracks_count": 7,
                "favorited_at": 1_700_000_000})],
            "artists" if trois_types => vec![json!({"id": 42, "name": "Dave Brubeck",
                "favorited_at": 1_700_000_000})],
            _ => Vec::new(),
        };
        bruts
            .iter()
            .map(|b| QobuzService::favori_date(b, fav_type))
            .collect()
    }

    /// Ce que `/v1/users/{id}/favorites/{type}` rend chez Tidal (enveloppes),
    /// passé par la projection datée du connecteur.
    fn favoris_tidal(fav_type: &str) -> Vec<serde_json::Value> {
        use crate::streaming::tidal::TidalService;
        let enveloppes = match fav_type {
            "tracks" => vec![json!({"created": "2019-04-18T09:53:31.000+0000",
                "item": {"id": 7, "title": "So What", "duration": 545,
                         "artist": {"name": "Miles Davis", "id": 42},
                         "album": {"id": 789, "title": "Kind of Blue"}}})],
            "albums" => vec![json!({"created": "2019-04-18T09:53:31.000+0000",
                "item": {"id": 789, "title": "Kind of Blue",
                         "artist": {"name": "Miles Davis", "id": 42},
                         "numberOfTracks": 5}})],
            _ => vec![json!({"created": "2019-04-18T09:53:31.000+0000",
                "item": {"id": 42, "name": "Miles Davis"}})],
        };
        enveloppes
            .iter()
            .filter_map(|e| TidalService::favori_date(e, fav_type))
            .collect()
    }
    #[async_trait]
    impl StreamingService for ServiceDate {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            self.service
        }
        fn enabled(&self) -> bool {
            true
        }
        fn set_enabled(&mut self, _enabled: bool) {}
        async fn authenticate(
            &mut self,
            _credentials: &serde_json::Value,
        ) -> Result<crate::streaming::traits::AuthStatus, crate::error::TuneError> {
            Ok(Default::default())
        }
        async fn auth_status(&self) -> crate::streaming::traits::AuthStatus {
            Default::default()
        }
        async fn logout(&mut self) -> Result<(), crate::error::TuneError> {
            Ok(())
        }
        async fn search(
            &self,
            _query: &str,
            _limit: usize,
        ) -> Result<crate::streaming::traits::SearchResults, crate::error::TuneError> {
            Err("hors sujet".into())
        }
        async fn get_track(&self, _id: &str) -> Result<StreamTrack, crate::error::TuneError> {
            Err("hors sujet".into())
        }
        async fn get_track_url(
            &self,
            _id: &str,
            _quality: Option<&str>,
        ) -> Result<crate::streaming::traits::StreamUrl, crate::error::TuneError> {
            Err("hors sujet".into())
        }
        async fn get_album(&self, _id: &str) -> Result<StreamAlbum, crate::error::TuneError> {
            Err("hors sujet".into())
        }
        async fn get_album_tracks(
            &self,
            _id: &str,
        ) -> Result<Vec<StreamTrack>, crate::error::TuneError> {
            Err("hors sujet".into())
        }
        async fn get_artist(&self, _id: &str) -> Result<StreamArtist, crate::error::TuneError> {
            Err("hors sujet".into())
        }
        async fn get_playlist(
            &self,
            _id: &str,
        ) -> Result<crate::streaming::traits::StreamPlaylist, crate::error::TuneError> {
            Err("hors sujet".into())
        }
        async fn get_playlist_tracks(
            &self,
            _id: &str,
        ) -> Result<Vec<StreamTrack>, crate::error::TuneError> {
            Err("hors sujet".into())
        }
        async fn get_user_playlists(
            &self,
        ) -> Result<Vec<crate::streaming::traits::StreamPlaylist>, crate::error::TuneError>
        {
            Ok(Vec::new())
        }
        async fn get_user_favorites_dated(
            &self,
            fav_type: &str,
        ) -> Result<Option<Vec<serde_json::Value>>, crate::error::TuneError> {
            Ok(Some(match self.service {
                "tidal" => favoris_tidal(fav_type),
                _ => favoris_qobuz(fav_type, self.trois_types),
            }))
        }
        async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, crate::error::TuneError> {
            unreachable!("le chemin daté doit primer");
        }
        async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, crate::error::TuneError> {
            Ok(Vec::new())
        }
        async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, crate::error::TuneError> {
            Ok(Vec::new())
        }
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn dates(backend: &Arc<dyn DbBackend>) -> Vec<(String, String)> {
        backend
            .query_many(
                "SELECT service_id, created_at FROM streaming_favorites ORDER BY service_id",
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                )
            })
            .collect()
    }

    /// La reprise écrit la date DU SERVICE, pas l'instant de la reprise —
    /// et redate ce qu'une reprise d'avant avait daté au même instant.
    #[tokio::test]
    async fn la_reprise_ecrit_la_date_du_service_et_redate_l_existant() {
        let backend = base();
        let repo = StreamingFavoritesRepo::with_backend(backend.clone());
        // Une reprise « d'avant » : la piste 1 existe déjà, datée au moment
        // de la reprise (ici : maintenant).
        repo.add(
            1,
            "track",
            "qobuz",
            "1",
            Some("Ancienne"),
            Some("A"),
            Some("X"),
            None,
        )
        .unwrap();

        let stats = reprendre_les_favoris_du_service(&QOBUZ, 1, &backend).await;
        assert_eq!(
            (stats.lus, stats.ajoutes, stats.deja_presents, stats.redates),
            (2, 1, 1, 1),
            "{stats:?}"
        );
        assert_eq!(
            dates(&backend),
            vec![
                ("1".to_string(), "2025-01-01T00:00:00Z".to_string()),
                ("2".to_string(), "2026-06-01T00:00:00Z".to_string()),
            ]
        );
        // Seconde reprise : rien ne bouge, rien n'est redaté.
        let stats = reprendre_les_favoris_du_service(&QOBUZ, 1, &backend).await;
        assert_eq!((stats.ajoutes, stats.redates), (0, 0), "{stats:?}");
    }

    /// Ce que la table a retenu : `(type, id, titre, artiste, album)`.
    fn lignes(backend: &Arc<dyn DbBackend>) -> Vec<[String; 5]> {
        backend
            .query_many(
                "SELECT item_type, service_id, title, artist, album FROM streaming_favorites \
                 ORDER BY item_type, service_id",
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| {
                std::array::from_fn(|i| r.get(i).and_then(|v| v.as_string()).unwrap_or_default())
            })
            .collect()
    }

    /// 🔴 #4552 — la reprise LIT ce que Qobuz rend : les trois types, leurs
    /// libellés, leur date. Rouge avant : `lus: 0`, table vide — les favoris
    /// posés dans l'application Qobuz n'arrivaient plus dans Tune.
    #[tokio::test]
    async fn la_reprise_lit_la_forme_reelle_de_qobuz() {
        let backend = base();
        let qobuz = ServiceDate {
            service: "qobuz",
            trois_types: true,
        };
        let stats = reprendre_les_favoris_du_service(&qobuz, 1, &backend).await;
        assert_eq!((stats.lus, stats.ajoutes), (4, 4), "{stats:?}");
        let s = |x: &str| x.to_string();
        assert_eq!(
            lignes(&backend),
            vec![
                [
                    s("album"),
                    s("999"),
                    s("Time Out"),
                    s("Dave Brubeck"),
                    s("")
                ],
                [s("artist"), s("42"), s("Dave Brubeck"), s(""), s("")],
                [s("track"), s("1"), s("Ancienne"), s("A"), s("X")],
                [s("track"), s("2"), s("Recente"), s("B"), s("Y")],
            ]
        );
        assert!(
            dates(&backend).contains(&(s("1"), s("2025-01-01T00:00:00Z"))),
            "la date de Qobuz : {:?}",
            dates(&backend)
        );
    }

    /// Même lecture pour Tidal, dont la projection sort les mêmes clés.
    #[tokio::test]
    async fn la_reprise_lit_la_forme_reelle_de_tidal() {
        let backend = base();
        let tidal = ServiceDate {
            service: "tidal",
            trois_types: true,
        };
        let stats = reprendre_les_favoris_du_service(&tidal, 1, &backend).await;
        assert_eq!((stats.lus, stats.ajoutes), (3, 3), "{stats:?}");
        let s = |x: &str| x.to_string();
        assert_eq!(
            lignes(&backend),
            vec![
                [
                    s("album"),
                    s("789"),
                    s("Kind of Blue"),
                    s("Miles Davis"),
                    s("")
                ],
                [s("artist"), s("42"), s("Miles Davis"), s(""), s("")],
                [
                    s("track"),
                    s("7"),
                    s("So What"),
                    s("Miles Davis"),
                    s("Kind of Blue")
                ],
            ]
        );
    }
}
