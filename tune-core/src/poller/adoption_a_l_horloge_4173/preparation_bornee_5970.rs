//! #5970 — la préparation de la suivante ne fige plus le sondeur, et une
//! suivante prête trop tard ne part pas en `SetNextAVTransportURI`.
//!
//! ## Le défaut établi (ticket 240, darTZeel LHC, Qobuz, 1.0.0-rc2)
//!
//! La résolution de la suivante a pris 50,8 s, attendue DANS le tick, sans
//! borne. Le sondeur ne relevait plus rien ; la fin de la piste n'a pas été
//! vue, et le `SetNext` est parti environ 40 s après elle, sur un renderer
//! déjà arrêté.
//!
//! ## Le banc
//!
//! Celui de #4173 / #3967 (vrai `tick`, vrai orchestrateur, renderer simulé
//! en `dlna`), avec une résolution de la suivante artificiellement lente
//! (`RESOLUTION_LENTE_5970`, un `sleep` en tête de `resolve_gapless_next`).
//!
//! | résolution | reste à l'armement | attendu |
//! |---|---|---|
//! | 60 s | ~8 s | tick rendu dans le budget, pas de `SetNext`, pas de nouvel essai, `Play` de la suivante à la fin, une fois |
//! | 1,5 s | ~3 s | résolue mais trop tard : pas de `SetNext`, `Play` de la suivante à la fin, une fois |
//! | 1 s | ~30 s | dans le budget : `SetNext` posé comme avant |

use super::*;
use crate::outputs::traits::TransportState;
use crate::poller::fin_de_piste::RESOLUTION_LENTE_5970;

/// Au-delà, un repli qui n'est pas parti est un silence, pas une attente.
const SONDAGES_MAX: usize = 20;

/// Ce que l'ancien code aurait attendu : la résolution entière. Le
/// correctif doit rendre le tick bien avant.
const TICK_MAX: Duration = Duration::from_secs(6);

fn resolution_lente(retard: Option<Duration>) {
    RESOLUTION_LENTE_5970.with(|c| c.set((retard, 0)));
}

fn resolutions_tentees() -> usize {
    RESOLUTION_LENTE_5970.with(|c| c.get().1)
}

async fn set_next_envoyes(banc: &Banc) -> usize {
    let reg = banc.outputs.lock().await;
    let arc = reg.get(APPAREIL).unwrap();
    let sortie = arc.lock().await;
    let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
    mock.set_next_call_count().await
}

async fn le_transport(banc: &Banc, etat: TransportState, position_ms: u64, duree_ms: u64) {
    let reg = banc.outputs.lock().await;
    let arc = reg.get(APPAREIL).unwrap();
    let sortie = arc.lock().await;
    let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
    mock.set_state(etat).await;
    mock.set_position(position_ms);
    mock.set_duration(duree_ms);
}

fn une_seconde_passe(banc: &mut Banc) {
    let Some(ps) = banc.poll_states.get_mut(&banc.zone_id) else {
        return;
    };
    ps.track_loaded_at = Instant::now() - Duration::from_secs(600);
    if let Some(t) = ps.track_started_at {
        ps.track_started_at = Some(t - Duration::from_secs(1));
    }
}

/// Un tick, chronométré, qui doit rendre la main avant `TICK_MAX`.
async fn tic_borne(banc: &mut Banc) -> Duration {
    let t0 = Instant::now();
    let rendu = tokio::time::timeout(TICK_MAX, banc.tic()).await;
    assert!(
        rendu.is_ok(),
        "le tick doit rendre la main avant {TICK_MAX:?} même quand la suivante tarde \
         à se résoudre : il sonde TOUTES les zones"
    );
    t0.elapsed()
}

/// La piste finie a été tirée jusqu'au dernier octet (#4645) : sa fin est
/// une vraie fin.
async fn la_piste_finie_est_tiree(banc: &Banc) {
    let flux = banc.flux_finie.clone();
    let total = banc
        .orchestrator
        .streamer_total_bytes(&flux)
        .await
        .expect("la piste finie a une taille connue");
    banc.le_renderer_tire(&flux, total).await;
}

/// Le renderer s'arrête à zéro après la fin ; rend le nombre de sondages
/// avant le premier `Play`, ou `None`.
async fn sondages_avant_le_repli(banc: &mut Banc) -> Option<usize> {
    for n in 1..=SONDAGES_MAX {
        le_transport(banc, TransportState::Stopped, 0, 0).await;
        une_seconde_passe(banc);
        banc.tic().await;
        if !banc.play_complets().await.is_empty() {
            return Some(n);
        }
    }
    None
}

/// La suivante part par `Play`, une fois ; l'écran avance d'UNE piste ; et
/// les sondages suivants, le renderer jouant, ne relancent rien.
async fn la_suivante_part_une_fois_par_play(banc: &mut Banc) {
    assert!(
        sondages_avant_le_repli(banc).await.is_some(),
        "en {SONDAGES_MAX} sondages, la fin de piste doit jouer la suivante"
    );
    assert_eq!(
        banc.play_complets().await,
        vec![ARMEE.to_string()],
        "la suivante doit partir par `Play`, une seule fois"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));
    for position in [1_000, 2_000, 3_000] {
        le_transport(banc, TransportState::Playing, position, 212_000).await;
        une_seconde_passe(banc);
        banc.tic().await;
    }
    assert_eq!(
        banc.play_complets().await,
        vec![ARMEE.to_string()],
        "jamais deux `Play`"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "jamais de double avance dans la file"
    );
}

/// LE défaut : la résolution de la suivante dure une minute. Le tick rend
/// la main dans son budget, ne re-tente pas, et la fin de piste joue la
/// suivante par `Play`.
#[tokio::test]
async fn resolution_d_une_minute_le_tick_continue_et_la_fin_joue_la_suivante() {
    let mut banc = Banc::monter().await;
    resolution_lente(Some(Duration::from_secs(60)));

    // Fenêtre d'armement, 8 s avant la fin : budget plancher.
    banc.renderer_a(229_000, 232).await;
    let duree = tic_borne(&mut banc).await;
    assert!(
        duree >= Duration::from_millis(decisions::PREPARATION_GAPLESS_PLANCHER_MS),
        "la résolution a eu sa chance ({duree:?})"
    );
    assert_eq!(resolutions_tentees(), 1);
    assert_eq!(set_next_envoyes(&banc).await, 0, "rien n'a été posé");
    assert!(!banc.poll_states[&banc.zone_id].gapless_sent);

    // Les sondages suivants de la fenêtre ne re-tentent pas : le tick ne se
    // fige pas une seconde fois.
    for (position, horloge) in [(233_000, 236), (236_000, 239)] {
        banc.renderer_a(position, horloge).await;
        let duree = tic_borne(&mut banc).await;
        assert!(duree < Duration::from_secs(1), "{duree:?}");
    }
    assert_eq!(
        resolutions_tentees(),
        1,
        "pas de nouvel essai pour cette position"
    );
    resolution_lente(None);

    la_piste_finie_est_tiree(&banc).await;
    la_suivante_part_une_fois_par_play(&mut banc).await;
    assert_eq!(
        set_next_envoyes(&banc).await,
        0,
        "jamais de `SetNext` tardif"
    );
}

/// La suivante est résolue, mais trop près de la fin pour qu'un `SetNext`
/// serve : elle ne part pas en `SetNext`, la fin de piste la joue.
#[tokio::test]
async fn suivante_prete_trop_tard_pas_de_set_next_la_fin_la_joue() {
    let mut banc = Banc::monter().await;
    resolution_lente(Some(Duration::from_millis(1_500)));

    banc.renderer_a(234_500, 237).await;
    tic_borne(&mut banc).await;
    assert_eq!(resolutions_tentees(), 1, "la résolution a abouti");
    assert_eq!(
        set_next_envoyes(&banc).await,
        0,
        "une suivante prête trop tard ne doit pas partir en `SetNext`"
    );
    assert!(!banc.poll_states[&banc.zone_id].gapless_sent);

    banc.renderer_a(236_000, 239).await;
    tic_borne(&mut banc).await;
    assert_eq!(
        resolutions_tentees(),
        1,
        "pas de nouvel essai pour cette position"
    );
    resolution_lente(None);

    la_piste_finie_est_tiree(&banc).await;
    la_suivante_part_une_fois_par_play(&mut banc).await;
    assert_eq!(set_next_envoyes(&banc).await, 0);
}

/// Contre-épreuve : lente mais dans son budget, la suivante est posée comme
/// avant. La borne ne coûte pas l'enchaînement sans blanc.
#[tokio::test]
async fn lente_mais_dans_le_budget_la_suivante_est_posee() {
    let mut banc = Banc::monter().await;
    resolution_lente(Some(Duration::from_secs(1)));

    banc.renderer_a(207_000, 210).await;
    tic_borne(&mut banc).await;
    resolution_lente(None);
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);
    assert!(banc.poll_states[&banc.zone_id].gapless_sent);
}

#[test]
fn budget_pris_sur_le_reste_de_la_piste_et_borne() {
    use decisions::budget_de_resolution_gapless as budget;
    let s = Duration::from_secs;
    assert_eq!(budget(s(30)), s(10), "plafond");
    assert_eq!(budget(s(15)), s(7), "reste - marge");
    assert_eq!(budget(s(8)), s(2), "plancher");
    assert_eq!(budget(s(0)), s(2), "plancher, même la fin passée");
}

#[test]
fn trop_tard_pour_un_set_next_sous_la_marge() {
    use decisions::suivante_trop_tardive_pour_setnext as trop_tard;
    assert!(trop_tard(Duration::ZERO));
    assert!(trop_tard(Duration::from_millis(2_000)));
    assert!(!trop_tard(Duration::from_millis(2_001)));
    assert!(!trop_tard(Duration::from_secs(20)));
}
