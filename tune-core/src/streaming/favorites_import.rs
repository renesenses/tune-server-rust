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
    let texte =
        |v: &serde_json::Value, cle: &str| v.get(cle).and_then(|x| x.as_str()).map(str::to_string);
    let id = |v: &serde_json::Value| match v.get("id") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    Some(
        items
            .iter()
            .map(|v| match item_type {
                "track" => Entree {
                    item_type,
                    service_id: id(v),
                    title: texte(v, "title"),
                    artist: texte(v, "artist"),
                    album: texte(v, "album"),
                    cover_url: texte(v, "cover_path"),
                    created_at: texte(v, "created_at"),
                },
                "album" => Entree {
                    item_type,
                    service_id: id(v),
                    title: texte(v, "title"),
                    artist: texte(v, "artist"),
                    album: None,
                    cover_url: texte(v, "cover_path"),
                    created_at: texte(v, "created_at"),
                },
                _ => Entree {
                    item_type,
                    service_id: id(v),
                    title: texte(v, "name"),
                    artist: None,
                    album: None,
                    cover_url: texte(v, "image_path"),
                    created_at: texte(v, "created_at"),
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

    /// Un service qui date ses favoris comme Qobuz : deux pistes reprises
    /// d'un coup, à des dates DIFFÉRENTES chez lui.
    struct ServiceDate;
    #[async_trait]
    impl StreamingService for ServiceDate {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            "qobuz"
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
            if fav_type != "tracks" {
                return Ok(Some(Vec::new()));
            }
            Ok(Some(vec![
                json!({"id": 1, "title": "Ancienne", "artist": "A", "album": "X",
                       "cover_path": null, "created_at": "2025-01-01T00:00:00Z"}),
                json!({"id": 2, "title": "Recente", "artist": "B", "album": "Y",
                       "cover_path": null, "created_at": "2026-06-01T00:00:00Z"}),
            ]))
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

        let stats = reprendre_les_favoris_du_service(&ServiceDate, 1, &backend).await;
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
        let stats = reprendre_les_favoris_du_service(&ServiceDate, 1, &backend).await;
        assert_eq!((stats.ajoutes, stats.redates), (0, 0), "{stats:?}");
    }
}
