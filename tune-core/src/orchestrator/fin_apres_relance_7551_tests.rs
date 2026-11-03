//! #7551 (JeanPitre, fil 2566, rc9 Windows, WASAPI exclusif) — après une
//! relance à une position, la piste locale prenait pour durée le temps qui
//! RESTE, alors que la sortie locale rapporte une position ABSOLUE.
//!
//! Le journal du rapport :
//!
//! ```text
//! 20:53:58.550 seek_local_output_recreating_stream position_ms=187861
//! 20:53:59.469 gapless_arm_trace reported_duration_ms=116338 queue_duration_ms=116338 position_ms=190065
//! 20:54:04.477 position_past_end_advancing position_ms=195081 track_dur=116338 past_end_ticks=3
//! 20:54:04.479 auto_next … title=On Every Street duration_ms=116338
//! ```
//!
//! 116 338 = 304 200 − 187 862 : 109 s coupées. Deux grandeurs portent cette
//! durée : celle du `NowPlaying` (que lit le sondeur, #7360) et celle que
//! `play_media` remet à la sortie (qu'elle rapporte ensuite dans son statut).
//! Les témoins ci-dessous tiennent la seconde, et rejouent la décision du
//! sondeur avec les chiffres du journal.

use super::duree_convertie_6630_tests::{constater, montage};
use crate::orchestrator::ResolvedStream;
use crate::outputs::mock::MockOutput;

async fn duree_recue_par_la_sortie(
    orch: &crate::orchestrator::PlaybackOrchestrator,
    appareil: &str,
) -> Option<u64> {
    let arc = orch
        .outputs
        .lock()
        .await
        .get(appareil)
        .expect("sortie factice inscrite");
    let sortie = arc.lock().await;
    let mock = sortie
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("sortie factice");
    mock.last_play_duration_ms().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_deplacement_la_sortie_locale_recoit_la_duree_de_la_piste_7551() {
    let (orch, zone, appareil, _dir) = montage(755101).await;
    let _ = constater(&orch, zone).await;
    assert_eq!(
        duree_recue_par_la_sortie(&orch, &appareil).await,
        Some(6000),
        "prémisse : depuis le début, la sortie reçoit la piste entière"
    );

    orch.seek(zone, 3000, Some(&appareil))
        .await
        .expect("déplacement");
    let (np, info, _, position) = constater(&orch, zone).await;
    assert_eq!(position, 3000);
    assert_eq!(
        info.duration_ms,
        Some(3000),
        "le flux sert toujours le temps restant (#6630)"
    );
    assert_eq!(np.duration_ms, 6000, "le NowPlaying garde la piste (#7360)");
    assert_eq!(
        duree_recue_par_la_sortie(&orch, &appareil).await,
        Some(6000),
        "#7551 : la sortie locale compte en position absolue, elle doit \
         recevoir la durée de la piste, pas les 3 s restantes"
    );
    orch.stop(zone, Some(&appareil)).await;
}

fn flux(duree_ms: i64, session: &str) -> ResolvedStream {
    ResolvedStream {
        url: format!("http://127.0.0.1:8888/stream/{session}.wav"),
        mime_type: "audio/wav".into(),
        title: "On Every Street".into(),
        artist: None,
        album: None,
        duration_ms: Some(duree_ms),
        source: "local".into(),
        cover_url: None,
        stream_id: Some(session.into()),
        file_size: None,
        sample_rate: Some(44_100),
        bit_depth: Some(32),
        channels: Some(2),
        origin_url: None,
        bitrate_kbps: None,
        format_source: None,
    }
}

#[test]
fn une_sortie_reseau_garde_la_duree_servie_7551() {
    let session = "temoin-7551-depart-187862";
    super::noter_depart_pcm_decode(session, 187_862);
    let relance = flux(116_338, session);
    assert_eq!(
        super::super::transport::duree_annoncee_a_la_sortie(&relance, true),
        Some(304_200),
        "sortie locale : la piste entière"
    );
    assert_eq!(
        super::super::transport::duree_annoncee_a_la_sortie(&relance, false),
        Some(116_338),
        "renderer réseau : il compte depuis le début du flux neuf, sa DIDL dit ce qui reste"
    );
    let sans_depart = flux(304_200, "temoin-7551-sans-depart");
    assert_eq!(
        super::super::transport::duree_annoncee_a_la_sortie(&sans_depart, true),
        Some(304_200),
        "sans relance, rien ne change"
    );
}

/// La décision du sondeur (`poller/tick.rs`, `effective_end_duration_ms` puis
/// `past_end_reached`), avec les chiffres du journal de JeanPitre.
#[test]
fn le_journal_de_jeanpitre_coupe_avec_le_temps_restant_pas_avec_la_piste_7551() {
    use crate::poller::decisions::{past_end_reached, sane_current_duration};
    let duree_fin = |piste: u64, rapportee: u64| piste.max(sane_current_duration(rapportee, piste));
    let position = 195_081;
    assert!(
        past_end_reached(duree_fin(116_338, 116_338), true, position),
        "prémisse : la durée restante fait passer au suivant à 195 s"
    );
    assert!(
        !past_end_reached(duree_fin(304_200, 304_200), true, position),
        "avec la durée de la piste, 195 s sur 304 s n'est pas une fin"
    );
    assert!(
        past_end_reached(duree_fin(304_200, 304_200), true, 304_200 + 3_000),
        "la vraie fin reste détectée"
    );
}
