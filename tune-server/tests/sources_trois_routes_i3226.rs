//! `sources` sur les TROIS routes qui melangeaient la bibliotheque et les
//! services sans qu'aucun parametre ne dise ce qu'on voulait.
//!
//! - `GET /api/v1/home/other-versions`
//! - `GET /api/v1/home/artist-releases`
//! - `GET /api/v1/library/tracks/{id}/versions`
//!
//! Le contrat est celui pose par #3226 sur `GET /search` (PR #3265), repris
//! mot pour mot : voir `tune-server/src/routes/filtre_sources.rs`.
//!
//! # Ce que ces essais mesurent
//!
//! **Le CORPS JSON de la reponse, et rien d'autre.** Aucune assertion ne
//! rappelle la condition de filtrage ecrite dans le code de production : un
//! essai qui recopie la condition ne la garde pas, il la duplique — il
//! resterait vert si la condition et sa copie devenaient fausses ensemble.
//! Ici on pose une URL et on regarde ce qui sort.
//!
//! # Le socle, et pourquoi il est reverifie a CHAQUE essai
//!
//! Sans correspondance des DEUX cotes, « vide » ne veut rien dire : il peut
//! signifier « correctement filtre » comme « il n'y avait rien a trouver »,
//! et l'epreuve serait verte contre rien. Le banc pose donc, pour le meme
//! morceau :
//!
//! - **du local** : le morceau de reference sur DEUX albums possedes, plus une
//!   ligne d'historique d'ecoute ;
//! - **deux services AUTHENTIFIES** — `qobuz` et `tidal` — qui repondent tous
//!   deux a ce morceau et publient tous deux une nouveaute de son artiste.
//!
//! Deux services, et non un : avec un seul, `sources=qobuz` rendrait la meme
//! chose que « Tous » sur la moitie streaming, et l'essai ne saurait pas
//! distinguer « le filtre a garde Qobuz » de « le filtre n'a rien fait ». Le
//! defaut de #3226 est precisement ne de cette confusion — Reivax66 n'avait
//! qu'un seul service authentifie.
//!
//! [`socle`] est appelee au debut de chaque essai et echoue bruyamment si
//! l'un des deux cotes est vide sans qu'on l'ait demande.
//!
//! # ⚠️ Les services factices s'appellent `qobuz` et `tidal`
//!
//! Ce n'est pas un ornement. `routes::versions::SERVICES_VERSIONS` et
//! `routes::artist_releases::SERVICES` sont des listes de noms ECRITES EN
//! DUR : un service nomme « banc-essai » ne serait jamais interroge par ces
//! routes, et tous les essais seraient verts contre un registre muet.
//! `ServiceRegistry::register` remplace par nom, donc ces doublures prennent
//! la place des vrais services — qui, eux, ne sont pas authentifies en essai.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::TuneError;
use tune_core::db::backend::ToSqlValue;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

/// Le morceau de reference. Titre et artiste sont INVENTES, et c'est
/// indispensable, pas un ornement.
///
/// ⚠️ Sous la fonctionnalite `bandcamp` — celle du job `Test` de la CI,
/// `ci.yml:237` — `versions_streaming` interroge Bandcamp par un VRAI appel
/// reseau, hors du registre et sans authentification. Avec un titre reel
/// (« Running Up That Hill » a servi au premier jet), la reponse du banc
/// dependait de ce que Bandcamp servait ce jour-la : neuf entrees imprevues
/// s'invitaient dans `streaming`, et l'essai devenait un pari sur le reseau.
///
/// Un titre qui n'existe nulle part rend la moitie Bandcamp vide de facon
/// DETERMINISTE : que l'appel reponde ou echoue, `classer_version` ecarte tout
/// ce qui ne porte pas ce titre. Le banc ne mesure donc plus que ses propres
/// doublures.
///
/// Le revers est dit franchement : ce banc ne peut PAS prouver que
/// `sources=qobuz` ecarte Bandcamp, puisque Bandcamp n'y rend jamais rien.
/// C'est la garde de site
/// [`le_filtre_de_sources_couvre_la_branche_bandcamp`] (dans
/// `src/routes/versions.rs`) qui couvre cette branche-la.
const TITRE: &str = "Ascenseur Vertige Qxzv";
const ARTISTE: &str = "Silene Faubourg";
/// L'album ECOUTE. Une version sur CE meme album serait « le meme
/// enregistrement », donc ecartee : les autres versions sont ailleurs.
const ALBUM_ECOUTE: &str = "Premier Etage";
/// L'autre album POSSEDE, celui qui porte l'autre version locale.
const ALBUM_POSSEDE: &str = "Anthologie Qxzv";

// ── Une doublure de service, authentifiee et qui repond ───────────────────

/// Un service authentifie qui rend, pour le morceau de reference, UNE autre
/// version, et publie UNE nouveaute de l'artiste.
///
/// Son nom est porte en champ pour qu'on puisse en poser deux : c'est ce qui
/// permet a `sources=qobuz` de se distinguer de « Tous » sur la moitie
/// streaming.
struct Doublure(&'static str);

#[async_trait::async_trait]
impl StreamingService for Doublure {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.0
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}

    async fn authenticate(&mut self, _credentials: &Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            username: Some("banc".into()),
            ..Default::default()
        }
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }

    /// Une piste dont le titre est celui de reference SUIVI d'un suffixe
    /// d'edition, par le MEME artiste, sur un AUTRE album : c'est la
    /// definition stricte d'« autre version », donc elle entre.
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: vec![StreamTrack {
                id: format!("{}-piste-1", self.0),
                title: format!("{TITRE} (Remaster {})", self.0),
                artist: ARTISTE.into(),
                album: Some(format!("Remasters chez {}", self.0)),
                album_id: Some(format!("{}-album-1", self.0)),
                duration_ms: 300_000,
                cover_path: None,
                track_number: Some(1),
                disc_number: Some(1),
                explicit: false,
                quality: None,
                isrc: None,
                composer: None,
                artist_id: None,
            }],
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }

    /// Une nouveaute de l'artiste possede : c'est ce qui alimente
    /// `/home/artist-releases`. Sans elle, cette route rendrait `[]` partout
    /// et ses essais seraient verts contre rien.
    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![StreamAlbum {
            id: format!("{}-nouveaute-1", self.0),
            title: format!("Inedits chez {}", self.0),
            artist: ARTISTE.into(),
            artist_id: None,
            cover_path: None,
            year: Some(2026),
            track_count: 9,
            quality: None,
        }])
    }

    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_track_url(
        &self,
        _track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_album_tracks(&self, _album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

// ── Le banc ───────────────────────────────────────────────────────────────

/// La bibliotheque, l'historique d'ecoute, et les deux services.
///
/// Rend l'identifiant de la piste ECOUTEE — celle que
/// `/library/tracks/{id}/versions` prend en entree.
async fn banc() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let b = &state.backend;

    b.execute(
        "INSERT INTO artists (name) VALUES (?1)",
        &[&ARTISTE as &dyn ToSqlValue],
    )
    .unwrap();
    let artiste = b.last_insert_rowid();

    b.execute(
        "INSERT INTO albums (title, artist_id, year) VALUES (?1, ?2, 1985)",
        &[
            &ALBUM_ECOUTE as &dyn ToSqlValue,
            &artiste as &dyn ToSqlValue,
        ],
    )
    .unwrap();
    let ecoute = b.last_insert_rowid();

    b.execute(
        "INSERT INTO albums (title, artist_id, year) VALUES (?1, ?2, 1986)",
        &[
            &ALBUM_POSSEDE as &dyn ToSqlValue,
            &artiste as &dyn ToSqlValue,
        ],
    )
    .unwrap();
    let possede = b.last_insert_rowid();

    // La piste de reference, sur l'album ecoute.
    b.execute(
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         VALUES (?1, ?2, ?3, 300000, '/ecoute.flac')",
        &[
            &TITRE as &dyn ToSqlValue,
            &ecoute as &dyn ToSqlValue,
            &artiste as &dyn ToSqlValue,
        ],
    )
    .unwrap();
    let piste = b.last_insert_rowid();

    // L'AUTRE version possedee : meme titre, meme artiste, autre album.
    b.execute(
        "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
         VALUES (?1, ?2, ?3, 298000, '/possede.flac')",
        &[
            &TITRE as &dyn ToSqlValue,
            &possede as &dyn ToSqlValue,
            &artiste as &dyn ToSqlValue,
        ],
    )
    .unwrap();

    // Le vivier de `/home/other-versions` : une ecoute recente.
    b.execute(
        "INSERT INTO listen_history (track_id, title, artist_name, album_title, listened_at) \
         VALUES (?1, ?2, ?3, ?4, '2026-09-03T09:00:00Z')",
        &[
            &piste as &dyn ToSqlValue,
            &TITRE as &dyn ToSqlValue,
            &ARTISTE as &dyn ToSqlValue,
            &ALBUM_ECOUTE as &dyn ToSqlValue,
        ],
    )
    .unwrap();

    {
        let mut registre = state.services.lock().await;
        registre.register(Box::new(Doublure("qobuz")));
        registre.register(Box::new(Doublure("tidal")));
    }

    (state, piste)
}

/// `GET <chemin>` sur le vrai routeur, avec `sources` tel qu'il est passe ici.
/// `None` veut dire « pas de parametre du tout », jamais « parametre vide ».
async fn obtenir(state: &AppState, chemin: &str, sources: Option<&str>) -> Value {
    let app = tune_server::routes::router(state.clone());
    let mut url = chemin.to_string();
    if let Some(s) = sources {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&format!("sources={s}"));
    }
    let reponse = app
        .oneshot(Request::get(&url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    assert_eq!(statut, StatusCode::OK, "GET {url} — corps : {corps}");
    corps
}

/// Le tableau a ce chemin, ou un echec qui montre le corps entier.
fn tableau<'a>(corps: &'a Value, chemin: &[&str]) -> &'a Vec<Value> {
    let mut cur = corps;
    for cle in chemin {
        cur = cur
            .get(*cle)
            .unwrap_or_else(|| panic!("clé « {cle} » absente de la réponse : {corps}"));
    }
    cur.as_array()
        .unwrap_or_else(|| panic!("{chemin:?} n'est pas un tableau : {cur}"))
}

/// Les noms de service presents dans une liste d'entrees streaming, tries.
fn services_cites(entrees: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = entrees
        .iter()
        .map(|e| {
            e["service"]
                .as_str()
                .unwrap_or("<sans service>")
                .to_string()
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

// ── Le socle, reverifie au debut de chaque essai ──────────────────────────

/// Les DEUX cotes repondent-ils, sans `sources` ?
///
/// C'est la contre-epreuve de tous les « vide » qui suivent. Si elle passait
/// alors qu'un cote est muet, chaque assertion « doit etre vide » serait verte
/// contre rien.
async fn socle(state: &AppState, piste: i64) {
    let v = obtenir(
        state,
        &format!("/api/v1/library/tracks/{piste}/versions"),
        None,
    )
    .await;
    assert_eq!(
        tableau(&v, &["versions"]).len(),
        1,
        "socle : la bibliotheque doit porter UNE autre version locale : {v}"
    );
    assert_eq!(
        services_cites(tableau(&v, &["streaming"])),
        vec!["qobuz".to_string(), "tidal".to_string()],
        "socle : les DEUX services doivent repondre, sans quoi « vide » ne \
         prouverait rien : {v}"
    );

    let a = obtenir(state, "/api/v1/home/other-versions", None).await;
    let groupes = a
        .as_array()
        .unwrap_or_else(|| panic!("liste attendue : {a}"));
    assert_eq!(
        groupes.len(),
        1,
        "socle : un groupe d'accueil attendu : {a}"
    );
    assert_eq!(
        tableau(&groupes[0], &["versions"]).len(),
        1,
        "socle : le groupe doit porter sa version LOCALE : {a}"
    );
    assert_eq!(
        services_cites(tableau(&groupes[0], &["streaming"])),
        vec!["qobuz".to_string(), "tidal".to_string()],
        "socle : le groupe doit porter les DEUX services : {a}"
    );

    let n = obtenir(state, "/api/v1/home/artist-releases", None).await;
    let groupes = n
        .as_array()
        .unwrap_or_else(|| panic!("liste attendue : {n}"));
    assert_eq!(
        groupes.len(),
        1,
        "socle : un groupe de nouveautes attendu : {n}"
    );
    assert_eq!(
        services_cites(tableau(&groupes[0], &["releases"])),
        vec!["qobuz".to_string(), "tidal".to_string()],
        "socle : les deux services doivent publier une nouveaute : {n}"
    );
}

// ═══ 1. `/library/tracks/{id}/versions` ═══════════════════════════════════

#[tokio::test]
async fn versions_piste_un_service_seul_ne_rend_plus_la_bibliotheque() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let v = obtenir(
        &state,
        &format!("/api/v1/library/tracks/{piste}/versions"),
        Some("qobuz"),
    )
    .await;

    assert_eq!(
        tableau(&v, &["versions"]).len(),
        0,
        "« qobuz » demandé seul : les versions LOCALES doivent être vides — la \
         bibliothèque en porte pourtant une. Corps : {v}"
    );
    assert_eq!(
        services_cites(tableau(&v, &["streaming"])),
        vec!["qobuz".to_string()],
        "et le service demandé, lui, doit être là — SEUL : {v}"
    );
}

#[tokio::test]
async fn versions_piste_local_seul_ne_rend_aucun_service() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let v = obtenir(
        &state,
        &format!("/api/v1/library/tracks/{piste}/versions"),
        Some("local"),
    )
    .await;

    assert_eq!(
        tableau(&v, &["versions"]).len(),
        1,
        "« local » demandé seul : la version possédée doit être servie : {v}"
    );
    assert_eq!(
        tableau(&v, &["streaming"]).len(),
        0,
        "et aucun service ne doit répondre : {v}"
    );
}

/// LE TEMOIN. `sources` absent est le seul cas qui marchait deja partout ; il
/// doit rendre EXACTEMENT ce que la route rendait avant. Il reste vert sous le
/// sabotage — c'est ce qui prouve que les essais ci-dessus mesurent le filtre
/// et non le hasard du banc.
#[tokio::test]
async fn versions_piste_le_temoin_sans_sources_rend_les_deux_moities() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let v = obtenir(
        &state,
        &format!("/api/v1/library/tracks/{piste}/versions"),
        None,
    )
    .await;

    // La FORME entière, pas seulement les deux tableaux : un champ perdu en
    // chemin serait une régression aussi sûrement qu'une liste vidée.
    assert_eq!(v["track_id"].as_i64(), Some(piste), "{v}");
    assert_eq!(v["title"].as_str(), Some(TITRE), "{v}");
    assert_eq!(v["artist_name"].as_str(), Some(ARTISTE), "{v}");
    assert_eq!(v["played_album"].as_str(), Some(ALBUM_ECOUTE), "{v}");
    assert_eq!(tableau(&v, &["versions"]).len(), 1, "{v}");
    assert_eq!(tableau(&v, &["streaming"]).len(), 2, "{v}");
    assert_eq!(
        tableau(&v, &["versions"])[0]["album_title"].as_str(),
        Some(ALBUM_POSSEDE),
        "{v}"
    );
}

/// `streaming=false` est un parametre PUBLIE, que `tune-web-client` envoie a
/// chaque appel (`src/lib/api.ts`, `getTrackVersions`). Il continue de dire
/// exactement ce qu'il disait : coupe les services, laisse le local.
#[tokio::test]
async fn versions_piste_le_vieux_parametre_streaming_faux_dit_toujours_la_meme_chose() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let v = obtenir(
        &state,
        &format!("/api/v1/library/tracks/{piste}/versions?streaming=false"),
        None,
    )
    .await;

    assert_eq!(
        tableau(&v, &["versions"]).len(),
        1,
        "`streaming=false` n'a jamais touché au local : {v}"
    );
    assert_eq!(
        tableau(&v, &["streaming"]).len(),
        0,
        "`streaming=false` coupe les services : {v}"
    );
}

/// Les deux parametres cohabitent : `streaming=false` est un VETO sur la
/// moitie streaming, il ne ressuscite jamais le local qu'un `sources` de
/// service a ecarte.
#[tokio::test]
async fn versions_piste_les_deux_parametres_ensemble_vident_les_deux_moities() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let v = obtenir(
        &state,
        &format!("/api/v1/library/tracks/{piste}/versions?streaming=false"),
        Some("qobuz"),
    )
    .await;

    assert_eq!(tableau(&v, &["versions"]).len(), 0, "{v}");
    assert_eq!(tableau(&v, &["streaming"]).len(), 0, "{v}");
}

/// La forme lue par le client reste ENTIERE quand une moitie est ecartee.
/// `LibraryView.svelte` garde ses deux acces (`g?.versions ?? []`), mais
/// `HomeView.svelte:1035` fait `{#each groupe.versions as v}` sans garde :
/// une cle absente planterait l'ecran la ou une cle vide passe.
#[tokio::test]
async fn versions_piste_les_deux_cles_restent_presentes_meme_vides() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    for source in ["qobuz", "local", "source-inconnue", ""] {
        let v = obtenir(
            &state,
            &format!("/api/v1/library/tracks/{piste}/versions"),
            Some(source),
        )
        .await;
        assert!(
            v.get("versions").is_some_and(Value::is_array),
            "sources={source} : la clé « versions » doit rester PRÉSENTE, \
             fût-elle vide : {v}"
        );
        assert!(
            v.get("streaming").is_some_and(Value::is_array),
            "sources={source} : la clé « streaming » doit rester PRÉSENTE : {v}"
        );
    }
}

/// Le contrat des bords : une source inconnue, ou vide, ne selectionne rien —
/// ni service ni local. Une liste presente est une selection EXPLICITE, elle
/// ne se replie pas sur « tout » quand elle ne reconnait rien.
#[tokio::test]
async fn versions_piste_une_source_inconnue_ne_selectionne_rien() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    for source in ["source-inconnue", ""] {
        let v = obtenir(
            &state,
            &format!("/api/v1/library/tracks/{piste}/versions"),
            Some(source),
        )
        .await;
        assert_eq!(
            tableau(&v, &["versions"]).len(),
            0,
            "sources={source} : rien de local : {v}"
        );
        assert_eq!(
            tableau(&v, &["streaming"]).len(),
            0,
            "sources={source} : aucun service : {v}"
        );
    }
}

/// `all` ne doit pas rendre MOINS que l'absence de parametre.
#[tokio::test]
async fn versions_piste_le_joker_all_vaut_tout() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let tout = obtenir(
        &state,
        &format!("/api/v1/library/tracks/{piste}/versions"),
        Some("all"),
    )
    .await;
    let sans = obtenir(
        &state,
        &format!("/api/v1/library/tracks/{piste}/versions"),
        None,
    )
    .await;
    assert_eq!(tout, sans, "`all` doit valoir le paramètre absent");
}

/// La liste est une UNION, pas un choix exclusif.
#[tokio::test]
async fn versions_piste_local_et_service_ensemble_rendent_les_deux() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let v = obtenir(
        &state,
        &format!("/api/v1/library/tracks/{piste}/versions"),
        Some("local,qobuz"),
    )
    .await;

    assert_eq!(tableau(&v, &["versions"]).len(), 1, "{v}");
    assert_eq!(
        services_cites(tableau(&v, &["streaming"])),
        vec!["qobuz".to_string()],
        "Qobuz seul, et Tidal écarté : {v}"
    );
}

// ═══ 2. `/home/other-versions` ════════════════════════════════════════════

#[tokio::test]
async fn accueil_versions_un_service_seul_ne_rend_plus_la_bibliotheque() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let a = obtenir(&state, "/api/v1/home/other-versions", Some("qobuz")).await;
    let groupes = a
        .as_array()
        .unwrap_or_else(|| panic!("liste attendue : {a}"));

    assert_eq!(
        groupes.len(),
        1,
        "le groupe existe par le streaming seul : {a}"
    );
    assert_eq!(
        tableau(&groupes[0], &["versions"]).len(),
        0,
        "« qobuz » demandé seul : les versions LOCALES du groupe doivent être \
         vides — la bibliothèque en porte pourtant une : {a}"
    );
    assert_eq!(
        services_cites(tableau(&groupes[0], &["streaming"])),
        vec!["qobuz".to_string()],
        "et Qobuz doit être là, seul : {a}"
    );
}

#[tokio::test]
async fn accueil_versions_local_seul_ne_rend_aucun_service() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let a = obtenir(&state, "/api/v1/home/other-versions", Some("local")).await;
    let groupes = a
        .as_array()
        .unwrap_or_else(|| panic!("liste attendue : {a}"));

    assert_eq!(groupes.len(), 1, "{a}");
    assert_eq!(
        tableau(&groupes[0], &["versions"]).len(),
        1,
        "« local » demandé seul : la version possédée doit être servie : {a}"
    );
    assert_eq!(
        groupes[0]["streaming"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        0,
        "et aucun service ne doit répondre : {a}"
    );
}

/// LE TEMOIN de l'accueil.
#[tokio::test]
async fn accueil_versions_le_temoin_sans_sources_rend_les_deux_moities() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let a = obtenir(&state, "/api/v1/home/other-versions", None).await;
    let groupes = a
        .as_array()
        .unwrap_or_else(|| panic!("liste attendue : {a}"));

    assert_eq!(groupes.len(), 1, "{a}");
    assert_eq!(groupes[0]["title"].as_str(), Some(TITRE), "{a}");
    assert_eq!(groupes[0]["artist_name"].as_str(), Some(ARTISTE), "{a}");
    assert_eq!(
        groupes[0]["played_album"].as_str(),
        Some(ALBUM_ECOUTE),
        "{a}"
    );
    assert_eq!(tableau(&groupes[0], &["versions"]).len(), 1, "{a}");
    assert_eq!(tableau(&groupes[0], &["streaming"]).len(), 2, "{a}");
    assert_eq!(
        tableau(&groupes[0], &["versions"])[0]["album_title"].as_str(),
        Some(ALBUM_POSSEDE),
        "{a}"
    );
}

/// ⚠️ La cle `versions` reste PRESENTE dans tout groupe rendu, meme vide :
/// `HomeView.svelte:1035` fait `{#each groupe.versions as v}` sans garde,
/// quand `groupe.streaming` est lu, lui, en `(groupe.streaming ?? [])`
/// (`HomeView.svelte:1022`). Un groupe sans `versions` planterait l'accueil.
#[tokio::test]
async fn accueil_versions_la_cle_locale_reste_presente_dans_chaque_groupe() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    for source in [None, Some("qobuz"), Some("local"), Some("all")] {
        let a = obtenir(&state, "/api/v1/home/other-versions", source).await;
        for g in a
            .as_array()
            .unwrap_or_else(|| panic!("liste attendue : {a}"))
        {
            assert!(
                g.get("versions").is_some_and(Value::is_array),
                "sources={source:?} : tout groupe rendu doit porter « versions », \
                 fût-il vide : {g}"
            );
        }
    }
}

#[tokio::test]
async fn accueil_versions_une_source_inconnue_ne_rend_rien() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    for source in ["source-inconnue", ""] {
        let a = obtenir(&state, "/api/v1/home/other-versions", Some(source)).await;
        assert_eq!(
            a,
            json!([]),
            "sources={source} : ni local ni service, donc aucun groupe : {a}"
        );
    }
}

// ═══ 3. `/home/artist-releases` ═══════════════════════════════════════════

/// ⚠️ Cette route n'a PAS de moitie locale a rendre : tout ce qu'elle sert
/// vient d'un service, la bibliotheque n'y est qu'un filtre. La ligne « un
/// service ⇒ bloc local vide » y est donc vraie par construction, et ce que
/// `sources` gouverne reellement, c'est QUEL service publie.
#[tokio::test]
async fn nouveautes_un_service_seul_ecarte_les_parutions_des_autres() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let n = obtenir(&state, "/api/v1/home/artist-releases", Some("qobuz")).await;
    let groupes = n
        .as_array()
        .unwrap_or_else(|| panic!("liste attendue : {n}"));

    assert_eq!(groupes.len(), 1, "{n}");
    assert_eq!(
        services_cites(tableau(&groupes[0], &["releases"])),
        vec!["qobuz".to_string()],
        "« qobuz » demandé seul : Tidal publie pourtant lui aussi une \
         nouveauté du même artiste, elle ne doit pas être là : {n}"
    );
    // La cle du groupe reste entiere : `HomeView.svelte:784` fait
    // `groupe.releases.length` sans garde.
    assert!(
        groupes[0].get("releases").is_some_and(Value::is_array),
        "{n}"
    );
    assert!(groupes[0].get("library_albums").is_some(), "{n}");
}

/// `sources=local` sur une section faite de parutions de services : il n'y a
/// aucune parution locale a montrer, donc la section est vide. C'est la
/// lecture fidele du contrat — « services : aucun ».
#[tokio::test]
async fn nouveautes_local_seul_ne_rend_aucune_parution() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let n = obtenir(&state, "/api/v1/home/artist-releases", Some("local")).await;
    assert_eq!(
        n,
        json!([]),
        "aucun service demandé : une section de nouveautés de streaming n'a \
         rien à montrer : {n}"
    );
}

/// LE TEMOIN des nouveautes.
#[tokio::test]
async fn nouveautes_le_temoin_sans_sources_rend_tous_les_services() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let n = obtenir(&state, "/api/v1/home/artist-releases", None).await;
    let groupes = n
        .as_array()
        .unwrap_or_else(|| panic!("liste attendue : {n}"));

    assert_eq!(groupes.len(), 1, "{n}");
    assert_eq!(groupes[0]["artist_name"].as_str(), Some(ARTISTE), "{n}");
    assert_eq!(tableau(&groupes[0], &["releases"]).len(), 2, "{n}");
    assert_eq!(
        services_cites(tableau(&groupes[0], &["releases"])),
        vec!["qobuz".to_string(), "tidal".to_string()],
        "{n}"
    );
    assert_eq!(groupes[0]["library_albums"].as_i64(), Some(2), "{n}");
}

#[tokio::test]
async fn nouveautes_une_source_inconnue_ne_rend_rien() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    for source in ["source-inconnue", ""] {
        let n = obtenir(&state, "/api/v1/home/artist-releases", Some(source)).await;
        assert_eq!(n, json!([]), "sources={source} : {n}");
    }
}

#[tokio::test]
async fn nouveautes_le_joker_all_vaut_tout() {
    let (state, piste) = banc().await;
    socle(&state, piste).await;

    let tout = obtenir(&state, "/api/v1/home/artist-releases", Some("all")).await;
    let sans = obtenir(&state, "/api/v1/home/artist-releases", None).await;
    assert_eq!(tout, sans, "`all` doit valoir le paramètre absent");
}
