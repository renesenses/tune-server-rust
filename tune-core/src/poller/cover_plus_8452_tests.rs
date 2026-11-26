//! #8452 — Cover+ au SITE D'APPEL : `refresh_radio_metadata`, face à une
//! fausse station ICY qui ne donne aucune pochette. La recherche iTunes
//! n'est pas jouée ici (les épreuves de `cover_plus` s'en chargent, face à
//! un faux iTunes) : la pochette est semée dans le cache, et l'on constate ce
//! que le sondeur en fait.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::db::backend::DbBackend;
use crate::db::radio_repo::{RadioRepo, RadioStation};
use crate::db::sqlite::SqliteDb;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::poller::PositionPoller;

const LOGO: &str = "https://logos.example/8452.png";

fn trouvaille(url: &str) -> crate::cover_plus::Trouvaille {
    crate::cover_plus::Trouvaille {
        url: url.into(),
        origine: crate::cover_plus::Origine::Pochette,
    }
}

/// Une station ICY qui annonce « Air - Sexy Boy », sans pochette.
async fn fausse_station(chemin: &'static str) -> String {
    fausse_station_dite(chemin, "Air - Sexy Boy").await
}

/// Une station ICY qui annonce `titre`, sans pochette.
async fn fausse_station_dite(chemin: &'static str, titre: &str) -> String {
    let meta = format!("StreamTitle='{titre}';");
    let blocs = meta.len().div_ceil(16);
    let mut corps = vec![0u8; 16];
    corps.push(blocs as u8);
    let mut charge = meta.into_bytes();
    charge.resize(blocs * 16, 0);
    corps.extend_from_slice(&charge);
    corps.extend_from_slice(&[0u8; 16]);
    let app = axum::Router::new().route(
        chemin,
        axum::routing::get(move || {
            let corps = corps.clone();
            async move {
                (
                    [("icy-metaint", "16"), ("content-type", "audio/mpeg")],
                    corps,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{chemin}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    db: Arc<dyn DbBackend>,
}

fn banc() -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let playback = Arc::new(PlaybackManager::new());
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    let services = Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new()));
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        services,
        outputs.clone(),
        None,
    ));
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs,
        db.clone(),
        Arc::new(Mutex::new(HashMap::new())),
    );
    Banc {
        poller,
        playback,
        db,
    }
}

/// La station dans « Mes radios », avec son logo et son pays en clair.
fn station_en_base(db: &Arc<dyn DbBackend>, nom: &str, url: &str) -> i64 {
    RadioRepo::with_backend(db.clone())
        .create(&RadioStation {
            id: None,
            name: nom.into(),
            url: url.into(),
            homepage: None,
            logo_url: Some(LOGO.into()),
            country: Some("France".into()),
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .unwrap()
}

async fn jouer_puis_sonder(b: &Banc, zone_id: i64, nom: &str, url: &str) -> NowPlaying {
    let np = NowPlaying {
        title: nom.into(),
        album_title: Some(nom.into()),
        source: "radio".into(),
        source_id: Some(url.to_string()),
        ..Default::default()
    };
    b.playback.play(zone_id, np).await;
    let etat = b.playback.get_state(zone_id).await;
    b.poller.refresh_radio_metadata(zone_id, &etat).await;
    b.playback
        .get_state(zone_id)
        .await
        .now_playing
        .expect("la zone joue")
}

#[tokio::test]
async fn la_pochette_cover_plus_remplace_le_logo_et_porte_son_label() {
    let b = banc();
    let nom = "Radio Cover+ 8452 sondeur";
    let url = fausse_station("/cp8452a").await;
    station_en_base(&b.db, nom, &url);
    let pochette = "https://is1.mzstatic.example/moon/600x600bb.jpg";
    crate::cover_plus::regler_globalement(&b.db, true).unwrap();
    crate::cover_plus::semer(
        Some("France"),
        nom,
        "Air",
        "Sexy Boy",
        Some(trouvaille(pochette)),
    );

    let np = jouer_puis_sonder(&b, 1, nom, &url).await;
    assert_eq!(np.title, "Sexy Boy");
    assert_eq!(np.cover_path.as_deref(), Some(pochette));
    assert!(
        np.cover_plus,
        "le label « Cover+ » doit accompagner la pochette"
    );
    let json = serde_json::to_value(&np).unwrap();
    assert_eq!(json["cover_plus"], true, "le champ voyage vers les clients");
    assert_eq!(json["cover_plus_source"], "itunes");
}

#[tokio::test]
async fn station_coupee_le_logo_reste_sans_label() {
    let b = banc();
    let nom = "Radio Cover+ 8452 coupee";
    let url = fausse_station("/cp8452b").await;
    let id = station_en_base(&b.db, nom, &url);
    crate::cover_plus::regler_globalement(&b.db, true).unwrap();
    crate::cover_plus::semer(
        Some("France"),
        nom,
        "Air",
        "Sexy Boy",
        Some(trouvaille(
            "https://is1.mzstatic.example/moon/600x600bb.jpg",
        )),
    );
    crate::cover_plus::regler_la_station(&b.db, id, false).unwrap();

    let np = jouer_puis_sonder(&b, 2, nom, &url).await;
    assert_eq!(np.title, "Sexy Boy");
    assert_eq!(np.cover_path.as_deref(), Some(LOGO));
    assert!(!np.cover_plus);
}

/// Sans pochette connue, le logo — jamais la pochette d'un titre précédent.
#[tokio::test]
async fn sans_pochette_connue_le_logo_de_la_station() {
    let b = banc();
    let nom = "Radio Cover+ 8452 attente";
    let url = fausse_station("/cp8452c").await;
    station_en_base(&b.db, nom, &url);

    crate::cover_plus::regler_globalement(&b.db, true).unwrap();
    let np = jouer_puis_sonder(&b, 3, nom, &url).await;
    assert_eq!(np.cover_path.as_deref(), Some(LOGO));
    assert!(!np.cover_plus);
    assert!(
        serde_json::to_value(&np)
            .unwrap()
            .get("cover_plus_source")
            .is_none(),
        "pas de source sans image Cover+"
    );
}

/// § 4 : à chaque changement de titre, l'image précédente ne reste pas.
/// Le titre suivant, pas encore cherché, revient au logo de la station.
#[tokio::test]
async fn au_changement_de_titre_l_image_precedente_s_en_va() {
    let b = banc();
    let nom = "Radio Cover+ 8452 changement";
    let url_a = fausse_station_dite("/cp8452d", "Air - Sexy Boy").await;
    station_en_base(&b.db, nom, &url_a);
    crate::cover_plus::regler_globalement(&b.db, true).unwrap();
    let pochette = "https://is1.mzstatic.example/moon/600x600bb.jpg";
    crate::cover_plus::semer(
        Some("France"),
        nom,
        "Air",
        "Sexy Boy",
        Some(trouvaille(pochette)),
    );
    let np = jouer_puis_sonder(&b, 4, nom, &url_a).await;
    assert_eq!(np.cover_path.as_deref(), Some(pochette));

    // Le flux passe au titre suivant : même zone, même station.
    let url_b = fausse_station_dite("/cp8452e", "Air - Kelly Watch the Stars").await;
    let repo = RadioRepo::with_backend(b.db.clone());
    let mut station = repo.get_by_url(&url_a).unwrap().expect("la station");
    station.url = url_b.clone();
    repo.update(&station).unwrap();
    let mut etat = b.playback.get_state(4).await;
    if let Some(np) = etat.now_playing.as_mut() {
        np.source_id = Some(url_b.clone());
    }
    b.playback.play(4, etat.now_playing.clone().unwrap()).await;
    let etat = b.playback.get_state(4).await;
    b.poller.refresh_radio_metadata(4, &etat).await;
    let np = b.playback.get_state(4).await.now_playing.unwrap();
    assert_eq!(np.title, "Kelly Watch the Stars");
    assert_eq!(
        np.cover_path.as_deref(),
        Some(LOGO),
        "le logo, pas la pochette d'avant"
    );
    assert!(!np.cover_plus);
}

/// Fil 3095 (rc13.90, DLNA) : on passe d'une station A affichée en Cover+ à
/// une station B. Le tour du sondeur qui avait lu la zone AVANT le
/// changement (son instantané porte A) rend ses métadonnées APRÈS : elles
/// ne doivent pas reposer A par-dessus B. L'écran doit montrer B.
#[tokio::test]
async fn changement_de_station_le_tour_en_vol_ne_repose_pas_l_ancienne_3095() {
    let b = banc();
    let zone_id = 5;
    let nom_a = "Morow 3095";
    let url_a = fausse_station_dite("/cp3095a", "Air - Sexy Boy").await;
    station_en_base(&b.db, nom_a, &url_a);
    let nom_b = "Radio DY10 3095";
    let url_b = fausse_station_dite("/cp3095b", "Daft Punk - Da Funk").await;
    station_en_base(&b.db, nom_b, &url_b);
    crate::cover_plus::regler_globalement(&b.db, true).unwrap();
    let pochette_a = "https://is1.mzstatic.example/air/600x600bb.jpg";
    crate::cover_plus::semer(
        Some("France"),
        nom_a,
        "Air",
        "Sexy Boy",
        Some(trouvaille(pochette_a)),
    );

    // A joue, en Cover+.
    let np_a = jouer_puis_sonder(&b, zone_id, nom_a, &url_a).await;
    assert!(np_a.cover_plus, "A s'affiche en Cover+");

    // Le flux de A change de titre ; le tour suivant lit la zone (A)…
    let mut ancien = b.playback.get_state(zone_id).await;
    if let Some(np) = ancien.now_playing.as_mut() {
        // … avec un titre d'avant, pour que le tour ait quelque chose à écrire.
        np.title = "Titre précédent".into();
    }

    // … puis l'auditeur passe à B avant que ce tour n'écrive.
    b.playback
        .play(
            zone_id,
            NowPlaying {
                title: nom_b.into(),
                album_title: Some(nom_b.into()),
                source: "radio".into(),
                source_id: Some(url_b.clone()),
                ..Default::default()
            },
        )
        .await;

    // Le tour en vol rend ses métadonnées de A.
    b.poller.refresh_radio_metadata(zone_id, &ancien).await;

    let np = b
        .playback
        .get_state(zone_id)
        .await
        .now_playing
        .expect("la zone joue");
    assert_eq!(
        np.source_id.as_deref(),
        Some(url_b.as_str()),
        "la zone doit rester sur la station B, pas revenir à A"
    );
    assert_ne!(np.title, "Sexy Boy", "le titre de A ne doit pas s'afficher");
    assert_ne!(
        np.cover_path.as_deref(),
        Some(pochette_a),
        "la pochette Cover+ de A ne doit pas rester"
    );
    assert!(!np.cover_plus);

    // Le tour suivant, lui, lit B et l'affiche.
    let etat = b.playback.get_state(zone_id).await;
    b.poller.refresh_radio_metadata(zone_id, &etat).await;
    let np = b.playback.get_state(zone_id).await.now_playing.unwrap();
    assert_eq!(np.title, "Da Funk");
    assert_eq!(np.album_title.as_deref(), Some(nom_b));
}
