//! #8558 — « le serveur s'arrête quand je lance un FLAC haute résolution sur
//! le Chromecast » (forum, fil 2777, VM de 4 Gio sans swap).
//!
//! La console du testeur montrait le tueur OOM du noyau : `tune-server` à
//! 3,7 Gio. Le bras FICHIER du transcodage réseau (cible FLAC, Content-Length
//! exigé) tient la piste entière en mémoire, et il la recopiait : PCM décodé,
//! copie de canaux, copie `f32`, copie avec pré-roll, sortie `f32` qui
//! grandissait par doublements, sortie `i32`, puis l'encodeur qui recopiait
//! encore tout en `i32` et par canal, et le fichier encodé cloné.
//!
//! Ce test mesure LA propriété qui change : le pic d'octets vivants pendant
//! le transcodage de PRODUCTION (`transcode_source_to_file`) d'un FLAC
//! 192 kHz / 24 bits stéréo ramené à 96 kHz, rapporté à la taille de la piste
//! décodée en `i32`. Le décodage entier est une donnée du bras fichier (une
//! fois la piste) ; tout ce qui dépasse ~2 fois la piste est une copie.
//!
//! Binaire de test à lui seul : l'allocateur global est PROCESSUS-global.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

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

    // Une réallocation qui grandit peut DÉPLACER le bloc : l'ancien et le
    // nouveau coexistent un instant. Compté ainsi, comme le voit la RAM.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            let live = LIVE.load(Ordering::Relaxed);
            PEAK.fetch_max(live + new_size, Ordering::Relaxed);
        }
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

const SECONDES: usize = 20;
const SOURCE_SR: u32 = 192_000;
const CIBLE_SR: u32 = 96_000;
const CANAUX: u32 = 2;

/// Un FLAC 192/24 stéréo de `SECONDES` s : deux sinus et un peu de bruit
/// déterministe, pour que l'encodeur ait de la matière à compresser.
fn ecrire_flac_192_24(chemin: &std::path::Path) {
    let mut enc = tune_core::audio::encoder::AudioEncoder::new("flac", SOURCE_SR, 24, CANAUX);
    enc.start_sync().expect("start");
    let trames_par_bloc = SOURCE_SR as usize;
    let mut graine: u32 = 0x2777;
    for seconde in 0..SECONDES {
        let mut octets = Vec::with_capacity(trames_par_bloc * CANAUX as usize * 3);
        for i in 0..trames_par_bloc {
            let t = (seconde * trames_par_bloc + i) as f64 / SOURCE_SR as f64;
            for c in 0..CANAUX {
                graine = graine.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                let bruit = ((graine >> 16) & 0xff) as f64 - 128.0;
                let f = if c == 0 { 997.0 } else { 1_499.0 };
                let v = (2.0 * std::f64::consts::PI * f * t).sin() * 4_000_000.0 + bruit * 64.0;
                octets.extend_from_slice(&(v as i32).to_le_bytes()[..3]);
            }
        }
        enc.write_sync(&octets).expect("write");
    }
    let flac = enc.finish_sync().expect("finish");
    std::fs::write(chemin, flac).expect("écriture du FLAC");
}

#[test]
fn transcodage_fichier_192_24_vers_96_ne_recopie_pas_la_piste_8558() {
    let dossier = tempfile::tempdir().expect("tempdir");
    let source = dossier.path().join("source-192-24.flac");
    let dest = dossier.path().join("sortie-96-24.flac");
    ecrire_flac_192_24(&source);

    // La piste décodée à sa cadence native, en `i32` : l'unité de mesure.
    let piste_i32 = SECONDES * SOURCE_SR as usize * CANAUX as usize * 4;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    let source_s = source.to_string_lossy().into_owned();
    let dest_s = dest.to_string_lossy().into_owned();
    let (pic, rendu) = rt.block_on(async move {
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let rendu = tune_core::orchestrator::transcoder_vers_un_fichier_8558(
            source_s,
            CIBLE_SR,
            CANAUX as u16,
            24,
            "flac".into(),
            dest_s,
        )
        .await;
        (PEAK.load(Ordering::Relaxed).saturating_sub(base), rendu)
    });
    let (taille, pcm, bd) = rendu.expect("le transcodage a échoué");

    // Témoins : le fichier est écrit, et le PCM rendu est bien la piste à
    // 96 kHz / 24 bits (à la trame près).
    assert_eq!(bd, 24);
    assert_eq!(std::fs::metadata(&dest).expect("sortie").len(), taille);
    let trames = pcm.len() / (3 * CANAUX as usize);
    let attendues = SECONDES * CIBLE_SR as usize;
    assert!(
        trames.abs_diff(attendues) <= 1,
        "{trames} trames rendues, {attendues} attendues"
    );

    let rapport = pic as f64 / piste_i32 as f64;
    eprintln!("#8558 pic={pic} o | piste i32 à 192 kHz={piste_i32} o | rapport={rapport:.2}");
    // Avant #8558 : décodage par doublements, copie de canaux, `f32`, pré-roll,
    // sortie `f32`, sortie `i32`, puis l'encodeur — plus de 4 fois la piste.
    // Après : la piste décodée, sa sortie à 96 kHz, ses octets, l'encodeur
    // par tranches — un peu plus d'une fois et demie.
    assert!(
        rapport < 2.0,
        "le transcodage fichier a détenu {pic} octets, {rapport:.2} fois la piste \
         décodée ({piste_i32} o) : la piste entière est encore recopiée (#8558)"
    );
}
