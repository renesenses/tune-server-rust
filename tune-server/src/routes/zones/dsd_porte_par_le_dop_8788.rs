//! #8788 — un DSD porté par du DoP s'annonce en DSD, pas en « WAV 176,4/24 ».
//!
//! Retour rc12 (fil 2833) : zone sur une sortie ALSA locale, DSF64 à DSF256
//! lus depuis MinimServer. Le DAC affiche bien la cadence DSD, la mini-barre
//! affiche « WAV » et 176,4 à 705,6 kHz.
//!
//! Le serveur média ne sert pas le `.dsf` : il le sert en **DoP dans un
//! conteneur WAV** (journal du 15/11 : « DSF64 de MinimServer → DoP 176,4,
//! `dop=true`, marqueurs présents »). La piste en cours prend donc le format
//! du conteneur (`format_du_mime("audio/wav")` = `wav`) et la cadence du
//! porteur PCM (176 400 Hz pour un DSD64). Les deux sont vrais pour le
//! conteneur et faux pour la musique : le client range la piste en Hi-Res au
//! lieu de DSD.
//!
//! La sortie, elle, sait : elle détecte le DoP **sur les octets**
//! (`is_dop_pcm`) et le poller le recopie dans `ZoneState::dop_active` à
//! chaque tour. Cette projection rapproche les deux, **à l'affichage
//! seulement**.
//!
//! 🔴 Elle ne touche pas `NowPlaying`. Son `format` sert aussi à relancer la
//! lecture (`transport.rs`, `media_format: np.format.clone()` au
//! déplacement) : y écrire `dsd` ferait prendre au WAV le chemin du DSD brut
//! (`est_dsd_brut`) et casserait le déplacement dans la piste.

use serde_json::{Map, Value, json};
use tune_core::playback::ZoneState;

/// Fréquence DSD pour une cadence de porteur DoP (16 bits DSD par mot de
/// 24 bits, par canal : DSD = porteur × 16). `None` hors des cadences de
/// porteur connues — un 192 kHz n'est jamais un DoP.
fn frequence_dsd_du_porteur(cadence: u64) -> Option<u64> {
    matches!(cadence, 176_400 | 352_800 | 705_600 | 1_411_200).then_some(cadence * 16)
}

fn deja_annonce_en_dsd(format: Option<&str>) -> bool {
    tune_core::orchestrator::est_source_dsd(format)
}

/// Réécrit, dans l'objet JSON d'une piste, le format et la résolution d'un
/// DSD porté par du DoP. Ne fait rien si la sortie ne détecte pas de DoP, si
/// la piste s'annonce déjà en DSD, ou si sa cadence n'est pas celle d'un
/// porteur DoP.
///
/// Le conteneur reste dit à côté (`format_conteneur`), avec `dop: true` :
/// rien de ce qui était vrai n'est perdu.
pub(crate) fn annoncer_le_dsd_porte(piste: &mut Map<String, Value>, dop_active: bool) {
    if !dop_active || deja_annonce_en_dsd(piste.get("format").and_then(Value::as_str)) {
        return;
    }
    // Un porteur DoP est toujours en 24 bits ; une profondeur différente
    // annoncée dit autre chose qu'un DoP.
    if piste
        .get("bit_depth")
        .and_then(Value::as_u64)
        .is_some_and(|b| b != 24)
    {
        return;
    }
    let Some(dsd) = piste
        .get("sample_rate")
        .and_then(Value::as_u64)
        .and_then(frequence_dsd_du_porteur)
    else {
        return;
    };
    let conteneur = piste.get("format").cloned().unwrap_or(Value::Null);
    piste.insert("format_conteneur".into(), conteneur);
    piste.insert("format".into(), json!("dsd"));
    piste.insert("sample_rate".into(), json!(dsd));
    piste.insert("bit_depth".into(), json!(1));
    piste.insert("dop".into(), json!(true));
}

/// La même règle, appliquée à la piste rangée sous `cle` d'une charge utile
/// de zone (`current_track` pour les routes, `now_playing` pour
/// l'instantané WebSocket).
pub(crate) fn inject_dsd_porte(obj: &mut Map<String, Value>, cle: &str, ps: &ZoneState) {
    if let Some(piste) = obj.get_mut(cle).and_then(Value::as_object_mut) {
        annoncer_le_dsd_porte(piste, ps.dop_active);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use tune_core::db::zone_repo::ZoneRepo;
    use tune_core::playback::NowPlaying;

    fn piste(format: Option<&str>, sr: u32, bd: Option<u32>) -> Map<String, Value> {
        let np = NowPlaying {
            source: "upnp".into(),
            format: format.map(String::from),
            sample_rate: Some(sr),
            bit_depth: bd,
            ..Default::default()
        };
        json!(np).as_object().unwrap().clone()
    }

    #[test]
    fn un_wav_porteur_de_dop_s_annonce_en_dsd() {
        for (porteur, dsd) in [
            (176_400, 2_822_400u64),
            (352_800, 5_644_800),
            (705_600, 11_289_600),
        ] {
            let mut p = piste(Some("wav"), porteur, Some(24));
            annoncer_le_dsd_porte(&mut p, true);
            assert_eq!(p["format"], "dsd", "porteur {porteur}");
            assert_eq!(p["sample_rate"], dsd, "porteur {porteur}");
            assert_eq!(p["bit_depth"], 1);
            assert_eq!(p["format_conteneur"], "wav");
            assert_eq!(p["dop"], true);
        }
    }

    #[test]
    fn sans_dop_ou_hors_porteur_rien_ne_change() {
        // Un vrai WAV 176,4/24 sans DoP sur le fil.
        let mut p = piste(Some("wav"), 176_400, Some(24));
        annoncer_le_dsd_porte(&mut p, false);
        assert_eq!(p["format"], "wav");
        assert_eq!(p["sample_rate"], 176_400);
        assert!(p.get("dop").is_none());
        // Une cadence qui n'est pas celle d'un porteur DoP.
        let mut p = piste(Some("wav"), 192_000, Some(24));
        annoncer_le_dsd_porte(&mut p, true);
        assert_eq!(p["format"], "wav");
        // Une profondeur qui n'est pas celle d'un porteur DoP.
        let mut p = piste(Some("flac"), 176_400, Some(16));
        annoncer_le_dsd_porte(&mut p, true);
        assert_eq!(p["format"], "flac");
        // Un DSF de la bibliothèque s'annonce déjà en DSD : intact.
        let mut p = piste(Some("dsf"), 2_822_400, Some(1));
        annoncer_le_dsd_porte(&mut p, true);
        assert_eq!(p["format"], "dsf");
        assert_eq!(p["sample_rate"], 2_822_400);
        assert!(p.get("format_conteneur").is_none());
    }

    /// Les trois charges utiles REST de la zone : liste, fiche, et celle que
    /// rendent les routes de lecture (`build_zone_json`).
    #[tokio::test]
    async fn les_charges_utiles_de_zone_annoncent_le_dsd_porte() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let id = ZoneRepo::with_backend(state.backend.clone())
            .create("DoP 8788", Some("local"), None)
            .unwrap();
        state
            .playback
            .play(
                id,
                NowPlaying {
                    source: "upnp".into(),
                    format: Some("wav".into()),
                    sample_rate: Some(176_400),
                    bit_depth: Some(24),
                    ..Default::default()
                },
            )
            .await;
        state.playback.set_dop_active(id, true).await;
        let router = crate::routes::router(state.clone());
        for (url, liste) in [
            ("/api/v1/zones".to_string(), true),
            (format!("/api/v1/zones/{id}"), false),
        ] {
            let reponse = router
                .clone()
                .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(reponse.status(), StatusCode::OK, "{url}");
            let json: Value =
                serde_json::from_slice(&to_bytes(reponse.into_body(), 1_000_000).await.unwrap())
                    .unwrap();
            let zone = if liste {
                json.as_array()
                    .unwrap()
                    .iter()
                    .find(|z| z["id"] == id)
                    .unwrap()
                    .clone()
            } else {
                json
            };
            assert_eq!(zone["current_track"]["format"], "dsd", "{url} : {zone}");
            assert_eq!(zone["current_track"]["sample_rate"], 2_822_400, "{url}");
            assert_eq!(zone["current_track"]["format_conteneur"], "wav", "{url}");
        }
        let zone = crate::routes::playback::build_zone_json(&state, id).await;
        assert_eq!(zone["current_track"]["format"], "dsd", "build_zone_json : {zone}");
        assert_eq!(zone["current_track"]["sample_rate"], 2_822_400);

        // L'état de lecture lui-même garde le conteneur : c'est lui que la
        // relance au déplacement renvoie à la résolution.
        let ps = state.playback.get_state(id).await;
        assert_eq!(ps.now_playing.unwrap().format.as_deref(), Some("wav"));
    }
}
