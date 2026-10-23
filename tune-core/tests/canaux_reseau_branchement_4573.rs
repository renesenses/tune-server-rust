//! #4573 — le BRANCHEMENT, de bout en bout : un renderer qui déclare deux
//! canaux, une piste 5.1, et ce que l'orchestrateur décide de servir.
//!
//! La .159 avait livré la lecture de la déclaration sans la brancher : « la
//! réduction n'est pas branchée, `audio::canaux_reseau_4573` n'a aucun
//! appelant hors de ses propres témoins » (commentaire de clôture sur
//! l'issue). Ce fichier est la garde de l'autre moitié.
//!
//! Le renderer est un VRAI serveur SOAP sur `127.0.0.1`, qui répond le Sink
//! relevé par Xavier Joly sur son Denon AVR-X1600H le 20/09/2026 — LPCM en
//! `channels=1` et `channels=2`, `audio/flac:*` sans aucun `channels=`. Rien
//! n'est simulé côté orchestrateur : la zone, la file, la piste et la sonde
//! sont les vraies.

use std::sync::Arc;

#[path = "banc_canaux_reseau_4573.rs"]
mod banc;
use banc::*;

/// Ce que l'orchestrateur décide de servir à ce renderer, pour cette piste.
async fn canaux_servis(canaux_source: i32, sink: &'static str) -> Option<u32> {
    let (base, handle) = renderer_qui_annonce(sink).await;
    let (backend, zone_id, _track_id) = base_avec_piste(canaux_source);

    let mut registre = tune_core::outputs::registry::OutputRegistry::new();
    registre.register(Box::new(tune_core::outputs::dlna::DlnaOutput::new(
        "Home Theater".into(),
        UDN.into(),
        "127.0.0.1".into(),
        format!("{base}/AVTransport"),
        format!("{base}/RenderingControl"),
        Some(format!("{base}/ConnectionManager")),
    )));

    let orch = tune_core::orchestrator::PlaybackOrchestrator::new(
        backend.clone(),
        Arc::new(tune_core::playback::PlaybackManager::new()),
        Arc::new(tune_core::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            tune_core::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(registre)),
        None,
    );
    let r = orch.resolve_queue_item_url(zone_id, 0).await.unwrap();
    handle.abort();
    r.channels
}

/// 🔴 LE témoin du lot. Rouge avant : la règle existait depuis la .159 mais
/// personne ne l'appelait — `DecisionLocale.channels` valait `track.channels`,
/// donc 6, et le FLAC 5.1 partait tel quel (`dlna_set_uri_ok …
/// advertised_mime=audio/flac`, journal de Xavier du 20/09).
#[tokio::test]
async fn un_51_vers_un_renderer_qui_annonce_deux_canaux_part_en_stereo() {
    assert_eq!(
        canaux_servis(6, SINK_DENON).await,
        Some(2),
        "le Denon annonce deux canaux au plus : le 5.1 doit être replié avant \
         de partir, pas laissé à la charge de l'ampli"
    );
}

/// 🔴 LA contre-épreuve, sur le MÊME chemin et la même piste : un renderer
/// dont le Sink ne porte aucun `channels=` ne fait RIEN réduire. C'est
/// « on ne sait pas », jamais « deux ».
#[tokio::test]
async fn un_renderer_muet_sur_ses_canaux_ne_declenche_aucune_reduction() {
    assert_eq!(
        canaux_servis(6, SINK_MUET_SUR_LES_CANAUX).await,
        Some(6),
        "aucune déclaration : les six voies partent intactes"
    );
}

/// Une stéréo ordinaire vers le même Denon ne change pas de chemin — et la
/// sonde SOAP n'est même pas lancée. Sans cette garde, toute la bibliothèque
/// aurait payé un aller-retour SOAP par piste.
#[tokio::test]
async fn une_stereo_vers_le_meme_denon_ne_change_rien() {
    assert_eq!(canaux_servis(2, SINK_DENON).await, Some(2));
}

/// Un 7.1 vers ce Denon tombe aussi à deux : la matrice BS.775 étendue
/// existe, et la déclaration est la même.
#[tokio::test]
async fn un_71_vers_le_meme_denon_part_aussi_en_stereo() {
    assert_eq!(canaux_servis(8, SINK_DENON).await, Some(2));
}

/// Garde de l'appareillage lui-même : le faux Denon répond bien, et ce que
/// Tune lit de son Sink est bien « deux canaux au plus ». Sans elle, un
/// témoin vert ne prouverait rien — il pourrait l'être parce que la sonde
/// échoue et que personne ne réduit jamais.
#[tokio::test]
async fn le_faux_denon_est_bien_lu_par_la_sonde() {
    let (base, handle) = renderer_qui_annonce(SINK_DENON).await;
    let sortie = tune_core::outputs::dlna::DlnaOutput::new(
        "Home Theater".into(),
        UDN.into(),
        "127.0.0.1".into(),
        format!("{base}/AVTransport"),
        format!("{base}/RenderingControl"),
        Some(format!("{base}/ConnectionManager")),
    );
    let caps = sortie.probe_capabilities().await;
    assert!(caps.probed, "la sonde doit aboutir : {caps:?}");
    assert_eq!(caps.canaux_max, Some(2), "Sink lu : {:?}", caps.sink);
    handle.abort();
}
