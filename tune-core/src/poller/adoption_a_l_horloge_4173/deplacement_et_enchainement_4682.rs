//! #4682 — Gapless : avancement et durée faux après un déplacement de la
//! barre de progression (Stéphane Villerio, Eversolo DMP-A6 en DLNA).
//!
//! Deux défauts, tous deux dans `tick` :
//!
//! - **L'enchaînement perdu.** Déplacé près de la fin, le morceau finit
//!   DANS la grâce de déplacement. Le renderer enchaîne sur la suivante, la
//!   chute de position est écartée par la grâce (#2170) — à raison, un flux
//!   recréé a la même forme — mais `last_position_ms` prend quand même la
//!   position de la suivante : la chute n'est plus jamais revue. L'écran
//!   reste sur la piste finie, avec la position de la suivante.
//! - **L'avance prématurée.** Le pic de position survivait au déplacement :
//!   après un recul, `played_enough` restait vrai, et un renderer qui rapporte
//!   une position transitoire proche de 0 avec une autre durée (celle de la
//!   suivante, `SetNext` armé) faisait avancer la file pendant que la piste
//!   courante jouait encore — la durée de la SUIVANTE à l'écran.
//!
//! Et, en marge : l'avance gapless n'effaçait pas `last_seek_at`, la grâce
//! d'un déplacement débordait sur la piste suivante.

use super::*;

impl Banc {
    /// Ce que le renderer rapporte, et RIEN d'autre : l'état de sondage n'est
    /// pas retouché, le déplacement doit y être replié par `tick` lui-même.
    async fn le_renderer_joue(&self, position_ms: u64, duree_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(crate::outputs::traits::TransportState::Playing)
            .await;
        mock.set_duration(duree_ms);
        mock.set_position(position_ms);
    }

    /// La grâce de déplacement est écoulée : l'instant du déplacement recule
    /// d'autant, et le sondeur le sait déjà replié (un NOUVEL instant serait
    /// pris pour un nouveau déplacement).
    async fn la_grace_est_ecoulee(&mut self, depuis: Duration) {
        let seek_at = self
            .playback
            .dater_le_deplacement(self.zone_id, depuis)
            .await
            .expect("un déplacement a eu lieu");
        self.poll_states
            .get_mut(&self.zone_id)
            .unwrap()
            .last_seek_seen = Some(seek_at);
    }
}

/// **Défaut A.** L'utilisateur déplace la barre à 3 s de la fin, `SetNext`
/// déjà armé. Le renderer finit la piste et enchaîne pendant la grâce ; à la
/// fin de la grâce, l'écran doit être sur la piste armée — sans aucun `Play`.
#[tokio::test]
async fn deplacement_pres_de_la_fin_l_enchainement_pendant_la_grace_est_retenu() {
    let mut banc = Banc::monter().await;
    banc.armer().await;

    banc.playback.seek(banc.zone_id, 234_500).await;
    banc.le_renderer_joue(235_000, DUREE_RENDERER_MS).await;
    banc.tic().await;
    banc.le_renderer_joue(236_800, DUREE_RENDERER_MS).await;
    banc.tic().await;

    // Le renderer enchaîne : sa position repart, sa durée reste à 2 s près
    // celle de la piste finie (le chemin `duration_changed` ne voit rien).
    banc.le_renderer_joue(800, DUREE_RENDERER_MS).await;
    banc.tic().await;
    let (_, titre, _) = banc.ecran().await;
    assert_eq!(
        titre, FINIE,
        "pendant la grâce la chute est écartée (#2170) : rien ne bouge encore"
    );

    banc.la_grace_est_ecoulee(Duration::from_secs(4)).await;
    banc.le_renderer_joue(2_000, DUREE_RENDERER_MS).await;
    banc.tic().await;

    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "la chute vue pendant la grâce était la fin réelle : l'écran doit suivre le renderer"
    );
    assert_eq!(
        banc.play_complets().await,
        Vec::<String>::new(),
        "le renderer joue déjà la suivante : aucun `Play`"
    );
    assert!(
        banc.playback
            .get_state(banc.zone_id)
            .await
            .last_seek_at
            .is_none(),
        "la grâce du déplacement ne court pas sur la piste suivante"
    );
}

/// **Défaut B.** `SetNext` armé, l'utilisateur recule à 1:40. Le DMP-A6
/// rapporte une position transitoire à 0 et la durée de la suivante : la
/// file ne doit PAS avancer, ni pendant la grâce, ni à sa fin.
#[tokio::test]
async fn recul_apres_armement_une_duree_transitoire_n_avance_pas() {
    let mut banc = Banc::monter().await;
    banc.armer().await;

    banc.playback.seek(banc.zone_id, 100_000).await;
    banc.le_renderer_joue(100_000, DUREE_RENDERER_MS).await;
    banc.tic().await;

    // Le transitoire : position 0, durée de la piste armée.
    banc.le_renderer_joue(0, 212_000).await;
    banc.tic().await;
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (0, FINIE),
        "un recul n'est pas une fin : la durée transitoire de la suivante ne fait pas avancer"
    );

    // La grâce se termine, la piste courante joue à la cible + 4 s : la chute
    // écartée pendant la grâce est réexaminée, et rejetée.
    banc.la_grace_est_ecoulee(Duration::from_secs(4)).await;
    banc.le_renderer_joue(104_000, DUREE_RENDERER_MS).await;
    banc.tic().await;
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (0, FINIE));
    assert_eq!(banc.play_complets().await, Vec::<String>::new());
}

/// En marge : l'avance gapless efface le déplacement de la piste d'avant,
/// comme `play()` le fait au changement de piste.
#[tokio::test]
async fn l_avance_gapless_efface_le_deplacement_de_la_piste_d_avant() {
    let banc = Banc::monter().await;
    banc.playback.seek(banc.zone_id, 200_000).await;
    banc.orchestrator
        .advance_queue_metadata(banc.zone_id, 1)
        .await
        .unwrap();
    assert!(
        banc.playback
            .get_state(banc.zone_id)
            .await
            .last_seek_at
            .is_none()
    );
}

/// Le verdict pur de la fin de grâce.
#[test]
fn le_verdict_pur_de_la_fin_de_grace() {
    use decisions::chute_en_grace_etait_une_fin as fin;
    // Déplacé à 234,5 s d'une piste de 237,651 s, 4 s plus tôt, renderer à 2 s.
    assert!(fin(234_500, 4_000, 2_000, 237_651));
    // La piste courante n'a pas pu finir : recul à 100 s.
    assert!(!fin(100_000, 4_000, 0, 237_651));
    // Déplacement vers le début : le flux recréé repart de 0 (#2170).
    assert!(!fin(0, 3_500, 500, 237_651));
    // La position rapportée est plus grande que le temps écoulé depuis le
    // déplacement : elle n'est pas repartie après lui.
    assert!(!fin(234_500, 4_000, 5_000, 237_651));
    // Durée inconnue : on ne tranche pas.
    assert!(!fin(234_500, 4_000, 2_000, 0));
}
