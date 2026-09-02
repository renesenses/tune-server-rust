//! #2505 — « Monkey's Audio décodé en entier ».
//!
//! L'ÉPREUVE. Le module `audio/decode.rs` rangeait `.ape` dans le repli
//! « décodage intégral puis découpage » de `decode_to_pcm_streaming_inner` :
//! rien ne sortait avant que le fichier entier ne soit décodé, et la piste
//! entière tenait en mémoire (~2 Gio de PCM natif pour un 24/96 d'une heure,
//! plus ~2,8 Gio de `i32`).
//!
//! Un test qui ne comparerait que le PCM rendu PASSERAIT avant comme après :
//! c'est exactement le piège. Ce test éprouve LA PROPRIÉTÉ QUI CHANGE — aucune
//! allocation ne détient jamais la piste entière — au moyen d'un allocateur
//! global qui suit le pic d'octets vivants pendant que la fonction de
//! PRODUCTION `decode_to_pcm_streaming_seeked` (celle que l'orchestrateur
//! appelle pour la lecture progressive locale et réseau) déroule le fichier.
//! Le témoin bit à bit est fait au passage, bloc par bloc.
//!
//! Binaire de test à lui seul, DÉLIBÉRÉMENT : l'allocateur global est
//! PROCESSUS-global, et un test voisin qui allouerait en parallèle fausserait
//! le pic.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "ape_fixture_i2505.rs"]
mod ape_fixture;

// ---------------------------------------------------------------------------
// Allocateur qui compte les octets vivants et retient leur maximum.
// ---------------------------------------------------------------------------

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct PeakTracking;

#[inline]
fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for PeakTracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        out
    }
}

#[global_allocator]
static ALLOC: PeakTracking = PeakTracking;

/// Secondes d'audio du fichier construit — une trame APE par seconde.
const SECONDES: u32 = 48;
/// Taille de bloc demandée au décodeur, celle que l'orchestrateur emploie.
const CHUNK: usize = 32_768;

#[test]
fn ape_flux_ne_materialise_jamais_la_piste_entiere() {
    let mut fx = ape_fixture::build_multi_frame_ape(SECONDES);
    let piste_octets = fx.expected_pcm.len();
    let trame_octets = fx.frame_pcm_bytes();
    assert_eq!(
        piste_octets,
        SECONDES as usize * 44_100 * 4,
        "48 s de 16 bits stéréo à 44,1 kHz"
    );
    let chemin = fx.path.clone();
    // MOVE, pas clone : la référence doit rester dans le socle mesuré, pas
    // s'ajouter au pic.
    let attendu = std::mem::take(&mut fx.expected_pcm);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    let (pic, lus) = rt.block_on(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let data_ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let (levels_tx, mut levels_rx) = tokio::sync::mpsc::unbounded_channel();
        // Le robinet de niveaux RECOPIE le PCM : sans purge il accumulerait la
        // piste entière et polluerait la mesure.
        let purge = tokio::spawn(async move { while levels_rx.recv().await.is_some() {} });

        // Le consommateur VÉRIFIE puis JETTE : il ne doit rien accumuler, sinon
        // c'est LUI qui détiendrait la piste. La comparaison au fil de l'eau
        // est aussi le témoin bit à bit.
        let consommateur = tokio::spawn(async move {
            let mut offset = 0usize;
            let mut premier = true;
            while let Some(bloc) = rx.recv().await {
                if premier {
                    premier = false;
                    // Premier bloc : l'en-tête WAV, seul.
                    assert_eq!(bloc.len(), 44, "en-tête WAV attendu en premier bloc");
                    assert_eq!(&bloc[0..4], b"RIFF");
                    continue;
                }
                let fin = offset + bloc.len();
                assert!(
                    fin <= attendu.len(),
                    "le flux dépasse la piste attendue ({fin} > {})",
                    attendu.len()
                );
                assert_eq!(
                    &bloc[..],
                    &attendu[offset..fin],
                    "PCM différent de la référence à l'octet {offset}"
                );
                offset = fin;
            }
            offset
        });

        // Socle : tout ce qui est déjà vivant est acquis ; seul le SURPLUS
        // alloué pendant le décodage nous intéresse.
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);

        let dr = data_ready.clone();
        let rendu = tokio::task::spawn_blocking(move || {
            tune_core::audio::decode::decode_to_pcm_streaming_seeked(
                &chemin,
                None,
                None,
                Some(16),
                tx,
                CHUNK,
                dr,
                levels_tx,
                0.0,
            )
        })
        .await
        .expect("tâche de décodage");
        let pic = PEAK.load(Ordering::Relaxed).saturating_sub(base);
        let lus = consommateur.await.expect("consommateur");
        purge.await.expect("purge des niveaux");
        let (bd, sr) = rendu.expect("le décodage progressif a échoué");
        assert_eq!(bd, 16);
        assert_eq!(sr, 44_100);
        (pic, lus)
    });

    // Témoin : le flux rend la piste ENTIÈRE, bit à bit (chaque bloc a déjà été
    // comparé à la référence ci-dessus).
    assert_eq!(
        lus, piste_octets,
        "le flux doit rendre la piste entière ({piste_octets} octets)"
    );

    // LA propriété qui change. Avec le décodage intégral, le pic vaut au bas
    // mot une fois la piste en octets PLUS deux fois la piste en `i32`. Avec le
    // décodage incrémental, il vaut quelques trames APE.
    eprintln!("i2505 pic={pic} o | piste={piste_octets} o | trame APE={trame_octets} o");
    assert!(
        pic < piste_octets / 2,
        "le décodage a détenu {pic} octets pour une piste de {piste_octets} : \
         la piste entière (ou davantage) a été matérialisée — le chemin n'est pas incrémental"
    );
    drop(fx);
}
