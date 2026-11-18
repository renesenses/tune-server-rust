//! #8983 (fil 2890, rc12 Linux, sortie locale ALSA) — un clic sur la barre de
//! progression : la barre va à la position, mais le son repart du DÉBUT et la
//! piste suivante arrive trop tôt.
//!
//! Le saut d'une sortie locale relance la piste à la position
//! (`replay_zone_at_position`) : le flux recréé doit COMMENCER à cette
//! position. Ces témoins le lisent dans les octets servis, sur un FLAC dont
//! chaque seconde porte une valeur distincte (seconde n → échantillons à
//! `MARQUE * (n + 1)`) : le premier échantillon rendu dit d'où part le son.
//! Aucune carte son : sortie locale factice, flux lu par le témoin.
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::mock::MockOutput;
use std::sync::Arc;
use std::time::Duration;

const SR: u32 = 44_100;
const SECONDES: u32 = 6;
const MARQUE: i16 = 1000;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// PCM 16 bits stéréo : la seconde `n` vaut `MARQUE * (n + 1)` partout.
fn pcm_marque() -> Vec<u8> {
    let mut pcm = Vec::with_capacity((SR * SECONDES * 4) as usize);
    for s in 0..SECONDES {
        let v = (MARQUE * (s as i16 + 1)).to_le_bytes();
        for _ in 0..SR * 2 {
            pcm.extend_from_slice(&v);
        }
    }
    pcm
}

/// Un AIFF 16 bits stéréo (gros-boutiste) des mêmes marques.
fn aiff_marque() -> Vec<u8> {
    let pcm_le = pcm_marque();
    let mut pcm_be = Vec::with_capacity(pcm_le.len());
    for e in pcm_le.chunks_exact(2) {
        pcm_be.extend_from_slice(&[e[1], e[0]]);
    }
    let trames = (SR * SECONDES) as u32;
    let mut comm = Vec::new();
    comm.extend_from_slice(&2u16.to_be_bytes());
    comm.extend_from_slice(&trames.to_be_bytes());
    comm.extend_from_slice(&16u16.to_be_bytes());
    // 44 100 Hz en flottant étendu 80 bits.
    comm.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
    let mut ssnd = vec![0u8; 8];
    ssnd.extend_from_slice(&pcm_be);
    let mut corps = b"AIFF".to_vec();
    corps.extend_from_slice(b"COMM");
    corps.extend_from_slice(&(comm.len() as u32).to_be_bytes());
    corps.extend_from_slice(&comm);
    corps.extend_from_slice(b"SSND");
    corps.extend_from_slice(&(ssnd.len() as u32).to_be_bytes());
    corps.extend_from_slice(&ssnd);
    let mut f = b"FORM".to_vec();
    f.extend_from_slice(&(corps.len() as u32).to_be_bytes());
    f.extend_from_slice(&corps);
    f
}

async fn montage(zone_id: i64) -> (PlaybackOrchestrator, String, tempfile::TempDir) {
    montage_au_format(zone_id, "flac").await
}

async fn montage_au_format(
    zone_id: i64,
    format: &str,
) -> (PlaybackOrchestrator, String, tempfile::TempDir) {
    montage_sur(zone_id, format, "local").await
}

/// `sortie` : `local` (carte son ALSA de la machine) ou `oaat` (récepteur
/// OAAT, qui reçoit le même WAV décodé, `oaat_needs_wav`).
async fn montage_sur(
    zone_id: i64,
    format: &str,
    sortie: &str,
) -> (PlaybackOrchestrator, String, tempfile::TempDir) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join(format!("marque-8983.{format}"));
    let octets = if format == "aiff" {
        aiff_marque()
    } else {
        let mut enc = crate::audio::encoder::AudioEncoder::new(format, SR, 16, 2);
        enc.start().await.unwrap();
        enc.write(&pcm_marque()).await.unwrap();
        enc.finish().await.unwrap()
    };
    std::fs::write(&piste, octets).unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'Marque', 1, 1, ?, '{format}', {}, {SR}, 16, 2)",
                SECONDES as i64 * 1000
            ),
            &[&chemin as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let appareil = format!("{sortie}:saut-8983-{zone_id}");
    orch.db
        .execute(
            &format!(
                "INSERT INTO zones (id, name, output_type, output_device_id) \
                 VALUES ({zone_id}, 'Sortie factice', '{sortie}', ?)"
            ),
            &[&appareil as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(&appareil, "Sortie factice").with_type(sortie),
    ));
    orch.play(PlayRequest {
        zone_id,
        output_device_id: Some(appareil.clone()),
        track_id: Some(1),
        source: Some("local".into()),
        ..Default::default()
    })
    .await
    .expect("lecture factice");
    (orch, appareil, dir)
}

/// Les octets PCM (après l'en-tête WAV) de la session installée sur la zone,
/// avec le format servi.
async fn octets_servis(orch: &PlaybackOrchestrator, zone_id: i64) -> (Vec<u8>, u16, u16, i64) {
    let etat = orch.playback.get_state(zone_id).await;
    let np = etat.now_playing.expect("piste installée");
    let sid = np.stream_id.expect("session installée");
    let info = orch
        .streamer
        .stream_output_wire(&sid)
        .await
        .expect("format servi");
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session présente");
    let octets = tokio::time::timeout(Duration::from_secs(10), async {
        let mut octets = Vec::new();
        while let Some(bloc) = session.recv_chunk().await {
            octets.extend_from_slice(&bloc);
        }
        octets
    })
    .await
    .expect("le décodeur finit le fichier synthétique");
    assert_eq!(&octets[..4], b"RIFF", "flux WAV");
    let data = octets
        .windows(4)
        .position(|w| w == b"data")
        .expect("bloc data")
        + 8;
    (
        octets[data..].to_vec(),
        info.bit_depth,
        info.channels as u16,
        etat.position_ms,
    )
}

/// La marque (seconde de la piste) du premier et du dernier échantillon.
fn secondes_lues(pcm: &[u8], bits: u16) -> (i64, i64, usize) {
    let largeur = usize::from(bits / 8);
    let lire = |i: usize| -> i64 {
        let b = &pcm[i..i + largeur];
        let v: i32 = match largeur {
            2 => i32::from(i16::from_le_bytes([b[0], b[1]])),
            3 => i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 16,
            4 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 16,
            _ => panic!("largeur {largeur}"),
        };
        (f64::from(v) / f64::from(MARQUE)).round() as i64 - 1
    };
    (lire(0), lire(pcm.len() - largeur), pcm.len())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depuis_le_debut_le_son_part_de_la_seconde_zero_8983() {
    let (orch, appareil, _dir) = montage(898301).await;
    let (pcm, bits, canaux, _) = octets_servis(&orch, 898301).await;
    let (premiere, derniere, n) = secondes_lues(&pcm, bits);
    assert_eq!(premiere, 0, "prémisse : la lecture part du début");
    assert_eq!(derniere, i64::from(SECONDES) - 1);
    let octets_par_seconde = SR as usize * usize::from(canaux) * usize::from(bits / 8);
    assert_eq!(n, octets_par_seconde * SECONDES as usize);
    orch.stop(898301, Some(&appareil)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_un_saut_le_son_part_de_la_position_demandee_8983() {
    let (orch, appareil, _dir) = montage(898302).await;
    let _ = octets_servis(&orch, 898302).await;
    orch.seek(898302, 3000, Some(&appareil))
        .await
        .expect("déplacement");
    let (pcm, bits, canaux, position) = octets_servis(&orch, 898302).await;
    assert_eq!(position, 3000, "la position de la zone est la cible");
    let (premiere, derniere, n) = secondes_lues(&pcm, bits);
    assert_eq!(
        premiere, 3,
        "#8983 : le premier échantillon servi après un saut à 3 s doit être \
         celui de la seconde 3, pas celui du début de la piste"
    );
    assert_eq!(
        derniere,
        i64::from(SECONDES) - 1,
        "la fin réelle est servie"
    );
    let octets_par_seconde = SR as usize * usize::from(canaux) * usize::from(bits / 8);
    assert_eq!(
        n,
        octets_par_seconde * 3,
        "trois secondes restent à servir : la fin tombe au bon moment"
    );
    orch.stop(898302, Some(&appareil)).await;
}

/// Le décodeur en flux, seul : les octets PCM rendus pour `seek_s`.
async fn pcm_du_decodeur(chemin: &str, cadence: u32, seek_s: f64) -> Vec<u8> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1024);
    let (ltx, _lrx) = tokio::sync::mpsc::unbounded_channel();
    let chemin = chemin.to_owned();
    let tache = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_tranche(
            &chemin,
            Some(cadence),
            Some(2),
            Some(32),
            tx,
            32768,
            Arc::new(tokio::sync::Notify::new()),
            ltx,
            seek_s,
            None,
        )
    });
    let mut octets = Vec::new();
    while let Some(bloc) = rx.recv().await {
        octets.extend_from_slice(&bloc);
    }
    tache.await.unwrap().expect("décodage");
    let data = octets.windows(4).position(|w| w == b"data").unwrap() + 8;
    octets[data..].to_vec()
}

/// Les FLAC du banc portent une SEEKTABLE, comme ceux d'un service ou d'un
/// encodeur courant : le départ décalé doit rendre exactement la suite du
/// départ à zéro.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_decodeur_part_bien_de_la_position_sur_un_flac_a_seektable_8983() {
    let racine = env!("CARGO_MANIFEST_DIR");
    for (nom, cadence) in [
        ("ref_16_44100_stereo.flac", 44_100u32),
        ("ref_24_96000_stereo.flac", 96_000u32),
    ] {
        let chemin = format!("{racine}/tests/fixtures/flac/{nom}");
        let entier = pcm_du_decodeur(&chemin, cadence, 0.0).await;
        let seek_s = 0.05;
        let decale = pcm_du_decodeur(&chemin, cadence, seek_s).await;
        let decalage = (seek_s * f64::from(cadence)) as usize * 2 * 4;
        assert!(!decale.is_empty(), "{nom} : rien rendu après le saut");
        assert_eq!(
            &decale[..64.min(decale.len())],
            &entier[decalage..decalage + 64.min(decale.len())],
            "{nom} : le premier échantillon après le saut n'est pas celui de la position"
        );
        assert_eq!(decale.len(), entier.len() - decalage, "{nom} : longueur");
    }
}

/// Chaque conteneur que la sortie locale reçoit décodé : le saut doit partir
/// de la position pour tous.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_un_saut_chaque_format_part_de_la_position_8983() {
    for (i, format) in ["flac", "wav", "aiff"].into_iter().enumerate() {
        let zone = 898310 + i as i64;
        let (orch, appareil, _dir) = montage_au_format(zone, format).await;
        let _ = octets_servis(&orch, zone).await;
        orch.seek(zone, 3000, Some(&appareil))
            .await
            .expect("déplacement");
        let (pcm, bits, canaux, _) = octets_servis(&orch, zone).await;
        let (premiere, derniere, n) = secondes_lues(&pcm, bits);
        let octets_par_seconde = SR as usize * usize::from(canaux) * usize::from(bits / 8);
        assert_eq!(
            (premiere, derniere, n),
            (3, i64::from(SECONDES) - 1, octets_par_seconde * 3),
            "{format} : après un saut à 3 s, le flux doit servir les secondes 3 à 5"
        );
        orch.stop(zone, Some(&appareil)).await;
    }
}

/// Fil 2890 : le testeur joue sur sa carte S/PDIF en ALSA « protocole OAAT ».
/// Même témoin sur une zone OAAT : le flux relancé commence à la position
/// (la sortie OAAT reçoit cette position par `set_pending_start_position_ms`
/// et compte en absolu, témoin #5978 de `outputs/oaat/integration_test.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_un_saut_une_zone_oaat_recoit_le_flux_a_la_position_8983() {
    for (i, format) in ["flac", "aiff"].into_iter().enumerate() {
        let zone = 898320 + i as i64;
        let (orch, appareil, _dir) = montage_sur(zone, format, "oaat").await;
        let _ = octets_servis(&orch, zone).await;
        orch.seek(zone, 3000, Some(&appareil))
            .await
            .expect("déplacement");
        let (pcm, bits, canaux, position) = octets_servis(&orch, zone).await;
        assert_eq!(position, 3000);
        let (premiere, derniere, n) = secondes_lues(&pcm, bits);
        let octets_par_seconde = SR as usize * usize::from(canaux) * usize::from(bits / 8);
        assert_eq!(
            (premiere, derniere, n),
            (3, i64::from(SECONDES) - 1, octets_par_seconde * 3),
            "OAAT, {format} : après un saut à 3 s, le flux doit servir les secondes 3 à 5"
        );
        orch.stop(zone, Some(&appareil)).await;
    }
}
