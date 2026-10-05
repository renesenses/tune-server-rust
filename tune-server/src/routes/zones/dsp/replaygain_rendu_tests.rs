//! Lot eq-niveau — le ReplayGain de la piste entre dans `rendered_db`.
//!
//! Sur la sortie locale, le rabot à l'unité porte sur le produit
//! `volume × ReplayGain × compensation` (`effective_volume_units`). Un
//! ReplayGain négatif libère donc de la marge, et à 100 % de volume une partie
//! de la compensation passe bel et bien. La carte de `GET /zones/{id}/dsp`
//! l'ignorait : volume à 100 %, ReplayGain à −6 dB, elle annonçait 0 dB rendu.
//!
//! Le banc : une zone locale, un égaliseur dont le niveau moyen vaut −4 dB
//! (une crête étroite de +3,3 dB à 18 kHz : la réserve automatique, enceintes
//! en pièce moyenne, l’amène à −3,98 dB mesurés), le volume à 100 %, une piste
//! en cours taguée.

use super::compensation_de_niveau_de_zone;
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::NowPlaying;

async fn banc(tags: &[(&str, &str)], mode: &str) -> (crate::state::AppState, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set("plugin_equalizer_enabled", "true").unwrap();
    let profil = json!({
        "enabled": true,
        "listening": "speakers",
        "room_size": "medium",
        "speaker_placement": "free_standing",
        "bass_gain_db": 0.0,
        "mid_gain_db": 0.0,
        "treble_gain_db": 0.0,
        "bands": [{ "freq": 18000.0, "gain": 3.3, "q": 4.0, "type": "peak" }],
    });
    s.set(&format!("zone_{zone}_eq_profile"), &profil.to_string())
        .unwrap();
    s.set(tune_core::audio::replaygain::MODE_KEY, mode).unwrap();

    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.format = Some("flac".into());
    let tid = TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .unwrap();
    let meta = TrackMetadataRepo::with_backend(state.backend.clone());
    for (k, v) in tags {
        meta.set(tid, k, v).unwrap();
    }
    // `set_volume` crée l'état de lecture de la zone ; `update_now_playing`
    // n'écrit que dans un état existant.
    state.playback.set_volume(zone, 1.0).await;
    state
        .playback
        .update_now_playing(
            zone,
            NowPlaying {
                title: "Piste".into(),
                track_id: Some(tid),
                format: Some("flac".into()),
                ..Default::default()
            },
        )
        .await;
    (state, zone)
}

fn nombre(lc: &Value, cle: &str) -> f64 {
    lc[cle]
        .as_f64()
        .unwrap_or_else(|| panic!("`{cle}` absent de level_compensation : {lc}"))
}

/// La compensation du banc : 4 dB, ce que la réserve de l'égaliseur retire.
fn compensation_du_banc(lc: &Value) -> f64 {
    let comp = nombre(lc, "compensation_db");
    assert!(
        (comp - 4.0).abs() < 0.05,
        "le banc veut une réserve d'environ −4 dB : {lc}"
    );
    comp
}

#[tokio::test]
async fn volume_plein_replaygain_moins_6_la_compensation_de_4_db_passe_en_entier() {
    let (state, zone) = banc(&[("rg_track_gain", "-6.00 dB")], "track").await;
    let lc = compensation_de_niveau_de_zone(&state, zone).await;
    eprintln!("100 %, RG −6 dB : {lc}");
    let comp = compensation_du_banc(&lc);
    assert_eq!(nombre(&lc, "volume"), 1.0, "{lc}");
    assert!(
        (nombre(&lc, "replaygain_db") + 6.0).abs() < 0.011,
        "le ReplayGain retenu est celui du tag : {lc}"
    );
    // Marge = −20·log10(1 × 10^(−6/20)) = 6 dB ≥ 4 dB demandés : les 4 dB
    // passent en entier.
    assert!(
        (nombre(&lc, "rendered_db") - 4.0).abs() < 0.05
            && (nombre(&lc, "rendered_db") - comp).abs() < 0.011,
        "volume 100 %, ReplayGain −6 dB : les {comp} dB passent, la carte doit le dire : {lc}"
    );
    assert_eq!(nombre(&lc, "unrendered_db"), 0.0, "{lc}");
}

/// Contre-épreuve : la même piste SANS tag — rien ne libère de marge, tout
/// est raboté, exactement comme avant.
#[tokio::test]
async fn volume_plein_sans_replaygain_rien_n_est_rendu() {
    let (state, zone) = banc(&[], "track").await;
    let lc = compensation_de_niveau_de_zone(&state, zone).await;
    let comp = compensation_du_banc(&lc);
    assert_eq!(nombre(&lc, "replaygain_db"), 0.0, "{lc}");
    assert_eq!(nombre(&lc, "rendered_db"), 0.0, "{lc}");
    assert!((nombre(&lc, "unrendered_db") - comp).abs() < 0.011, "{lc}");
}

/// Un ReplayGain plus petit que la compensation n'en libère que sa part.
#[tokio::test]
async fn replaygain_moins_2_ne_rend_que_2_db() {
    let (state, zone) = banc(&[("rg_track_gain", "-2.00 dB")], "track").await;
    let lc = compensation_de_niveau_de_zone(&state, zone).await;
    let comp = compensation_du_banc(&lc);
    assert!((nombre(&lc, "rendered_db") - 2.0).abs() < 0.011, "{lc}");
    assert!(
        (nombre(&lc, "unrendered_db") - (comp - 2.0)).abs() < 0.011,
        "{lc}"
    );
}

/// Granularité : en mode album, c'est le gain d'ALBUM qui joue — et c'est
/// donc lui qui ouvre la marge.
#[tokio::test]
async fn en_mode_album_c_est_le_gain_d_album_qui_compte() {
    let tags = [("rg_track_gain", "-1.00 dB"), ("rg_album_gain", "-6.00 dB")];
    let (state, zone) = banc(&tags, "album").await;
    let lc = compensation_de_niveau_de_zone(&state, zone).await;
    let comp = compensation_du_banc(&lc);
    assert!((nombre(&lc, "replaygain_db") + 6.0).abs() < 0.011, "{lc}");
    assert!((nombre(&lc, "rendered_db") - comp).abs() < 0.011, "{lc}");

    // Même piste, mode piste : −1 dB seulement.
    let (state, zone) = banc(&tags, "track").await;
    let lc = compensation_de_niveau_de_zone(&state, zone).await;
    assert!((nombre(&lc, "replaygain_db") + 1.0).abs() < 0.011, "{lc}");
    assert!((nombre(&lc, "rendered_db") - 1.0).abs() < 0.011, "{lc}");
}

/// ReplayGain coupé : le tag est là, mais rien ne l'applique.
#[tokio::test]
async fn replaygain_coupe_n_ouvre_aucune_marge() {
    let (state, zone) = banc(&[("rg_track_gain", "-6.00 dB")], "off").await;
    let lc = compensation_de_niveau_de_zone(&state, zone).await;
    assert_eq!(nombre(&lc, "replaygain_db"), 0.0, "{lc}");
    assert_eq!(nombre(&lc, "rendered_db"), 0.0, "{lc}");
}

/// PURE : la sortie n'applique jamais le ReplayGain — le facteur retenu est
/// l'unité, même avec un tag et le mode armé.
#[tokio::test]
async fn sous_pure_le_replaygain_est_ignore() {
    let (state, zone) = banc(&[("rg_track_gain", "-6.00 dB")], "track").await;
    assert!(
        (super::facteur_replaygain_de_zone(&state, zone).await - 10f64.powf(-6.0 / 20.0)).abs()
            < 1e-3,
        "hors PURE, le facteur est celui du tag"
    );
    SettingsRepo::with_backend(state.backend.clone())
        .set(&format!("zone_{zone}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    assert_eq!(super::facteur_replaygain_de_zone(&state, zone).await, 1.0);
    let lc = compensation_de_niveau_de_zone(&state, zone).await;
    assert_eq!(nombre(&lc, "replaygain_db"), 0.0, "{lc}");
}
