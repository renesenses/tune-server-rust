//! #2505 — les propriétés que le passage de `.ape` à un décodage incrémental
//! ne doit PAS perdre, et celles qu'il gagne.
//!
//! Le pic mémoire est éprouvé à part (`ape_pic_memoire_i2505`, binaire dédié :
//! l'allocateur y est processus-global). Ici : le délai avant le premier bloc,
//! l'exactitude à l'échantillon de la recherche, et le fait que
//! `max_duration_s` ARRÊTE le décodage au lieu de tronquer après coup.

use std::time::Instant;

#[path = "ape_fixture_i2505.rs"]
mod ape_fixture;

const CHUNK: usize = 32_768;

/// Déroule la lecture progressive exactement comme l'orchestrateur, et rend
/// `(pcm_sans_entete, delai_premier_bloc_pcm, duree_totale)`.
async fn lire_en_flux(
    chemin: &str,
    seek_s: f64,
) -> (Vec<u8>, std::time::Duration, std::time::Duration) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let data_ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let (levels_tx, mut levels_rx) = tokio::sync::mpsc::unbounded_channel();
    let purge = tokio::spawn(async move { while levels_rx.recv().await.is_some() {} });

    let debut = Instant::now();
    let consommateur = tokio::spawn(async move {
        let mut pcm = Vec::new();
        let mut premier_bloc_pcm: Option<std::time::Duration> = None;
        let mut entete_vue = false;
        while let Some(bloc) = rx.recv().await {
            if !entete_vue {
                entete_vue = true;
                assert_eq!(bloc.len(), 44, "en-tête WAV attendu en premier bloc");
                continue;
            }
            if premier_bloc_pcm.is_none() {
                premier_bloc_pcm = Some(debut.elapsed());
            }
            pcm.extend_from_slice(&bloc);
        }
        (pcm, premier_bloc_pcm.expect("aucun bloc PCM émis"))
    });

    let p = chemin.to_string();
    let dr = data_ready.clone();
    let rendu = tokio::task::spawn_blocking(move || {
        tune_core::audio::decode::decode_to_pcm_streaming_seeked(
            &p,
            None,
            None,
            Some(16),
            tx,
            CHUNK,
            dr,
            levels_tx,
            seek_s,
        )
    })
    .await
    .expect("tâche de décodage");
    let total = debut.elapsed();
    let (pcm, premier) = consommateur.await.expect("consommateur");
    purge.await.expect("purge");
    rendu.expect("le décodage progressif a échoué");
    (pcm, premier, total)
}

/// Le premier bloc PCM doit sortir BIEN avant la fin du décodage.
///
/// C'est la seconde moitié du ticket : « rien ne sort avant que le fichier
/// entier ne soit décodé ». Le rapport est mesuré dans la MÊME exécution que le
/// total, donc une machine chargée déplace les deux et non le rapport. Avec 48
/// trames APE, l'incrémental sort le premier bloc vers 1/48 du total ; le
/// décodage intégral ne le sort qu'une fois tout décodé.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ape_flux_emet_le_premier_bloc_pcm_avant_la_fin_du_decodage() {
    let fx = ape_fixture::build_multi_frame_ape(48);
    let (pcm, premier, total) = lire_en_flux(&fx.path, 0.0).await;
    assert_eq!(pcm, fx.expected_pcm, "PCM du flux ≠ référence");
    eprintln!(
        "i2505 premier bloc PCM à {:?} sur {:?} au total ({:.1} %)",
        premier,
        total,
        100.0 * premier.as_secs_f64() / total.as_secs_f64()
    );
    assert!(
        premier * 2 < total,
        "premier bloc PCM à {premier:?} pour un décodage total de {total:?} : \
         le flux attend la fin du décodage"
    );
}

/// La recherche doit rester exacte à l'échantillon — ET ne plus s'arrêter au
/// bout de la trame trouvée.
///
/// `ApeDecoder::decode_from`, employé jusqu'ici, ne rend QUE la fin de la trame
/// contenant l'échantillon visé (≈ 6,7 s à 44,1 kHz sur un vrai fichier). Le
/// chemin progressif, lui, ignorait purement et simplement `seek_s` : il
/// rejouait la piste depuis le début.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ape_recherche_exacte_a_l_echantillon_et_va_jusqu_au_bout() {
    let fx = ape_fixture::build_multi_frame_ape(12);
    // 4,5 s : au MILIEU d'une trame APE (une trame = 1 s ici), donc le reliquat
    // intra-trame de `SeekResult::skip_samples` est réellement exercé.
    let seek_s = 4.5_f64;
    let octets_par_trame_audio = fx.channels as usize * (fx.bit_depth as usize / 8);
    let offset = (seek_s * fx.sample_rate as f64) as usize * octets_par_trame_audio;
    let (pcm, _, _) = lire_en_flux(&fx.path, seek_s).await;
    assert_eq!(
        pcm.len(),
        fx.expected_pcm.len() - offset,
        "la lecture depuis {seek_s} s doit rendre TOUTE la fin de piste"
    );
    assert_eq!(
        pcm,
        fx.expected_pcm[offset..],
        "le PCM depuis {seek_s} s n'est pas exact à l'échantillon"
    );
}

/// Le décodage par lots (conversion, analyse) garde le même contrat de
/// recherche : exact à l'échantillon, et jusqu'à la fin de la piste.
#[test]
fn ape_par_lots_recherche_rend_toute_la_fin_de_piste() {
    let fx = ape_fixture::build_multi_frame_ape(12);
    let seek_s = 4.5_f64;
    let octets_par_trame_audio = fx.channels as usize * (fx.bit_depth as usize / 8);
    let offset = (seek_s * fx.sample_rate as f64) as usize * octets_par_trame_audio;
    let decode = tune_core::audio::decode::decode_to_pcm(&fx.path, None, None, seek_s, 0.0)
        .expect("décodage par lots");
    assert_eq!(decode.sample_rate, fx.sample_rate);
    assert_eq!(decode.channels, fx.channels as u32);
    assert_eq!(
        decode.pcm_bytes(),
        fx.expected_pcm[offset..],
        "le décodage par lots depuis {seek_s} s n'est pas exact à l'échantillon"
    );
}

/// `max_duration_s` doit ARRÊTER le décodage, pas jeter la fin à l'arrivée.
///
/// Preuve déterministe : la DERNIÈRE trame du fichier est sabotée (son CRC ne
/// tombe plus juste). Un décodage borné à 3 s ne l'atteint jamais et réussit ;
/// un décodage intégral l'atteint et échoue.
#[test]
fn ape_max_duration_arrete_le_decodage_avant_la_trame_sabotee() {
    let fx = ape_fixture::build_multi_frame_ape_corrupt_last(8);
    let borne = 3.0_f64;
    let borne_octets = (borne * fx.sample_rate as f64) as usize
        * fx.channels as usize
        * (fx.bit_depth as usize / 8);

    let borne_ok = tune_core::audio::decode::decode_to_pcm(&fx.path, None, None, 0.0, borne)
        .expect("le décodage borné ne doit jamais atteindre la trame sabotée");
    assert_eq!(
        borne_ok.pcm_bytes(),
        fx.expected_pcm[..borne_octets],
        "la fenêtre bornée n'est pas exacte"
    );
    assert!(
        (borne_ok.duration_s - borne).abs() < 1e-9,
        "durée rendue {} au lieu de {borne}",
        borne_ok.duration_s
    );

    let entier = tune_core::audio::decode::decode_to_pcm(&fx.path, None, None, 0.0, 0.0);
    assert!(
        entier.is_err(),
        "le décodage intégral doit buter sur la trame sabotée — sinon la borne \
         ne prouve rien"
    );
}
