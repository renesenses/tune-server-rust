//! #4716 — l'interface hôte WASM ouverte aux playlists, au streaming et à un
//! stockage (tranche 1 de l'épique #4715, greffon « Playlists converter »).
//!
//! Le refus deny-by-default de chaque capacité est gardé dans
//! `tune-plugin-runtime-wasm` (au niveau du bac à sable, là où la permission se
//! lit). Ici on garde l'AUTRE moitié : ce que ces capacités font vraiment
//! contre la base et contre un service, par [`AppStateHost`] — l'implémentation
//! que tout greffon chargé atteint.
//!
//! Ce que ces essais mesurent :
//!
//! 1. `playlists` — créer, lister, ajouter, relire : l'aller-retour complet,
//!    lu en base et non dans la réponse de l'appel qui vient de l'écrire.
//! 2. `kv` — le cloisonnement PAR GREFFON. Deux greffons, la même clé : deux
//!    valeurs. Et la contre-épreuve de la collision de clés (`a_b` + `x` contre
//!    `a` + `b_x`), qu'un séparateur `_` aurait fondues en une seule ligne.
//! 3. `streaming` — un service de banc : ce qui est annoncé, ce qui est écrit
//!    chez lui, et l'appariement — celui de `matching::apparier_chez_le_service`,
//!    partagé avec la route de transfert, jamais un second.

use std::sync::Arc;
use std::sync::Mutex;

use serde_json::{Value, json};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_plugin_runtime_wasm::HostContext;
use tune_server::plugins_host::AppStateHost;
use tune_server::state::AppState;

const SERVICE: &str = "banc";
const GREFFON: &str = "playlists-converter";

// ---------------------------------------------------------------------------
// Socle
// ---------------------------------------------------------------------------

/// Une base en mémoire avec `n` pistes en bibliothèque, prêtes à entrer dans
/// une playlist.
fn base_avec_pistes(n: i64) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState");
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Charles Aznavour')",
            &[],
        )
        .unwrap();
    state
        .backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    for i in 1..=n {
        let titre = format!("Piste {i}");
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) \
                 VALUES (?, ?, 1, 1, 210000)",
                &[&i as &dyn ToSqlValue, &titre.as_str()],
            )
            .unwrap();
    }
    state
}

// ---------------------------------------------------------------------------
// 1. `playlists`
// ---------------------------------------------------------------------------

#[test]
fn la_permission_playlists_cree_liste_ajoute_et_relit() {
    let state = base_avec_pistes(3);
    let host = AppStateHost::from_state(&state);

    // Créer
    let creee = host
        .playlist_create("Transfert Qobuz", Some("posée par un greffon"))
        .expect("playlist_create");
    let id = creee["playlist_id"].as_i64().expect("un identifiant");

    // La playlist existe VRAIMENT : lue en base, pas dans la réponse.
    let en_base = PlaylistRepo::with_backend(state.backend.clone())
        .get(id)
        .unwrap()
        .expect("la playlist doit exister en base");
    assert_eq!(en_base.name, "Transfert Qobuz");

    // Lister
    let liste = host.playlists_list(50, 0).expect("playlists_list");
    let noms: Vec<&str> = liste["playlists"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        noms.contains(&"Transfert Qobuz"),
        "la playlist créée doit apparaître dans la liste — vu {noms:?}"
    );

    // Ajouter
    let ajout = host.playlist_add_tracks(id, vec![1, 2, 3]).expect("add");
    assert_eq!(ajout["added"], 3);
    assert_eq!(ajout["demandees"], 3);

    // Relire : les pistes portent ce dont un convertisseur a besoin.
    let pistes = host.playlist_tracks(id).expect("playlist_tracks");
    assert_eq!(pistes["count"], 3);
    let premiere = &pistes["tracks"][0];
    assert_eq!(premiere["track_id"], 1);
    assert_eq!(premiere["title"], "Piste 1");
    assert_eq!(premiere["artist_name"], "Charles Aznavour");
    assert_eq!(premiere["duration_ms"], 210_000);
}

#[test]
fn playlist_add_tracks_ne_compte_que_ce_qui_est_entre() {
    // Une piste disparue : le greffon doit lire ce qui est ENTRÉ, jamais ce
    // qu'il a demandé (#3663, même règle que `queue_add`).
    let state = base_avec_pistes(2);
    let host = AppStateHost::from_state(&state);
    let id = host.playlist_create("Essai", None).unwrap()["playlist_id"]
        .as_i64()
        .unwrap();
    let ajout = host.playlist_add_tracks(id, vec![1, 2]).unwrap();
    assert_eq!(ajout["added"], 2, "contre-épreuve : rien ne manque");

    let posees = PlaylistRepo::with_backend(state.backend.clone())
        .get_track_ids(id)
        .unwrap();
    assert_eq!(posees, vec![1, 2]);
}

#[test]
fn une_playlist_d_un_autre_profil_reste_invisible() {
    // Les ids de playlists sont de petits entiers séquentiels : un greffon ne
    // doit pas pouvoir lire celle du voisin en devinant un numéro (#2794).
    let state = base_avec_pistes(1);
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let voisine = repo.create("Chez le voisin", None, 42).unwrap();

    let host = AppStateHost::from_state(&state);
    let erreur = host
        .playlist_tracks(voisine)
        .expect_err("la playlist d'un autre profil doit être introuvable");
    assert!(erreur.contains("introuvable"), "{erreur}");

    let refus = host
        .playlist_add_tracks(voisine, vec![1])
        .expect_err("et on ne doit pas pouvoir y écrire non plus");
    assert!(refus.contains("introuvable"), "{refus}");
}

// ---------------------------------------------------------------------------
// 2. `kv`
// ---------------------------------------------------------------------------

#[test]
fn le_stockage_kv_est_cloisonne_par_greffon() {
    let state = base_avec_pistes(0);
    let host = AppStateHost::from_state(&state);

    host.kv_set(GREFFON, "transfert/42", json!({ "pas": 3 }))
        .expect("kv_set greffon A");
    host.kv_set("un-autre-greffon", "transfert/42", json!({ "pas": 99 }))
        .expect("kv_set greffon B");

    let a = host.kv_get(GREFFON, "transfert/42").unwrap();
    let b = host.kv_get("un-autre-greffon", "transfert/42").unwrap();
    assert_eq!(a["value"], json!({ "pas": 3 }));
    assert_eq!(
        b["value"],
        json!({ "pas": 99 }),
        "la MÊME clé chez deux greffons doit porter deux valeurs"
    );

    // Et chacun ne liste que les siennes.
    let cles_a = host.kv_list(GREFFON, "").unwrap();
    assert_eq!(cles_a["keys"], json!(["transfert/42"]));
    assert_eq!(cles_a["count"], 1);
}

#[test]
fn kv_une_cle_absente_se_dit_absente_sans_erreur() {
    let state = base_avec_pistes(0);
    let host = AppStateHost::from_state(&state);
    let rendu = host.kv_get(GREFFON, "jamais-ecrite").unwrap();
    assert_eq!(rendu["found"], false);
    assert_eq!(rendu["value"], Value::Null);
}

#[test]
fn kv_deux_greffons_ne_peuvent_pas_se_marcher_dessus_par_collision_de_cles() {
    // 🔴 La collision qu'un séparateur `_` aurait fabriquée : le greffon `a_b`
    // avec la clé `x` et le greffon `a` avec la clé `b_x` auraient écrit dans
    // la MÊME ligne de `settings`. Deux états distincts sur une seule clé.
    let state = base_avec_pistes(0);
    let host = AppStateHost::from_state(&state);

    host.kv_set("a_b", "x", json!("celle de a_b")).unwrap();
    host.kv_set("a", "b_x", json!("celle de a")).unwrap();

    assert_eq!(
        host.kv_get("a_b", "x").unwrap()["value"],
        json!("celle de a_b")
    );
    assert_eq!(
        host.kv_get("a", "b_x").unwrap()["value"],
        json!("celle de a")
    );
}

#[test]
fn kv_sans_identifiant_de_greffon_refuse() {
    let state = base_avec_pistes(0);
    let host = AppStateHost::from_state(&state);
    let erreur = host
        .kv_set("", "x", json!(1))
        .expect_err("sans identifiant, la clé ne serait cloisonnée par rien");
    assert!(erreur.contains("non identifié"), "{erreur}");
}

#[test]
fn kv_le_prefixe_de_cloisonnement_ne_fuit_pas_vers_le_greffon() {
    let state = base_avec_pistes(0);
    let host = AppStateHost::from_state(&state);
    host.kv_set(GREFFON, "snapshot/1", json!(1)).unwrap();
    host.kv_set(GREFFON, "snapshot/2", json!(2)).unwrap();
    host.kv_set(GREFFON, "lien/1", json!(3)).unwrap();

    let snapshots = host.kv_list(GREFFON, "snapshot/").unwrap();
    assert_eq!(snapshots["keys"], json!(["snapshot/1", "snapshot/2"]));

    // La ligne réellement écrite porte bien le préfixe — que le greffon ne voit
    // jamais.
    let brute = SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("plugin_kv:{GREFFON}:lien/1"))
        .unwrap();
    assert_eq!(brute.as_deref(), Some("3"));
}

#[test]
fn kv_refuse_une_valeur_demesuree() {
    let state = base_avec_pistes(0);
    let host = AppStateHost::from_state(&state);
    let enorme = "x".repeat(300 * 1024);
    let erreur = host
        .kv_set(GREFFON, "gros", json!(enorme))
        .expect_err("la table settings est relue en entier : il faut une borne");
    assert!(erreur.contains("trop grande"), "{erreur}");
}

// ---------------------------------------------------------------------------
// 3. `streaming`
// ---------------------------------------------------------------------------

/// Un service de banc qui SAIT écrire : il note les playlists créées et les
/// pistes ajoutées, et rend un résultat de recherche appariable.
struct ServiceDeBanc {
    creees: Arc<Mutex<Vec<String>>>,
    ajoutees: Arc<Mutex<Vec<(String, Vec<String>)>>>,
}

#[async_trait::async_trait]
impl StreamingService for ServiceDeBanc {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        SERVICE
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

    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: vec![
                piste("banc-99", "Quelque chose d'autre", "Un autre"),
                // Accents et suffixe de remasterisation : seul l'appariement
                // partagé sait retrouver « La Boheme » là-dedans.
                piste("banc-1", "La Bohème (Remastered 2014)", "Charles Aznavour"),
                // Une AUTRE prise du même titre, bien plus courte : c'est elle
                // que le greffon retiendra quand sa tolérance de durée
                // écartera le verdict.
                piste_duree("banc-2", "La Bohème", "Charles Aznavour", 150_500),
            ],
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }

    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_track_url(
        &self,
        _id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(TuneError::NotFound("banc".into()))
    }
    async fn get_playlist_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![piste(
            &format!("{id}-t1"),
            "La Bohème",
            "Charles Aznavour",
        )])
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![StreamPlaylist {
            id: "pl-1".into(),
            name: "Mes classiques".into(),
            description: None,
            cover_path: None,
            track_count: 1,
            owner: None,
        }])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }

    async fn create_playlist(
        &self,
        name: &str,
        _description: Option<&str>,
    ) -> Result<String, TuneError> {
        self.creees.lock().unwrap().push(name.to_string());
        Ok("pl-neuve".to_string())
    }
    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        self.ajoutees
            .lock()
            .unwrap()
            .push((playlist_id.to_string(), track_ids.to_vec()));
        Ok(track_ids.len())
    }
    fn supports_write(&self) -> bool {
        true
    }
}

fn piste(id: &str, titre: &str, artiste: &str) -> StreamTrack {
    piste_duree(id, titre, artiste, 210_000)
}

fn piste_duree(id: &str, titre: &str, artiste: &str, duree_ms: u64) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: None,
        album_id: None,
        duration_ms: duree_ms,
        cover_path: None,
        track_number: Some(1),
        disc_number: Some(1),
        explicit: false,
        disponible: None,
        isrc: None,
        composer: None,
        artist_id: None,
        quality: None,
    }
}

/// Un état avec le service de banc enregistré, et les deux journaux d'écriture.
#[allow(clippy::type_complexity)]
async fn banc() -> (
    AppState,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<Vec<(String, Vec<String>)>>>,
) {
    let state = base_avec_pistes(0);
    let creees = Arc::new(Mutex::new(Vec::new()));
    let ajoutees = Arc::new(Mutex::new(Vec::new()));
    state
        .services
        .lock()
        .await
        .register(Box::new(ServiceDeBanc {
            creees: creees.clone(),
            ajoutees: ajoutees.clone(),
        }));
    (state, creees, ajoutees)
}

/// Les capacités `streaming` passent par `block_on`, qui n'est licite qu'HORS
/// d'un worker tokio — exactement comme en production, où le gestionnaire de
/// route conduit l'appel wasm dans `spawn_blocking`.
async fn hors_du_worker<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .expect("spawn_blocking")
}

#[tokio::test]
async fn streaming_n_annonce_que_les_services_authentifies() {
    let (state, _, _) = banc().await;
    let host = AppStateHost::from_state(&state);
    let rendu = hors_du_worker(move || host.streaming_services())
        .await
        .unwrap();
    assert_eq!(rendu["count"], 1);
    assert_eq!(rendu["services"][0]["name"], SERVICE);
    assert_eq!(rendu["services"][0]["supports_write"], true);
}

#[tokio::test]
async fn streaming_lit_les_playlists_et_leurs_pistes() {
    let (state, _, _) = banc().await;
    let host = Arc::new(AppStateHost::from_state(&state));

    let h = host.clone();
    let listes = hors_du_worker(move || h.streaming_playlists(SERVICE))
        .await
        .unwrap();
    assert_eq!(listes["count"], 1);
    assert_eq!(listes["playlists"][0]["name"], "Mes classiques");

    let h = host.clone();
    let pistes = hors_du_worker(move || h.streaming_playlist_tracks(SERVICE, "pl-1"))
        .await
        .unwrap();
    assert_eq!(pistes["count"], 1);
    assert_eq!(pistes["tracks"][0]["title"], "La Bohème");
}

#[tokio::test]
async fn streaming_ecrit_chez_le_service_creation_puis_ajout() {
    let (state, creees, ajoutees) = banc().await;
    let host = Arc::new(AppStateHost::from_state(&state));

    let h = host.clone();
    let creee = hors_du_worker(move || h.streaming_playlist_create(SERVICE, "Copie", None))
        .await
        .unwrap();
    assert_eq!(creee["playlist_id"], "pl-neuve");
    assert_eq!(creees.lock().unwrap().as_slice(), &["Copie".to_string()]);

    let h = host.clone();
    let ajout = hors_du_worker(move || {
        h.streaming_playlist_add_tracks(
            SERVICE,
            "pl-neuve",
            vec!["banc-1".to_string(), "banc-2".to_string()],
        )
    })
    .await
    .unwrap();
    assert_eq!(ajout["added"], 2);
    let journal = ajoutees.lock().unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].0, "pl-neuve");
    assert_eq!(journal[0].1, vec!["banc-1", "banc-2"]);
}

#[tokio::test]
async fn streaming_apparie_avec_l_appariement_partage() {
    // Le service rend deux résultats dont un seul est le bon, avec accents et
    // « (Remastered 2014) ». Un « prends le premier » se tromperait ; c'est
    // l'appariement de `matching` — celui de la route de transfert — qui
    // tranche.
    let (state, _, _) = banc().await;
    let host = AppStateHost::from_state(&state);
    let rendu = hors_du_worker(move || {
        host.streaming_match_track(SERVICE, "La Boheme", "Charles Aznavour", "", 210_000)
    })
    .await
    .unwrap();
    assert_eq!(rendu["matched"]["source_id"], "banc-1");
    assert_eq!(rendu["approximate"], false);
    // La forme d'avant est intacte, et le verdict est la TÊTE du classement.
    assert_eq!(rendu["candidates"][0]["track"]["source_id"], "banc-1");
}

/// 🔴 Le manque que cette tranche répare, chez un service : le verdict rate la
/// tolérance de durée que le greffon applique ensuite (±3 s), et un autre
/// résultat de la MÊME recherche l'aurait tenue. Avec un seul candidat rendu,
/// le titre ressortait « introuvable ».
#[tokio::test]
async fn streaming_match_track_rend_un_second_candidat_quand_le_premier_rate_la_duree() {
    const TOLERANCE_MS: u64 = 3_000;
    let source_ms: u64 = 150_000;
    let (state, _, _) = banc().await;
    let host = AppStateHost::from_state(&state);
    let rendu = hors_du_worker(move || {
        host.streaming_match_track(SERVICE, "La Boheme", "Charles Aznavour", "", source_ms)
    })
    .await
    .unwrap();

    // Le verdict est inchangé — et il rate la tolérance.
    assert_eq!(rendu["matched"]["source_id"], "banc-1", "{rendu}");
    let ecart = rendu["matched"]["duration_ms"]
        .as_u64()
        .unwrap()
        .abs_diff(source_ms);
    assert!(
        ecart > TOLERANCE_MS,
        "le verdict doit bien rater la tolérance, sinon l'essai ne prouve rien"
    );

    // Le greffon a maintenant un recours, borné et classé.
    let candidats = rendu["candidates"].as_array().expect("une liste");
    assert_eq!(candidats.len(), 2, "{rendu}");
    let retenu = candidats
        .iter()
        .find(|c| {
            c["track"]["duration_ms"]
                .as_u64()
                .unwrap_or(0)
                .abs_diff(source_ms)
                <= TOLERANCE_MS
        })
        .expect("un candidat doit tenir la tolérance de durée");
    assert_eq!(retenu["track"]["source_id"], "banc-2");
}

#[tokio::test]
async fn streaming_un_service_inconnu_se_dit_inconnu() {
    let (state, _, _) = banc().await;
    let host = AppStateHost::from_state(&state);
    let erreur = hors_du_worker(move || host.streaming_playlists("jamais-vu"))
        .await
        .expect_err("un service absent du registre n'est pas une réussite vide");
    assert!(erreur.contains("inconnu"), "{erreur}");
}
