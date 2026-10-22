//! #6497 (fil 2332), course restée ouverte après #6543 — dans l'enchaînement
//! « fichier direct », la position était remise à zéro AVANT l'URI.
//!
//! Le bloc d'enchaînement faisait, dans cet ordre :
//!
//! ```text
//! position_ms.store(0)
//! *current_title.lock().await  = …   // verrou 1
//! *current_artist.lock().await = …   // verrou 2
//! *current_uri.lock().await    = ""  // verrou 3
//! ```
//!
//! `get_status` lit la position, puis l'URI. Un sondage qui tombe entre la
//! remise à zéro et le troisième verrou voit donc « position 0, URI de la
//! piste FINIE » : exactement la preuve `RejeuDeLaPisteFinie` du journal de
//! Steve Taylor, qui fait relancer la piste suivante depuis zéro.
//!
//! Le banc provoque cet entrelacement sans sommeil arbitraire : il tient le
//! verrou du titre — le premier que l'enchaînement prend après la remise à
//! zéro — pendant la bascule. La tâche de lecture reste alors suspendue
//! entre ses écritures, et le banc lit ce que `get_status` lirait (position,
//! puis URI, dans le même ordre). Tenir ce verrou ne crée pas la fenêtre :
//! il la rend aussi large que nécessaire pour qu'on la lise à coup sûr. En
//! production, un `get_status` concurrent qui tient lui-même un de ces
//! verrous suffit à y suspendre la tâche de lecture.
//!
//! L'enchaînement DSD natif avait le même ordre ; il est éprouvé de la même
//! façon.
//!
//! Aucun son réel : faux endpoint local qui avale l'audio.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::outputs::oaat::OaatOutput;
use crate::outputs::oaat::rejeu_lecture_directe_6497_tests::{faux_endpoint_avec, wav};
use crate::outputs::traits::{OutputTarget, PlayMedia};
use crate::poller::decisions;

const FLUX_FINI: &str = "95d3dfd1-000e-4bcd-96da-8ceef446eb47";

/// Attend `condition` en rendant la main à l'exécuteur ; échoue après
/// `delai` avec `quoi`. Les conditions attendues sont des états de la
/// sortie, pas des instants : le résultat ne dépend pas de la vitesse.
async fn attendre(delai: Duration, quoi: &str, mut condition: impl FnMut() -> bool) {
    let boucle = async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(delai, boucle)
        .await
        .unwrap_or_else(|_| panic!("{quoi}"));
}

/// DSF DSD64 stéréo de `millis` ms, au contenu non nul.
fn dsf(millis: u32) -> Vec<u8> {
    const BLOC: usize = 4096;
    let octets_par_canal = (2_822_400 / 8 * millis as usize / 1000).div_ceil(BLOC) * BLOC;
    let data_len = octets_par_canal * 2;
    let mut b = Vec::with_capacity(92 + data_len);
    b.extend_from_slice(b"DSD ");
    b.extend_from_slice(&28u64.to_le_bytes());
    b.extend_from_slice(&(92 + data_len as u64).to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes());
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&52u64.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&2u32.to_le_bytes());
    b.extend_from_slice(&2u32.to_le_bytes());
    b.extend_from_slice(&2_822_400u32.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&((octets_par_canal * 8) as u64).to_le_bytes());
    b.extend_from_slice(&(BLOC as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(12 + data_len as u64).to_le_bytes());
    b.extend((0..data_len).map(|i| (i % 251) as u8 | 1));
    b
}

/// Joue la piste 1, arme la piste 2 en fichier local, tient le verrou du
/// titre pendant la bascule et rend ce que `get_status` lirait au moment où
/// la position vient de revenir à zéro : (position, URI).
async fn instantane_pendant_la_bascule(
    endpoint_dsd: bool,
    extension: &str,
    mime: &str,
    contenu: fn(u32) -> Vec<u8>,
) -> (u64, Option<String>) {
    let dir = crate::test_scratch::scratch_dir("tune-oaat-course-6497");
    let piste_1 = dir.join(format!("01 - The Chrysalis.{extension}"));
    let piste_2 = dir.join(format!("02 - That's Our Sun.{extension}"));
    // Piste 1 assez longue pour poser le verrou bien avant sa fin.
    std::fs::write(&piste_1, contenu(3_000)).unwrap();
    std::fs::write(&piste_2, contenu(3_000)).unwrap();

    let port = faux_endpoint_avec(endpoint_dsd).await;
    let sortie = OaatOutput::new(
        "red [Verox OAAT]".into(),
        "127.0.0.1".into(),
        port,
        "faux-verox".into(),
    );
    let url_1 = format!("http://127.0.0.1:1/stream/{FLUX_FINI}.{extension}");
    let chemin_1 = piste_1.to_str().unwrap().to_owned();
    sortie
        .play_media(&PlayMedia {
            url: &url_1,
            mime_type: mime,
            title: Some("The Chrysalis"),
            file_path: Some(&chemin_1),
            duration_ms: Some(3_000),
            ..Default::default()
        })
        .await
        .expect("play_media");

    attendre(
        Duration::from_secs(15),
        "la lecture depuis le fichier ne s'est pas engagée",
        || sortie.prefers_local_file_gapless(),
    )
    .await;

    let url_2 = format!("http://127.0.0.1:1/stream/jamais-tire.{extension}");
    let chemin_2 = piste_2.to_str().unwrap().to_owned();
    sortie
        .set_next_media(&PlayMedia {
            url: &url_2,
            mime_type: mime,
            title: Some("That's Our Sun"),
            file_path: Some(&chemin_2),
            duration_ms: Some(3_000),
            ..Default::default()
        })
        .await
        .expect("set_next_media");

    // La piste 1 avance : la position n'est plus celle du départ.
    let position = sortie.position_ms.clone();
    attendre(
        Duration::from_secs(15),
        "la position de la piste 1 n'a jamais avancé",
        || position.load(Ordering::SeqCst) > 0,
    )
    .await;

    // Point de synchronisation : tant que le banc tient ce verrou,
    // l'enchaînement ne peut pas dépasser sa première écriture du titre.
    let verrou_titre = sortie.current_title.lock().await;
    assert_eq!(
        verrou_titre.as_deref(),
        Some("The Chrysalis"),
        "le verrou a été pris après la bascule : le banc n'a rien observé"
    );

    // La tâche de lecture atteint la fin de la piste 1 et se suspend sur le
    // verrou du titre. Sa remise à zéro de la position est le signal.
    attendre(
        Duration::from_secs(20),
        "l'enchaînement n'a pas remis la position à zéro",
        || position.load(Ordering::SeqCst) == 0,
    )
    .await;

    // Ce que `get_status` lirait maintenant, dans son ordre : position, URI.
    let position_lue = position.load(Ordering::SeqCst);
    let uri_lue = sortie.current_uri.lock().await.clone();
    drop(verrou_titre);
    let _ = sortie.stop().await;
    (position_lue, uri_lue)
}

fn verifier(position: u64, uri: Option<String>, chemin: &str) {
    assert_eq!(position, 0);
    assert!(
        !decisions::renderer_rejoue_la_piste_finie(uri.as_deref(), Some(FLUX_FINI), None, false),
        "pendant l'enchaînement {chemin}, un sondage voit la position revenue à 0 \
         avec l'URI de la piste finie ({uri:?}) : le garde-fou #5411 y voit un \
         rejeu et relance la piste suivante"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_sondage_pendant_l_enchainement_direct_ne_voit_jamais_position_zero_et_flux_fini() {
    let (position, uri) = instantane_pendant_la_bascule(false, "wav", "audio/wav", wav).await;
    verifier(position, uri, "direct (WAV)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_sondage_pendant_l_enchainement_dsd_natif_ne_voit_jamais_position_zero_et_flux_fini() {
    let (position, uri) = instantane_pendant_la_bascule(true, "dsf", "audio/x-dsf", dsf).await;
    verifier(position, uri, "DSD natif (DSF)");
}
