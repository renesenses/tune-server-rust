//! #7974 — en fin de bail d'écoute, la vraie fin de piste du sondeur arrête
//! la zone : une seule tentative d'avance, refusée, aucune piste « sautée ».
//!
//! Avant le correctif, le refus du bail remontait dans la boucle de reprises
//! d'`avancer_avec_reprises` comme une piste en échec : un
//! `playback.track_skipped` par ligne restante de la file, jusqu'à la fin de
//! la file ou au 25e essai.
use super::lire_ensuite_dans_la_fenetre_gapless::Banc;
use crate::db::politique_parentale::{PolitiqueParentale, poser};
use crate::playback::PlayState;

/// Profil des sélections d'une base vierge : celui qui écoute sur une zone
/// sans profil de session.
const PROFIL: i64 = 1;

#[tokio::test]
async fn fin_de_bail_en_fin_de_piste_arrete_sans_sauter_7974() {
    let banc = Banc::monter().await;
    poser(
        &banc.db,
        PROFIL,
        PROFIL,
        0,
        &PolitiqueParentale {
            budget_quotidien_minutes: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    let mut bus = banc.poller.event_bus.as_ref().unwrap().subscribe();
    let mut lecture = banc.playback.subscribe();

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(
        (etat.queue_position, etat.queue_length),
        (0, 3),
        "prémisse : deux pistes restent en file après la courante"
    );
    banc.poller.handle_track_end(banc.zone_id, &etat).await;

    let mut sautees = Vec::new();
    while let Ok(ev) = bus.try_recv() {
        if ev.event_type == "playback.track_skipped" {
            sautees.push(ev.data);
        }
    }
    assert!(
        sautees.is_empty(),
        "le refus du bail n'est pas une piste en échec : aucune piste sautée, \
         reçu {sautees:?}"
    );
    let mut fins = 0;
    while let Ok(ev) = lecture.try_recv() {
        if ev.event == "parental_lease_ended" {
            fins += 1;
        }
    }
    assert_eq!(fins, 1, "la fin du bail est annoncée une fois aux clients");
    assert!(
        banc.play_complets().await.is_empty(),
        "rien n'est parti au renderer"
    );
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Stopped,
        "la zone est arrêtée"
    );
}
