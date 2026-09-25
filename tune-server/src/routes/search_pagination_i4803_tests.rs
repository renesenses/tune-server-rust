//! #4803 — la recherche fédérée pagine ses services.
//!
//! Témoins SANS réseau : deux services simulés.
//!
//! * `pagineur` redéfinit `search_page` sur un catalogue fixe de sept pistes
//!   « Coltrane 0 » à « Coltrane 6 » — un service qui pagine, comme Qobuz ;
//! * `sanspage` garde le `search_page` par défaut — un service qui ne pagine
//!   pas, comme Tidal, Deezer, YouTube ou Bandcamp aujourd'hui.
//!
//! Les deux COMPTENT leurs appels : le coût d'une page globale se lit là.
//!
//! Les requêtes passent par la chaîne de requête (`Query::try_from_uri`), pas
//! par un littéral de `SearchParams` : c'est l'URL qu'un client envoie, et
//! c'est ce qui laisse ces témoins COMPILER sur la base, où les paramètres
//! neufs sont simplement ignorés — leur rouge y est donc un rouge de
//! comportement.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::{Query, State};
use serde_json::Value;
use tune_core::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchPage, SearchResults, SearchTotals, StreamAlbum, StreamArtist, StreamPlaylist,
    StreamTrack, StreamUrl, StreamingService,
};

use super::federated_search;

const CATALOGUE: usize = 7;

fn piste(i: usize, prefixe: &str) -> StreamTrack {
    StreamTrack {
        id: format!("{prefixe}{i}"),
        title: format!("Coltrane {i}"),
        artist: "John Coltrane".to_string(),
        album: None,
        album_id: None,
        duration_ms: 300_000,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

fn resultats(tracks: Vec<StreamTrack>) -> SearchResults {
    SearchResults {
        tracks,
        albums: vec![],
        artists: vec![],
        playlists: vec![],
    }
}

#[derive(Default)]
struct Compteurs {
    search: AtomicUsize,
    search_page: AtomicUsize,
}

struct Simule {
    nom: &'static str,
    /// `true` : redéfinit `search_page` (pagine) ; `false` : garde le défaut.
    pagine: bool,
    /// Pistes que le service possède.
    catalogue: usize,
    appels: Arc<Compteurs>,
}

#[async_trait::async_trait]
impl StreamingService for Simule {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, limit: usize) -> Result<SearchResults, TuneError> {
        self.appels.search.fetch_add(1, Ordering::SeqCst);
        let n = limit.min(self.catalogue);
        Ok(resultats(
            (0..n)
                .map(|i| piste(i, &format!("{}-", self.nom)))
                .collect(),
        ))
    }
    async fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchPage, TuneError> {
        if !self.pagine {
            // Le défaut du trait, recopié : un service qui ne pagine pas.
            if offset > 0 {
                self.appels.search_page.fetch_add(1, Ordering::SeqCst);
                return Ok(SearchPage::au_dela(offset));
            }
            // `search` compte lui-même cet appel-là : un seul appel réseau.
            let borne = tune_core::streaming::traits::limite_sans_pagination(limit);
            return Ok(SearchPage::page_unique_bornee(
                self.search(query, borne).await?,
                borne,
            ));
        }
        self.appels.search_page.fetch_add(1, Ordering::SeqCst);
        let fin = (offset + limit).min(self.catalogue);
        let tracks: Vec<StreamTrack> = (offset.min(fin)..fin)
            .map(|i| piste(i, &format!("{}-", self.nom)))
            .collect();
        let rendus = tracks.len();
        Ok(SearchPage {
            results: resultats(tracks),
            offset,
            totals: SearchTotals {
                tracks: self.catalogue,
                ..Default::default()
            },
            has_more: offset + rendus < self.catalogue,
            truncated: false,
        })
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
}

struct Banc {
    state: crate::state::AppState,
    pagineur: Arc<Compteurs>,
    sanspage: Arc<Compteurs>,
}

async fn banc() -> Banc {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let pagineur = Arc::new(Compteurs::default());
    let sanspage = Arc::new(Compteurs::default());
    {
        let mut registre = state.services.lock().await;
        registre.register(Box::new(Simule {
            nom: "pagineur",
            pagine: true,
            catalogue: CATALOGUE,
            appels: pagineur.clone(),
        }));
        registre.register(Box::new(Simule {
            nom: "sanspage",
            pagine: false,
            catalogue: 2,
            appels: sanspage.clone(),
        }));
    }
    Banc {
        state,
        pagineur,
        sanspage,
    }
}

async fn chercher(state: &crate::state::AppState, chaine: &str) -> Value {
    let uri: axum::http::Uri = format!("/?{chaine}").parse().expect("URI valide");
    let Query(params) = Query::try_from_uri(&uri).expect("paramètres lisibles");
    federated_search(
        State(state.clone()),
        crate::routes::active_profile::ActiveProfile(1),
        Query(params),
    )
    .await
    .0
}

fn ids(bloc: &Value) -> Vec<String> {
    bloc["tracks"]
        .as_array()
        .unwrap_or_else(|| panic!("pas de tableau de pistes : {bloc}"))
        .iter()
        .map(|p| p["source_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn attendus(de: usize, a: usize) -> Vec<String> {
    (de..a).map(|i| format!("pagineur-{i}")).collect()
}

/// Un décalage rend la page SUIVANTE d'un service — le « Voir plus » d'un bloc.
#[tokio::test]
async fn un_decalage_rend_la_page_suivante_d_un_service() {
    let b = banc().await;
    let r = chercher(
        &b.state,
        "q=coltrane&limit=3&sources=pagineur&service_offsets=pagineur:3",
    )
    .await;
    let bloc = &r["services"]["pagineur"];
    assert_eq!(
        ids(bloc),
        attendus(3, 6),
        "la page à partir du rang 3, pas la première : {bloc}"
    );
    assert_eq!(bloc["offset"], 3, "{bloc}");
    assert_eq!(bloc["limit"], 3, "{bloc}");
    assert_eq!(bloc["total"]["tracks"], CATALOGUE, "{bloc}");
    assert_eq!(bloc["has_more"], true, "{bloc}");
}

/// Une limite PAR SERVICE : `service_limits` l'emporte sur `limit`.
#[tokio::test]
async fn une_limite_par_service_l_emporte_sur_limit() {
    let b = banc().await;
    let r = chercher(
        &b.state,
        "q=coltrane&limit=3&sources=pagineur&service_limits=pagineur:5",
    )
    .await;
    let bloc = &r["services"]["pagineur"];
    assert_eq!(ids(bloc), attendus(0, 5), "{bloc}");
    assert_eq!(bloc["limit"], 5, "{bloc}");
}

/// `has_more` est FAUX à la dernière page, et le curseur global s'éteint.
#[tokio::test]
async fn has_more_est_faux_a_la_derniere_page() {
    let b = banc().await;
    let r = chercher(
        &b.state,
        "q=coltrane&limit=3&sources=pagineur&service_offsets=pagineur:6",
    )
    .await;
    let bloc = &r["services"]["pagineur"];
    assert_eq!(ids(bloc), attendus(6, 7), "{bloc}");
    assert_eq!(bloc["has_more"].as_bool(), Some(false), "{bloc}");
    assert_eq!(r["has_more"].as_bool(), Some(false), "{r}");
    assert!(
        r.get("next_cursor").is_some_and(Value::is_null),
        "dernière page : `next_cursor` présent et nul — {r}"
    );
}

/// Le curseur global rend la page suivante du résultat FUSIONNÉ, sans doublon
/// ni trou, et n'interroge plus un service épuisé.
#[tokio::test]
async fn le_curseur_global_rend_la_suite_fusionnee_sans_doublon_ni_trou() {
    let b = banc().await;
    let mut vus_pagineur: Vec<String> = Vec::new();
    let mut vus_sanspage: Vec<String> = Vec::new();
    let mut chaine = "q=coltrane&limit=3&sources=pagineur,sanspage&paged=true".to_string();
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= 10, "le curseur ne s'éteint jamais");
        let r = chercher(&b.state, &chaine).await;
        if let Some(bloc) = r["services"].get("pagineur") {
            vus_pagineur.extend(ids(bloc));
        }
        if let Some(bloc) = r["services"].get("sanspage") {
            vus_sanspage.extend(ids(bloc));
        }
        match r.get("next_cursor") {
            Some(Value::String(c)) => {
                chaine = format!(
                    "q=coltrane&limit=3&sources=pagineur,sanspage&cursor={}",
                    urlencoding::encode(c)
                );
            }
            Some(Value::Null) => {
                assert_eq!(r["has_more"], false, "{r}");
                break;
            }
            _ => panic!("page {pages} : pas de `next_cursor` — {r}"),
        }
    }
    assert_eq!(
        vus_pagineur,
        attendus(0, CATALOGUE),
        "le service paginé, parcouru page après page, doit rendre son catalogue \
         entier, dans l'ordre, sans doublon ni trou"
    );
    assert_eq!(
        vus_sanspage,
        vec!["sanspage-0", "sanspage-1"],
        "le service épuisé dès la première page n'y revient pas"
    );
    assert_eq!(pages, 3, "7 pistes par pages de 3 : trois pages");

    // Le coût : UNE page de service par page globale, et seulement pour les
    // sources qui ont une suite.
    assert_eq!(b.pagineur.search_page.load(Ordering::SeqCst), 3);
    assert_eq!(b.pagineur.search.load(Ordering::SeqCst), 0);
    assert_eq!(
        b.sanspage.search.load(Ordering::SeqCst) + b.sanspage.search_page.load(Ordering::SeqCst),
        1,
        "le service sans suite n'est interrogé qu'à la première page"
    );
}

/// La bibliothèque locale voyage dans le même curseur que les services.
#[tokio::test]
async fn le_curseur_global_porte_aussi_la_bibliotheque_locale() {
    let b = banc().await;
    let db = &b.state.backend;
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'Coltrane')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES \
         (1, 'Ballads', 1), (2, 'Giant Steps', 1), (3, 'Blue Train', 1), \
         (4, 'Olé', 1), (5, 'Crescent', 1)",
        &[],
    )
    .unwrap();

    let mut albums: Vec<i64> = Vec::new();
    let mut chaine = "q=coltrane&limit=2&sources=local&paged=true".to_string();
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= 10, "le curseur ne s'éteint jamais");
        let r = chercher(&b.state, &chaine).await;
        albums.extend(
            r["local"]["albums"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["id"].as_i64().unwrap()),
        );
        match r.get("next_cursor") {
            Some(Value::String(c)) => {
                chaine = format!(
                    "q=coltrane&limit=2&sources=local&cursor={}",
                    urlencoding::encode(c)
                );
            }
            Some(Value::Null) => break,
            _ => panic!("page {pages} : pas de `next_cursor` — {r}"),
        }
    }
    let mut tries = albums.clone();
    tries.sort_unstable();
    tries.dedup();
    assert_eq!(
        tries,
        vec![1, 2, 3, 4, 5],
        "ni doublon ni trou : {albums:?}"
    );
    assert_eq!(albums.len(), 5, "aucun album rendu deux fois : {albums:?}");
    assert_eq!(pages, 3);
}

/// Sans aucun paramètre de pagination, la réponse est celle d'AVANT #4803 :
/// `search` (pas `search_page`) avec `limit` tel quel, des blocs de service
/// réduits à leurs quatre clés, et aucune clé neuve en tête.
///
/// Garde de NON-RÉGRESSION : elle est verte sur la base par construction, et
/// doit le rester.
#[tokio::test]
async fn sans_parametre_la_reponse_est_celle_d_avant() {
    let b = banc().await;
    let r = chercher(&b.state, "q=coltrane&limit=3&sources=pagineur,sanspage").await;

    let cles_de_tete: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(cles_de_tete, vec!["local", "radios", "services"], "{r}");
    for nom in ["pagineur", "sanspage"] {
        let bloc = &r["services"][nom];
        let cles: std::collections::BTreeSet<&str> = bloc
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            cles,
            ["albums", "artists", "playlists", "tracks"]
                .into_iter()
                .collect(),
            "{nom} : {bloc}"
        );
    }
    assert_eq!(ids(&r["services"]["pagineur"]), attendus(0, 3));
    assert_eq!(b.pagineur.search.load(Ordering::SeqCst), 1);
    assert_eq!(b.pagineur.search_page.load(Ordering::SeqCst), 0);
    assert_eq!(b.sanspage.search_page.load(Ordering::SeqCst), 0);
}
