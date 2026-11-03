//! #6119 — publier le parc local ne doit pas sonder le matériel pendant
//! qu'une sortie locale joue.
//!
//! Le journal de Gros Bidon (fil 2216) : `audio_stream_device_lost` sur
//! « Sortie SMSL SU-8 » (WASAPI) à 07:41:38.978, puis
//! `local_audio_host_selected backend="asio"` à 07:41:38.988 — la page Zones
//! venait de demander la fiche de diagnostic, qui énumérait les sorties ASIO.
//! Le banc compte les énumérations : une sortie `local:` en lecture, et
//! l'énumération ne doit pas avoir lieu ; la même sortie à l'arrêt, et elle
//! doit avoir lieu (sans quoi le banc ne mesurerait rien).
use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tune_core::outputs::traits::TransportState;

const SU8: &str = "local:Sortie SMSL SU-8";

async fn banc(etat_de_la_sortie: TransportState) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    let sortie = tune_core::outputs::mock::MockOutput::new(SU8, "Sortie SMSL SU-8");
    sortie.set_state(etat_de_la_sortie).await;
    state.outputs.lock().await.register(Box::new(sortie));
    state
}

async fn enumerations_pendant(state: &AppState) -> usize {
    let compteur = Arc::new(AtomicUsize::new(0));
    let c = compteur.clone();
    let _ = parc_local_sans_couper_la_lecture(state, || async move {
        c.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    })
    .await;
    compteur.load(Ordering::SeqCst)
}

#[tokio::test]
async fn une_sortie_locale_qui_joue_n_est_jamais_sondee_6119() {
    let state = banc(TransportState::Playing).await;
    assert_eq!(
        enumerations_pendant(&state).await,
        0,
        "« {SU8} » joue : énumérer les sorties (ASIO) maintenant coupe sa lecture \
         WASAPI (fil 2216) — le parc doit venir du cache"
    );
}

#[tokio::test]
async fn a_l_arret_le_parc_est_bien_sonde_6119() {
    let state = banc(TransportState::Stopped).await;
    assert_eq!(
        enumerations_pendant(&state).await,
        1,
        "rien ne joue : l'énumération doit avoir lieu, sinon un DAC branché \
         n'apparaîtrait jamais"
    );
}

/// #7263 : FIP joue sur le réseau, aucune sortie locale ne joue. Le rescan
/// périodique Linux évite déjà ALSA ; les lectures API du parc doivent aussi
/// s'en tenir au cache, puis retrouver le hotplug après l'arrêt.
#[tokio::test]
async fn une_radio_reseau_suspend_les_sondes_du_parc_sous_linux_7263() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    state
        .playback
        .play(
            10,
            tune_core::playback::NowPlaying {
                source: "radio".into(),
                title: "FIP".into(),
                ..Default::default()
            },
        )
        .await;
    let attendu = usize::from(!cfg!(target_os = "linux"));
    assert_eq!(
        enumerations_pendant(&state).await,
        attendu,
        "#7263 : FIP réseau joue ; sous Linux la lecture du parc sonde encore ALSA au lieu de servir le cache"
    );
    state.playback.stop(10).await;
    assert_eq!(
        enumerations_pendant(&state).await,
        1,
        "#7263 : à l'arrêt, le hotplug doit pouvoir reprendre"
    );
}

/// Même décision de production, système injecté : la règle Linux reste
/// vérifiable sur le Mac, sans prétendre y exécuter un pilote ALSA.
#[tokio::test]
async fn la_garde_reseau_linux_est_verifiee_sur_tous_les_hotes_7263() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    state
        .playback
        .play(
            10,
            tune_core::playback::NowPlaying {
                source: "radio".into(),
                title: "FIP".into(),
                ..Default::default()
            },
        )
        .await;
    for (os, attendu) in [("linux", 0), ("windows", 1), ("macos", 1)] {
        let compteur = Arc::new(AtomicUsize::new(0));
        let appels = compteur.clone();
        parc_local_sans_couper_la_lecture_selon_os(&state, os, || async move {
            appels.fetch_add(1, Ordering::SeqCst);
            Vec::new()
        })
        .await;
        assert_eq!(
            compteur.load(Ordering::SeqCst),
            attendu,
            "#7263 : sous {os}, une lecture réseau laisse une sonde ALSA inutile contourner la garde du rescan"
        );
    }
    state.playback.stop(10).await;
    let compteur = Arc::new(AtomicUsize::new(0));
    let appels = compteur.clone();
    parc_local_sans_couper_la_lecture_selon_os(&state, "linux", || async move {
        appels.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    })
    .await;
    assert_eq!(
        compteur.load(Ordering::SeqCst),
        1,
        "#7263 : le hotplug reprend à l'arrêt sous Linux"
    );
}
