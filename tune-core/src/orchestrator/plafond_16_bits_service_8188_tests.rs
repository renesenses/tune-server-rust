//! #8188 — Ruark R3 et Roberts Stream 94i+ (DLNA, puces Frontier Silicon),
//! « Limiter à 16 bits » coché : un FLAC Qobuz 24/96 ne produisait AUCUN son
//! (Sevy Tabroc, fil 2714, captures du 09/11/2026 : Qobuz, FLAC 96/24).
//!
//! Le plafond 16 bits n'était honoré que par la bibliothèque locale et par le
//! bras DASH (#6909) : le bras HTTPS des services relayait les octets du CDN
//! VERBATIM, en 24 bits, vers un renderer qui ne décode que 16 bits.
//!
//! Témoins par le bras réel (`relayer_le_flux`), sans réseau : le « CDN » est
//! un serveur en boucle locale qui sert un vrai FLAC 24/96 ; le renderer est
//! une zone DLNA dont l'appareil n'est pas inscrit (la sonde `audio/flac`
//! répond « oui », comme le Sink d'un Ruark R3).
use super::*;
use crate::db::zone_repo::ZoneRepo;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

fn flac_24_96() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/flac/ref_24_96000_stereo.flac"
    ))
    .unwrap()
}

struct Servi {
    url: String,
    sid: String,
    mime: String,
    taille: Option<u64>,
}

/// Une lecture Qobuz 24/96 vers une zone DLNA « Ruark R3 », plafond 16 bits
/// coché ou non, par le bras HTTPS des services.
async fn lire_qobuz_24_96(
    orch: &PlaybackOrchestrator,
    plafond_16: bool,
) -> (Servi, tokio::task::JoinHandle<()>) {
    let flac = flac_24_96();
    let app = axum::Router::new().route(
        "/piste.flac",
        axum::routing::get(move || {
            let flac = flac.clone();
            async move { ([("Content-Type", "audio/flac")], flac) }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/piste.flac", ecoute.local_addr().unwrap());
    let serveur = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });

    let zones = ZoneRepo::with_backend(orch.db.clone());
    let zone = zones
        .create(
            "Ruarkaudio R3",
            Some("dlna"),
            Some("uuid:3DCC7100-F76C-11DD-87AF-8188"),
        )
        .unwrap();
    zones.update_dlna_cap_16bit(zone, plafond_16).unwrap();

    let req = super::service_wav_progressif_5080::requete_qobuz(zone);
    let flux = super::service_wav_progressif_5080::flux_qobuz(url, 96_000, 24);
    let (url, sid, mime, taille) = orch
        .relayer_le_flux(
            &req,
            "8188",
            "qobuz",
            &flux,
            super::service_wav_progressif_5080::info_amont(&flux),
            true,
            "flac".into(),
            None,
            None,
        )
        .await
        .expect("la piste se résout");
    (
        Servi {
            url,
            sid: sid.expect("une session"),
            mime,
            taille,
        },
        serveur,
    )
}

/// 🔴 Le témoin du défaut : zone plafonnée à 16 bits, FLAC Qobuz 24/96. Ce
/// qui part vers le renderer doit être un FLAC 16 bits à 96 kHz (le plafond
/// de profondeur ne touche pas la cadence), servi depuis un FICHIER de taille
/// connue (`Content-Length`, Range), et annoncé 16 bits dans le DIDL.
///
/// Sabotage qui rend ce témoin ROUGE : passer `false` au lieu de
/// `plafond_16` en quatrième argument de `streaming_needs_pretranscode`
/// (`resolve_stream.rs`) — le flux repart en proxy verbatim, 24 bits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_flac_qobuz_24_96_part_en_16_bits_vers_une_zone_plafonnee_8188() {
    let orch = orchestrateur();
    let (servi, serveur) = lire_qobuz_24_96(&orch, true).await;

    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&servi.sid)
        .cloned()
        .expect("session inscrite");
    let chemin = session.file_path.lock().await.clone().expect(
        "#8188 : zone plafonnée à 16 bits — le FLAC 24 bits du CDN est parti en proxy \
         VERBATIM (aucun fichier transcodé) : un Ruark R3 joue du silence",
    );
    assert_eq!(servi.mime, "audio/flac", "le renderer annonce audio/flac");
    assert!(
        servi.url.ends_with(".flac"),
        "URL servie au renderer : {}",
        servi.url
    );

    // Taille annoncée = taille du fichier servi : Content-Length exact.
    let sur_disque = std::fs::metadata(&chemin).unwrap().len();
    assert_eq!(
        servi.taille,
        Some(sur_disque),
        "la taille annoncée doit être celle du fichier (Content-Length)"
    );

    // Le contenu : 16 bits, 96 kHz, stéréo.
    let rendu = crate::audio::decode::decode_to_pcm(&chemin, None, Some(2), 0.0, 0.0).unwrap();
    assert_eq!(
        (rendu.bit_depth, rendu.sample_rate, rendu.channels),
        (16, 96_000, 2),
        "#8188 : le flux servi doit être un FLAC 16 bits / 96 kHz"
    );
    assert!(!rendu.pcm_bytes().is_empty());

    // L'annonce (DIDL `bitsPerSample`) suit le fil, pas le service.
    assert_eq!(
        orch.profondeur_servie_sur_le_fil(Some(&servi.sid), true)
            .await,
        Some(16),
        "#8188 : le DIDL doit annoncer 16 bits pour un flux de 16 bits"
    );

    orch.streamer.remove_session(&servi.sid).await;
    serveur.abort();
}

/// Contre-épreuve : la même piste vers une zone SANS plafond garde le proxy
/// verbatim, bit-perfect, 24 bits — le correctif ne dégrade personne.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_plafond_le_flac_24_96_reste_relaye_verbatim_8188() {
    let orch = orchestrateur();
    let (servi, serveur) = lire_qobuz_24_96(&orch, false).await;
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&servi.sid)
        .cloned()
        .expect("session inscrite");
    assert!(
        session.file_path.lock().await.is_none(),
        "sans plafond, le flux reste un proxy du CDN (aucun transcodage)"
    );
    assert_eq!(servi.taille, None);
    assert_eq!(
        orch.profondeur_servie_sur_le_fil(Some(&servi.sid), true)
            .await,
        Some(24),
        "sans plafond, l'annonce reste celle de la source"
    );
    orch.streamer.remove_session(&servi.sid).await;
    serveur.abort();
}

/// La règle pure : le plafond 16 bits SEUL impose le pré-transcodage.
///
/// Sabotage qui rend ce témoin ROUGE : retirer `|| plafond_16_bits` de
/// `streaming_needs_pretranscode`.
#[test]
fn le_plafond_16_bits_seul_impose_le_pretranscodage_8188() {
    use super::regles::streaming_needs_pretranscode;
    assert!(streaming_needs_pretranscode(true, false, false, true));
    assert!(!streaming_needs_pretranscode(true, false, false, false));
}

/// Hors sortie réseau, l'annonce ne change pas : la sortie locale lit
/// l'en-tête de son WAV.
#[tokio::test]
async fn hors_sortie_reseau_l_annonce_garde_la_profondeur_du_service_8188() {
    let orch = orchestrateur();
    assert_eq!(
        orch.profondeur_servie_sur_le_fil(Some("x"), false).await,
        None
    );
    assert_eq!(orch.profondeur_servie_sur_le_fil(None, true).await, None);
}
