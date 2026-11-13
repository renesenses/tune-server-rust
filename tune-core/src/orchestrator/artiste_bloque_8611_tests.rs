//! #8611 — témoins de la garde des artistes bloqués à la lecture : une piste
//! locale (artiste, album, balise) ou de service d'un artiste bloqué est
//! refusée au point d'entrée de `play()` pour le profil supervisé ; le profil
//! administrateur sur la même base joue tout ; dans une file, la piste
//! bloquée est sautée et le saut dit sa raison.
//!
//! Services simulés (aucun appel réel), aucune sortie audible.
use std::sync::Arc;

use super::artiste_bloque_8611::{sentinelle_contenu_bloque, sentinelle_politique_illisible};
use super::enjamber_les_refus::SensDuGeste;
use super::{PlayRequest, PlaybackOrchestrator};
use crate::TuneError;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::politique_parentale::{
    ArtisteBloque, CODE_POLITIQUE_ILLISIBLE, CODE_REFUS_CONTENU_BLOQUE, PolitiqueParentale, poser,
};
use crate::source_url::{FournisseurDUrl, RefusDUrl, UrlFournie};
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;
const ZONE: i64 = 86111;
const INTERDIT: i64 = 7;
const LIBRE: i64 = 8;
const PISTE_INTERDITE: i64 = 700;
const PISTE_LIBRE: i64 = 800;
/// Piste du libre, sur l'album de l'artiste bloqué.
const PISTE_SUR_ALBUM_INTERDIT: i64 = 802;

/// `get_track` rend l'artiste « Groupe Interdit » pour un identifiant qui
/// commence par `interdit`.
struct ServiceSimule {
    nom: &'static str,
}

#[async_trait::async_trait]
impl StreamingService for ServiceSimule {
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
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        if id == "muet" {
            return Err("service injoignable".into());
        }
        Ok(StreamTrack {
            id: id.to_string(),
            title: "Titre".into(),
            artist: if id.starts_with("interdit") {
                "Groupe Interdit".into()
            } else {
                "Groupe Libre".into()
            },
            album: None,
            album_id: None,
            duration_ms: 180_000,
            cover_path: None,
            track_number: Some(1),
            disc_number: None,
            explicit: false,
            quality: None,
            isrc: None,
            disponible: None,
            composer: None,
            artist_id: None,
        })
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
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

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let sql = |s: &str, p: &[&dyn ToSqlValue]| {
        db.execute(s, p).unwrap();
    };
    for (id, nom) in [(INTERDIT, "Groupe Interdit"), (LIBRE, "Groupe Libre")] {
        sql("INSERT INTO artists (id, name) VALUES (?, ?)", &[&id, &nom]);
        sql(
            "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, ?)",
            &[&(id * 10), &format!("Album de {nom}"), &id],
        );
        sql(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path) VALUES (?, ?, ?, ?, ?)",
            &[
                &(id * 100),
                &"Titre",
                &id,
                &(id * 10),
                &format!("/m/{id}.flac"),
            ],
        );
    }
    sql(
        "INSERT INTO tracks (id, title, artist_id, album_id, file_path) \
         VALUES (802, 'Featuring', 8, 70, '/m/802.flac')",
        &[],
    );
    db
}

async fn orchestrateur() -> PlaybackOrchestrator {
    let db = base();
    db.execute(
        "INSERT INTO zones (id, name, output_type, output_device_id) \
         VALUES (?, 'Zone 8611', 'local', 'local:banc-8611')",
        &[&ZONE as &dyn ToSqlValue],
    )
    .unwrap();
    let mut services = crate::streaming::registry::ServiceRegistry::new();
    for nom in ["qobuz", "tidal"] {
        services.register(Box::new(ServiceSimule { nom }));
    }
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(services)),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.playback.set_session_profile(ZONE, Some(ENFANT)).await;
    orch
}

fn bloquer(db: &Arc<dyn DbBackend>) {
    poser(
        db,
        ENFANT,
        ADMIN,
        0,
        &PolitiqueParentale {
            artistes_bloques: vec![ArtisteBloque {
                id: Some(INTERDIT),
                nom: Some("Groupe Interdit".into()),
            }],
            ..Default::default()
        },
    )
    .unwrap();
}

fn locale(id: i64) -> PlayRequest {
    PlayRequest {
        zone_id: ZONE,
        track_id: Some(id),
        ..Default::default()
    }
}

fn service(nom: &str, id: &str, artiste: Option<&str>) -> PlayRequest {
    PlayRequest {
        zone_id: ZONE,
        source: Some(nom.into()),
        source_id: Some(id.into()),
        title: Some("Titre".into()),
        artist_name: artiste.map(String::from),
        ..Default::default()
    }
}

/// Les demandes d'un artiste bloqué, et les autres.
fn demandes() -> (Vec<PlayRequest>, Vec<PlayRequest>) {
    (
        vec![
            locale(PISTE_INTERDITE),
            locale(PISTE_SUR_ALBUM_INTERDIT),
            // Le nom porté par la demande (file, serveur média).
            service("upnp", "x", Some("groupe interdit")),
            // Le nom que le service rend, la demande n'en portant pas.
            service("qobuz", "interdit-1", None),
            // Un nom maquillé par le client ne trompe pas le service.
            service("tidal", "interdit-2", Some("Groupe Libre")),
        ],
        vec![
            locale(PISTE_LIBRE),
            service("qobuz", "libre-1", None),
            service("upnp", "y", Some("Groupe Libre")),
            // Un service muet ne dit rien : pas de blocage.
            service("qobuz", "muet", None),
        ],
    )
}

#[tokio::test]
async fn la_garde_refuse_l_artiste_bloque_et_laisse_le_reste_8611() {
    let orch = orchestrateur().await;
    bloquer(&orch.db);
    assert!(orch.garde_parentale_par_piste(ZONE).await);
    let (bloquees, autres) = demandes();
    for d in &bloquees {
        let e = orch
            .verifier_l_artiste_bloque(d)
            .await
            .expect_err(&format!("{d:?} : artiste bloqué, elle doit être refusée"));
        assert!(e.contains(CODE_REFUS_CONTENU_BLOQUE), "{e}");
    }
    for d in &autres {
        assert_eq!(orch.verifier_l_artiste_bloque(d).await, Ok(()), "{d:?}");
    }
}

/// `play()` lui-même refuse, au point d'étranglement de toutes les entrées :
/// rien ne part.
#[tokio::test]
async fn play_refuse_l_artiste_bloque_au_point_d_entree_8611() {
    let orch = orchestrateur().await;
    bloquer(&orch.db);
    let (bloquees, autres) = demandes();
    for d in bloquees {
        let e = orch
            .play(d.clone())
            .await
            .err()
            .unwrap_or_else(|| panic!("{d:?} : la lecture devait être refusée"));
        assert_eq!(e, sentinelle_contenu_bloque(), "{d:?}");
        assert!(
            orch.playback.get_state(ZONE).await.now_playing.is_none(),
            "{d:?} : la zone ne doit rien jouer"
        );
    }
    for d in autres {
        // Sans sortie, la lecture échoue plus loin : seule compte l'absence
        // du refus parental.
        if let Err(e) = orch.play(d.clone()).await {
            assert!(
                !e.contains(CODE_REFUS_CONTENU_BLOQUE),
                "{d:?} : refusée à tort : {e}"
            );
        }
    }
}

/// Témoin administrateur : la même base, la même politique pour l'enfant ;
/// l'administrateur sur la zone joue tout.
#[tokio::test]
async fn l_administrateur_n_est_pas_filtre_8611() {
    let orch = orchestrateur().await;
    bloquer(&orch.db);
    orch.playback.set_session_profile(ZONE, Some(ADMIN)).await;
    assert!(!orch.garde_parentale_par_piste(ZONE).await);
    let (bloquees, _) = demandes();
    for d in &bloquees {
        assert_eq!(orch.verifier_l_artiste_bloque(d).await, Ok(()), "{d:?}");
    }
}

/// Sans politique, ou sans artiste bloqué, rien ne change et rien ne se lit.
#[tokio::test]
async fn sans_artiste_bloque_rien_ne_change_8611() {
    let orch = orchestrateur().await;
    let (bloquees, _) = demandes();
    assert!(
        !orch.garde_parentale_par_piste(ZONE).await,
        "sans politique"
    );
    for d in &bloquees {
        assert_eq!(orch.verifier_l_artiste_bloque(d).await, Ok(()), "{d:?}");
    }
    poser(
        &orch.db,
        ENFANT,
        ADMIN,
        0,
        &PolitiqueParentale {
            budget_quotidien_minutes: Some(600),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!orch.garde_parentale_par_piste(ZONE).await, "aucun artiste");
    for d in &bloquees {
        assert_eq!(orch.verifier_l_artiste_bloque(d).await, Ok(()), "{d:?}");
    }
}

/// En fin de piste et sur « Suivant », le refus est propre à la piste : la
/// file passe à la suivante ; l'auditeur lit la vraie raison.
#[test]
fn le_refus_est_propre_a_la_piste_et_se_dit_8611() {
    let e = sentinelle_contenu_bloque();
    assert!(crate::poller::refus_propre_a_la_piste(&e));
    assert!(crate::poller::refus_propre_a_la_piste(&format!(
        "play failed: {e}"
    )));
    let m = PlaybackOrchestrator::message_piste_non_demarree("suivante", &e);
    assert!(m.contains("artiste bloqué"), "{m}");
    assert!(m.contains("contrôle parental"), "{m}");
    assert!(
        super::enjamber_les_refus::raison_du_refus(&e).contains("contrôle parental"),
        "la raison du saut doit être dite"
    );
}

// --- Dans une file : la piste bloquée est sautée, la suivante joue. ---

const APPAREIL: &str = "dlna:banc-8611";
const SOURCE: &str = "circle";

struct FauxFournisseur {
    demandes: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl FournisseurDUrl for FauxFournisseur {
    async fn url(&self, _zone_id: i64, source_id: &str) -> Result<UrlFournie, RefusDUrl> {
        self.demandes.lock().unwrap().push(source_id.to_string());
        Ok(UrlFournie {
            url: format!("http://127.0.0.1:9/stream/{}", source_id.replace(':', "-")),
            media_format: Some("flac".into()),
        })
    }
}

fn ligne(reference: &str, titre: &str, artiste: &str) -> QueueInput {
    QueueInput::Streaming {
        source: SOURCE.into(),
        source_id: reference.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: None,
        cover_url: None,
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

struct BancDeFile {
    db: Arc<dyn DbBackend>,
    orch: Arc<PlaybackOrchestrator>,
    playback: Arc<crate::playback::PlaybackManager>,
    zone_id: i64,
    fournisseur: Arc<FauxFournisseur>,
    bus: Arc<crate::event_bus::EventBus>,
}

/// Une zone DLNA simulée, une file de trois titres (le deuxième de
/// l'artiste bloqué), le profil `profil` à l'écoute.
async fn banc_de_file(profil: i64) -> BancDeFile {
    use crate::outputs::OutputRegistry;
    use crate::outputs::mock::MockOutput;
    let db = base();
    let zone_id = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .create("Chambre", Some("dlna"), Some(APPAREIL))
        .unwrap();
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone_id,
            &[
                ligne("8611:1", "Un", "Groupe Libre"),
                ligne("8611:2", "Deux", "Groupe Interdit"),
                ligne("8611:3", "Trois", "Groupe Libre"),
            ],
        )
        .unwrap();
    let outputs = Arc::new(tokio::sync::Mutex::new(OutputRegistry::new()));
    outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Renderer").with_type("dlna"),
    ));
    let playback = Arc::new(crate::playback::PlaybackManager::new());
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::ServiceRegistry::new(),
        )),
        outputs,
        None,
    );
    orch.event_bus = Some(bus.clone());
    let orch = Arc::new(orch);
    let fournisseur = Arc::new(FauxFournisseur {
        demandes: std::sync::Mutex::new(Vec::new()),
    });
    orch.sources_url().inscrire(SOURCE, fournisseur.clone());
    playback.update_queue_info(zone_id, 0, 3).await;
    playback.set_session_profile(zone_id, Some(profil)).await;
    BancDeFile {
        db,
        orch,
        playback,
        zone_id,
        fournisseur,
        bus,
    }
}

fn sauts(
    rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
) -> Vec<serde_json::Value> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if e.event_type == "playback.track_skipped" {
            v.push(e.data.clone());
        }
    }
    v
}

#[tokio::test]
async fn dans_une_file_la_piste_bloquee_est_sautee_8611() {
    let BancDeFile {
        db,
        orch,
        playback,
        zone_id,
        fournisseur,
        bus,
    } = banc_de_file(ENFANT).await;
    bloquer(&db);
    let mut rx = bus.subscribe();

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        orch.jouer_en_enjambant_les_refus(zone_id, 1, SensDuGeste::Suivant),
    )
    .await
    .expect("le geste doit rendre la main")
    .expect("la piste bloquée sautée, la suivante doit jouer");

    // La piste bloquée n'a jamais été demandée à sa source : rien n'est parti.
    assert_eq!(*fournisseur.demandes.lock().unwrap(), vec!["8611:3"]);
    let etat = playback.get_state(zone_id).await;
    assert_eq!(etat.queue_position, 2);
    assert_eq!(
        etat.now_playing.and_then(|n| n.source_id).as_deref(),
        Some("8611:3")
    );
    let saut = sauts(&mut rx)
        .pop()
        .expect("le saut de la piste bloquée doit être annoncé");
    assert_eq!(saut["position"], 1);
    assert!(
        saut["reason"]
            .as_str()
            .unwrap_or_default()
            .contains(CODE_REFUS_CONTENU_BLOQUE),
        "{saut}"
    );

    // Témoin : la même file pour l'administrateur joue la piste « bloquée ».
    playback.set_session_profile(zone_id, Some(ADMIN)).await;
    fournisseur.demandes.lock().unwrap().clear();
    orch.jouer_en_enjambant_les_refus(zone_id, 1, SensDuGeste::Suivant)
        .await
        .expect("l'administrateur joue la piste");
    assert_eq!(*fournisseur.demandes.lock().unwrap(), vec!["8611:2"]);
}

// --- Politique qui ne se relit pas (relecture de #8636). ---

/// La ligne de la politique de l'enfant, corrompue.
fn corrompre(db: &Arc<dyn DbBackend>) {
    db.execute(
        "UPDATE politiques_parentales SET politique_json = '{pas du json' WHERE profile_id = ?",
        &[&ENFANT as &dyn ToSqlValue],
    )
    .unwrap();
}

/// La base ne relit plus la table des politiques (erreur qui n'est ni une
/// table absente ni une ligne corrompue : base occupée, PG coupé).
fn casser_la_lecture(db: &Arc<dyn DbBackend>) {
    db.execute(
        "ALTER TABLE politiques_parentales RENAME COLUMN politique_json TO autre_8611",
        &[],
    )
    .unwrap();
}

fn demande_de_la_file(zone_id: i64, artiste: &str) -> PlayRequest {
    PlayRequest {
        zone_id,
        source: Some(SOURCE.into()),
        source_id: Some("8611:9".into()),
        artist_name: Some(artiste.into()),
        ..Default::default()
    }
}

/// Politique corrompue : la garde des artistes refuse au code
/// `parental_policy_unreadable`, qui n'est pas propre à la piste et ARRÊTE
/// la file : pas de saut en chaîne, rien n'est demandé à la source.
#[tokio::test]
async fn une_politique_corrompue_arrete_la_file_sans_sauter_8611() {
    let e = sentinelle_politique_illisible();
    assert!(
        !crate::poller::refus_propre_a_la_piste(&e),
        "pas propre à la piste"
    );
    assert!(crate::source_url::arret_de_la_file(&e), "la file s'arrête");
    let m = PlaybackOrchestrator::message_piste_non_demarree("suivante", &e);
    assert!(m.contains("illisible"), "{m}");

    let banc = banc_de_file(ENFANT).await;
    bloquer(&banc.db);
    corrompre(&banc.db);
    assert_eq!(
        banc.orch
            .verifier_l_artiste_bloque(&demande_de_la_file(banc.zone_id, "Groupe Libre"))
            .await,
        Err(sentinelle_politique_illisible())
    );
    assert!(!e.contains(CODE_REFUS_CONTENU_BLOQUE));
    let mut rx = banc.bus.subscribe();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        banc.orch
            .jouer_en_enjambant_les_refus(banc.zone_id, 0, SensDuGeste::Suivant),
    )
    .await
    .expect("le geste doit rendre la main")
    .expect_err("politique corrompue : rien ne joue");
    assert!(sauts(&mut rx).is_empty(), "aucun saut en chaîne");
    assert!(banc.fournisseur.demandes.lock().unwrap().is_empty());

    // Témoin : l'administrateur (sans politique) joue sur la même base.
    banc.playback
        .set_session_profile(banc.zone_id, Some(ADMIN))
        .await;
    banc.orch
        .jouer_en_enjambant_les_refus(banc.zone_id, 1, SensDuGeste::Suivant)
        .await
        .expect("l'administrateur n'est pas bloqué");
    assert_eq!(*banc.fournisseur.demandes.lock().unwrap(), vec!["8611:2"]);
}

/// La base ne relit plus les politiques (base occupée, PG coupé) : la garde
/// des artistes ne bloque RIEN et ne refuse pas en chaîne ; l'administrateur
/// joue la file. (Pour le profil supervisé, le bail #7974 garde son propre
/// comportement : seule la garde des artistes est jugée ici.)
#[tokio::test]
async fn une_base_qui_ne_repond_pas_ne_bloque_pas_sur_l_artiste_8611() {
    let banc = banc_de_file(ADMIN).await;
    bloquer(&banc.db);
    casser_la_lecture(&banc.db);
    let mut rx = banc.bus.subscribe();
    banc.orch
        .jouer_en_enjambant_les_refus(banc.zone_id, 1, SensDuGeste::Suivant)
        .await
        .expect("l'administrateur n'est pas bloqué pendant la panne");
    assert_eq!(*banc.fournisseur.demandes.lock().unwrap(), vec!["8611:2"]);
    assert!(sauts(&mut rx).is_empty(), "aucun saut");

    banc.playback
        .set_session_profile(banc.zone_id, Some(ENFANT))
        .await;
    assert_eq!(
        banc.orch
            .verifier_l_artiste_bloque(&demande_de_la_file(banc.zone_id, "Groupe Interdit"))
            .await,
        Ok(()),
        "base muette : la garde des artistes ne refuse rien"
    );
    // Pas de préparation d'avance pendant la panne.
    assert!(banc.orch.garde_parentale_par_piste(banc.zone_id).await);
}
