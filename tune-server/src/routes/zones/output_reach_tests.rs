use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn zone_with(output_type: Option<&str>, device: Option<&str>) -> Zone {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend);
    let id = repo.create("Ce PC", output_type, device).unwrap();
    repo.get(id).unwrap().unwrap()
}

/// Zone navigateur en lecture depuis `started_ago`, avec une session.
fn browser_playing_since(started_ago: Duration) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Track".into(),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        last_play_started_at: Instant::now().checked_sub(started_ago),
        ..Default::default()
    }
}

#[test]
fn zone_sans_sortie_est_signalee_avant_le_clic() {
    let zone = zone_with(Some("local"), None);
    assert_eq!(
        output_reach_of(&zone, &ZoneState::default(), false, Some(true)),
        "no_output"
    );
}

#[test]
fn zone_avec_sortie_ne_signale_rien() {
    let zone = zone_with(Some("dlna"), Some("dev-1"));
    assert_eq!(
        output_reach_of(&zone, &ZoneState::default(), false, Some(true)),
        "ok"
    );
}

#[test]
fn zone_navigateur_a_larret_ne_signale_rien() {
    // Une zone navigateur n'a jamais de périphérique : sans lecture en
    // cours il n'y a rien à reprocher.
    let zone = zone_with(Some("browser"), None);
    assert_eq!(
        output_reach_of(&zone, &ZoneState::default(), false, Some(true)),
        "ok"
    );
}

#[test]
fn zone_navigateur_qui_demarre_beneficie_du_delai() {
    let zone = zone_with(Some("browser"), None);
    let ps = browser_playing_since(Duration::from_secs(2));
    assert_eq!(
        output_reach_of(&zone, &ps, false, Some(true)),
        "ok",
        "un onglet qui vient de recevoir stream_url n'a pas encore tiré d'octets"
    );
}

#[test]
fn zone_navigateur_ecoutee_ne_signale_rien() {
    let zone = zone_with(Some("browser"), None);
    let ps = browser_playing_since(Duration::from_secs(60));
    assert_eq!(output_reach_of(&zone, &ps, true, Some(true)), "ok");
}

#[test]
fn zone_navigateur_sans_personne_au_bout_est_signalee() {
    let zone = zone_with(Some("browser"), None);
    let ps = browser_playing_since(Duration::from_secs(60));
    assert_eq!(
        output_reach_of(&zone, &ps, false, Some(true)),
        "browser_unattended",
        "une minute de lecture sans un octet tiré : personne n'écoute"
    );
}

/// Le bandeau et l'abandon doivent basculer au MÊME instant (#2630).
///
/// Le poller arrête désormais une lecture que personne ne tire au bout de
/// `tune_core::poller::DELAI_SILENCE_ETABLI`. Si cette vue concluait plus
/// tard, l'utilisateur verrait la lecture s'arrêter sans avoir jamais lu
/// pourquoi ; plus tôt, elle accuserait un onglet qui a encore le droit de
/// démarrer. Un seuil re-codé en dur ici les ferait diverger en silence.
#[test]
fn le_bandeau_bascule_a_linstant_ou_le_poller_renonce() {
    let zone = zone_with(Some("browser"), None);
    let seuil = tune_core::poller::DELAI_SILENCE_ETABLI;
    assert_eq!(
        output_reach_of(&zone, &browser_playing_since(seuil), false, Some(true)),
        "browser_unattended",
        "à l'échéance du poller, le client doit déjà savoir pourquoi"
    );
    assert_eq!(
        output_reach_of(
            &zone,
            &browser_playing_since(seuil - Duration::from_secs(1)),
            false,
            Some(true)
        ),
        "ok",
        "une seconde avant, l'onglet peut encore démarrer"
    );
}

/// #2588 — l'explication du silence survit à l'arrêt qui la provoquait.
///
/// C'est LE défaut du ticket : le bandeau « aucun onglet ne reçoit le
/// son » est le seul endroit où Tune explique le silence d'une zone
/// navigateur, et il disparaissait à l'instant même où l'utilisateur
/// arrêtait la zone — c'est-à-dire au moment exact où il réagissait à
/// l'absence de son. Pierre M l'a vu passer sans pouvoir le relire.
#[test]
fn le_constat_de_silence_survit_a_larret() {
    let zone = zone_with(Some("browser"), None);
    let mut ps = browser_playing_since(Duration::from_secs(60));
    ps.state = PlayState::Stopped;
    ps.browser_unattended_at = Some(Instant::now());
    assert_eq!(
        output_reach_of(&zone, &ps, false, Some(true)),
        "browser_unattended",
        "arrêtée juste après le constat, la zone doit encore dire pourquoi"
    );
}
/// La rétention est bornée : une zone laissée tranquille cesse d'accuser.
#[test]
fn le_constat_de_silence_finit_par_se_taire() {
    let zone = zone_with(Some("browser"), None);
    let mut ps = browser_playing_since(Duration::from_secs(60));
    ps.state = PlayState::Stopped;
    ps.browser_unattended_at =
        Instant::now().checked_sub(BROWSER_UNATTENDED_RETENTION + Duration::from_secs(1));
    assert_eq!(output_reach_of(&zone, &ps, false, Some(true)), "ok");
}
/// Une zone à l'arrêt qui n'a jamais rien eu à expliquer se tait.
///
/// Contre-épreuve de la précédente : sans ce cas, un `return
/// "browser_unattended"` inconditionnel passerait les deux autres.
#[test]
fn zone_a_larret_sans_constat_ne_dit_rien() {
    let zone = zone_with(Some("browser"), None);
    let mut ps = browser_playing_since(Duration::from_secs(60));
    ps.state = PlayState::Stopped;
    assert_eq!(
        output_reach_of(&zone, &ps, false, Some(true)),
        "ok",
        "aucun silence constaté : rien à dire"
    );
}
/// Le constat ne doit pas survivre à une lecture qui, elle, est reçue.
///
/// `play()` efface la marque, et la vue la lève dès que l'onglet tire le
/// flux. Tant que la zone joue, c'est la consommation qui tranche — la
/// marque d'hier n'a pas voix au chapitre.
#[test]
fn une_lecture_recue_ignore_le_constat_precedent() {
    let zone = zone_with(Some("browser"), None);
    let mut ps = browser_playing_since(Duration::from_secs(60));
    ps.browser_unattended_at = Some(Instant::now());
    assert_eq!(output_reach_of(&zone, &ps, true, Some(true)), "ok");
}
#[test]
fn etat_restaure_ne_conclut_rien() {
    // `last_play_started_at` est `#[serde(skip)]` : après un redémarrage il
    // vaut None. On ne doit pas inventer un silence sur cette absence.
    let zone = zone_with(Some("browser"), None);
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Track".into(),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        last_play_started_at: None,
        ..Default::default()
    };
    assert_eq!(output_reach_of(&zone, &ps, false, Some(true)), "ok");
}

// ---------------------------------------------------------------------------
// #4601 / #4580 — l'appareil est nommé, mais le registre vivant l'a perdu
// ---------------------------------------------------------------------------

/// Deux testeurs ont SUPPRIMÉ ET RECRÉÉ une zone pour la faire refonctionner.
/// `output_reach` ne savait dire que « pas de sortie du tout » et « l'onglet
/// ne tire rien ». Une zone dont l'appareil a disparu du registre — renderer
/// DLNA qui change d'identifiant en redémarrant, bail DHCP renouvelé —
/// rendait `ok`, et l'écran n'avait rien à dire.
#[test]
fn un_appareil_disparu_du_registre_se_dit() {
    let zone = zone_with(Some("dlna"), Some("uuid:eversolo-a8"));
    assert_eq!(
        output_reach_of(&zone, &ZoneState::default(), false, Some(false)),
        "output_missing"
    );
}

#[test]
fn le_meme_appareil_present_ne_dit_rien() {
    // Contre-épreuve du cas ci-dessus : c'est bien l'ABSENCE qui parle, pas
    // le type de sortie ni l'identifiant.
    let zone = zone_with(Some("dlna"), Some("uuid:eversolo-a8"));
    assert_eq!(
        output_reach_of(&zone, &ZoneState::default(), false, Some(true)),
        "ok"
    );
}

#[test]
fn ne_rien_savoir_ne_vaut_pas_absent() {
    // 🔴 `None` = on n'a pas regardé. On préfère taire un défaut réel que
    // d'en inventer un — la même discipline que `last_play_started_at`.
    let zone = zone_with(Some("dlna"), Some("uuid:eversolo-a8"));
    assert_eq!(
        output_reach_of(&zone, &ZoneState::default(), false, None),
        "ok"
    );
}

#[test]
fn une_zone_sans_appareil_reste_no_output() {
    // L'ordre des tests compte : « pas d'appareil du tout » prime sur
    // « appareil introuvable », sinon la zone jamais configurée recevrait le
    // message qui promet un rattachement automatique.
    let zone = zone_with(Some("local"), None);
    assert_eq!(
        output_reach_of(&zone, &ZoneState::default(), false, Some(false)),
        "no_output"
    );
}

#[test]
fn une_zone_navigateur_ne_cherche_aucun_appareil() {
    // Sa sortie est un onglet, pas une entrée du registre : même en lui
    // affirmant l'absence, elle garde son propre verdict.
    let zone = zone_with(Some("browser"), Some("browser"));
    assert_eq!(
        output_reach_of(&zone, &ZoneState::default(), false, Some(false)),
        "ok"
    );
}
