//! #6017 — fil 2194 (Cyrille, 1.0.0-rc3, Yamaha R-N2000A en DLNA) : avancer
//! dans une piste de la BIBLIOTHÈQUE échoue avec « code UPnP inconnu », alors
//! que le même geste marche sur une piste Qobuz.
//!
//! Journal du 09/10 : la piste AAC est convertie à la volée en WAV
//! (`transcode_required … target=Wav`), servie par un canal (`voie="conversion"`)
//! et annoncée `DLNA.ORG_OP=00` — pas de recherche par octets. Tune envoyait
//! pourtant un `Seek` SOAP nu, que le Yamaha refusait (`dlna_command_finished
//! action="Seek" outcome="soap_fault"`). La piste Qobuz, elle, passe par le
//! mandataire (`seek_streaming_direct_on_seekable_session`), cherchable.
//!
//! Le banc : une piste de la bibliothèque qui joue sur une zone DLNA par un
//! canal sans `Range` (la session de l'AAC converti), un vrai fichier pour la
//! relecture, et une sortie factice qui REFUSE tout `Seek`, comme le Yamaha
//! sur ce flux.
//!
//! Contre-épreuve : retirer la branche `bibliotheque_sans_range_sur_reseau`
//! de `deplacer_la_sortie` fait tomber
//! `avancer_dans_une_piste_convertie_recree_le_flux_a_la_position`.
use super::PlaybackOrchestrator;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

const SR: u32 = 44_100;
const SECONDES: u32 = 6;
const APPAREIL: &str = "dlna:uuid-9ab0c000-r-n2000a-6017";
const CIBLE_MS: u64 = 3_000;

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

/// `(play_media reçus, cibles des seek reçus)`.
async fn recu_par_la_sortie(orch: &PlaybackOrchestrator) -> (usize, Vec<u64>) {
    let arc = { orch.outputs.lock().await.get(APPAREIL) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (mock.play_call_count().await, mock.seek_calls())
}

/// Une piste de la bibliothèque qui joue sur la zone DLNA, servie — comme
/// l'AAC converti de Cyrille (`voie="conversion"`) — par un CANAL, sans `Range`.
///
/// La session de départ est posée à la main : la conversion à la volée d'un
/// AAC demande un encodeur AAC que le banc n'a pas. La relecture, elle, passe
/// par la vraie résolution, sur un vrai fichier.
async fn piste_convertie_en_lecture() -> (PlaybackOrchestrator, i64, tempfile::TempDir) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("1-02 Is It Like Today.flac");
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    enc.write(&vec![0u8; (SR * SECONDES * 4) as usize])
        .await
        .unwrap();
    std::fs::write(&piste, enc.finish().await.unwrap()).unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    orch.db
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'World Party')",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Bang!', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'Is It Like Today?', 1, 1, ?, 'flac', {}, {SR}, 16, 2)",
                SECONDES as i64 * 1000
            ),
            &[&chemin as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Pièce par défaut", Some("dlna"), Some(APPAREIL))
        .unwrap();
    let sortie = MockOutput::new(APPAREIL, "R-N2000A").with_type("dlna");
    // Le Yamaha refuse le `Seek` sur un flux annoncé `DLNA.ORG_OP=00`.
    sortie.refuser_le_seek(Some(
        "seek refusé par le renderer: code UPnP inconnu (commande refusée)",
    ));
    orch.outputs.lock().await.register(Box::new(sortie));

    let (sid, _tx, _pret) = orch
        .streamer
        .create_session(
            crate::http::streamer::StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: SR,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            false,
            256,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            crate::playback::NowPlaying {
                title: "Is It Like Today?".into(),
                track_id: Some(1),
                source: "local".into(),
                stream_id: Some(sid),
                duration_ms: SECONDES as i64 * 1000,
                ..Default::default()
            },
        )
        .await;
    (orch, zone_id, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn avancer_dans_une_piste_convertie_recree_le_flux_a_la_position() {
    let (orch, zone_id, _dir) = piste_convertie_en_lecture().await;
    let np = orch.playback.get_state(zone_id).await.now_playing.unwrap();
    let sid = np.stream_id.clone().expect("un flux porte la piste");
    assert_eq!(np.source, "local", "prémisse : piste de la bibliothèque");
    assert!(
        !orch.streamer.is_seekable_session(&sid).await,
        "prémisse : la piste est convertie et servie par un canal, sans Range"
    );

    let resultat = orch.seek(zone_id, CIBLE_MS, Some(APPAREIL)).await;
    assert!(
        resultat.is_ok(),
        "avancer dans une piste convertie ne doit pas finir sur le refus du \
         renderer (fil 2194, « code UPnP inconnu ») : {resultat:?}"
    );
    let (lectures, seeks) = recu_par_la_sortie(&orch).await;
    assert_eq!(
        lectures, 1,
        "le flux doit être recréé à la position (un play_media)"
    );
    assert!(
        seeks.is_empty(),
        "aucun Seek SOAP nu ne doit partir vers le flux sans Range : {seeks:?}"
    );
    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(
        etat.position_ms, CIBLE_MS as i64,
        "la position suit le geste"
    );
    let nouveau = etat.now_playing.unwrap().stream_id.unwrap();
    assert_ne!(
        nouveau, sid,
        "une nouvelle session porte le flux à la position"
    );
}
