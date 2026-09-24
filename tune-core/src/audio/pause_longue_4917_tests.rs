//! #4917 — une pause de plus de cinq minutes tronquait la piste en cours.
//!
//! Fil 1915 (Reivax66, Windows, sortie locale WASAPI partagée) : « R and R »
//! (11:52) s'arrête à 7:51. Le journal le dit, lu dans l'ordre :
//!
//! ```text
//! 14:50:24 taches_de_fond_ralenties_pour_la_lecture zone_id=19   <- REPRISE après une pause
//! 14:50:26 stream_delivery_stall … attente_transport_ms=1310218 channel_max=0 bytes_sent=157810688
//! 14:50:54 local_audio_gapless_read_error error=error decoding response body
//! 14:50:56 local_audio_track_ended_naturally_post_drain total_bytes_read=166064128
//! ```
//!
//! La sortie locale en pause (mode partagé) garde sa connexion HTTP et cesse
//! de lire (`LocalOutput::pause` ne pose qu'un drapeau). Le canal de la session
//! se remplit, le décodeur reste bloqué sur `tx.send` — et au bout de
//! `SEND_TIMEOUT_SECS` (300 s) il rendait la main, `Ok`, exactement comme à la
//! fin du fichier : `close_sender`, canal fermé (`channel_max=0`). Au retour,
//! la sortie vide ce qui restait dans le canal (8 253 440 octets = 23,4 s),
//! puis le corps s'arrête loin du `Content-Length` annoncé. 21 min 50 s
//! d'attente de transport (`1310218` ms), cinq de trop.
//!
//! La session, elle, survit une demi-heure sans un octet servi
//! (`SESSION_IDLE_TIMEOUT`, #2536 : « une lecture EN PAUSE ne tire plus rien du
//! serveur »). Le producteur abandonnait donc une session que le
//! ramasse-miettes gardait encore.
use super::*;

/// La garde rapide : le producteur ne lâche pas avant le ramasse-miettes.
///
/// Rouge sur `origin/main` (300 s contre 1 800 s).
#[test]
fn le_producteur_ne_lache_pas_une_session_que_le_ramasse_miettes_garde() {
    let delai_du_producteur = std::time::Duration::from_secs(SEND_TIMEOUT_SECS);
    let vie_d_une_session_inactive = crate::http::streamer::SESSION_IDLE_TIMEOUT;
    assert!(
        delai_du_producteur >= vie_d_une_session_inactive,
        "le décodeur abandonne au bout de {delai_du_producteur:?} un lecteur qui ne \
         lit plus, alors qu'une session inactive vit {vie_d_une_session_inactive:?} : \
         une pause plus longue que le premier et plus courte que la seconde \
         tronque la piste à la reprise (#4917, fil 1915)"
    );
}

const TAUX: u32 = 44_100;

/// Un vrai WAV 16 bits stéréo de `secondes`, sinusoïde grossière.
fn ecrire_wav(chemin: &std::path::Path, secondes: u32) {
    let trames = TAUX * secondes;
    let octets = trames * 4;
    let mut f = Vec::with_capacity(44 + octets as usize);
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + octets).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&TAUX.to_le_bytes());
    f.extend_from_slice(&(TAUX * 4).to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&octets.to_le_bytes());
    for n in 0..trames {
        let v = ((n as f32 / 40.0).sin() * 8000.0) as i16;
        f.extend_from_slice(&v.to_le_bytes());
        f.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(chemin, f).unwrap();
}

/// LA PREUVE LONGUE, le scénario du fil 1915 à l'échelle : ~5 min 15 s.
///
/// Même chaîne que la sortie locale : `decode_to_pcm_streaming_tranche`,
/// canal de 256 blocs de 32 Kio (`resolve_local`, `create_session(…, 256)`),
/// sortie 44,1 kHz 32 bits stéréo — 8 octets par trame, ceux du journal. Le
/// lecteur prend 10 s, se met en pause 310 s (plus que l'ancien délai de
/// 300 s, moins que les 30 min d'une session), puis lit jusqu'au bout.
///
/// Sur `origin/main` : 10 s lues, puis le reliquat du canal, et le flux se
/// ferme — environ 34 s servies sur 60. Corrigé : les 60 s entières.
///
/// `cargo test -p tune-core --lib pause_longue_4917 -- --ignored`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "preuve longue : 5 min 15 s de pause réelle"]
async fn une_pause_de_plus_de_cinq_minutes_ne_tronque_plus_la_piste() {
    const DUREE_S: u32 = 60;
    const OCTETS_PAR_TRAME: usize = 8;
    const PAUSE_S: u64 = 310;
    let d = tempfile::TempDir::new().unwrap();
    let f = d.path().join("r-and-r.wav");
    ecrire_wav(&f, DUREE_S);

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
    let pret = std::sync::Arc::new(tokio::sync::Notify::new());
    let (niveaux, _niveaux_rx) = tokio::sync::mpsc::unbounded_channel();
    let fp = f.to_string_lossy().to_string();
    let tache = tokio::task::spawn_blocking(move || {
        decode_to_pcm_streaming_tranche(
            &fp,
            Some(TAUX),
            Some(2),
            Some(32),
            tx,
            32768,
            pret,
            niveaux,
            0.0,
            None,
        )
    });

    let avant_la_pause = 44 + 10 * TAUX as usize * OCTETS_PAR_TRAME;
    let mut lus = 0usize;
    while lus < avant_la_pause {
        lus += rx
            .recv()
            .await
            .expect("le flux s'est fermé avant la pause")
            .len();
    }
    // La pause : on garde le récepteur, on ne lit plus.
    tokio::time::sleep(std::time::Duration::from_secs(PAUSE_S)).await;
    while let Some(bloc) = rx.recv().await {
        lus += bloc.len();
    }
    let _ = tache.await;

    let attendu = 44 + (DUREE_S * TAUX) as usize * OCTETS_PAR_TRAME;
    assert_eq!(
        lus,
        attendu,
        "après une pause de {PAUSE_S} s, {lus} octets servis sur {attendu} : {:.1} s de \
         musique sur {DUREE_S} — le décodeur a lâché la session pendant la pause \
         et le flux s'est fermé comme sur une fin de fichier (#4917)",
        lus.saturating_sub(44) as f64 / (TAUX as usize * OCTETS_PAR_TRAME) as f64
    );
}
