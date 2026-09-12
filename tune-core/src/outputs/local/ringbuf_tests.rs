use super::{
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED_HRESULT, NativePcmRing, RingBuf, WasapiInitDecision,
    render_local_shared_integer_callback, wasapi_aligned_duration_100ns, wasapi_init_decision,
};
use crate::audio::soft_mute::SoftMuteRamp;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

struct AllocationTracker;

thread_local! {
    static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    // Le compteur est thread-local, comme l'armement : deux tests parallèles
    // qui mesurent en même temps ne doivent ni s'additionner, ni se remettre
    // à zéro l'un l'autre — un faux vert aussi bien qu'un faux rouge.
    static TRACKED_ALLOCATOR_CALLS: Cell<usize> = const { Cell::new(0) };
}

fn record_allocator_call() {
    TRACK_ALLOCATIONS.with(|tracking| {
        if tracking.get() {
            TRACKED_ALLOCATOR_CALLS.with(|calls| calls.set(calls.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for AllocationTracker {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocator_call();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocator_call();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocator_call();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static TEST_ALLOCATOR: AllocationTracker = AllocationTracker;

fn count_allocator_calls<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    // Initialise le TLS avant d'armer la mesure : son premier accès peut
    // appartenir à l'infrastructure de test, pas au chemin temps réel.
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
    TRACKED_ALLOCATOR_CALLS.with(|calls| calls.set(0));
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
    let result = operation();
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
    (result, TRACKED_ALLOCATOR_CALLS.with(Cell::get))
}

fn assert_no_allocation<T>(operation: impl FnOnce() -> T) -> T {
    let (result, calls) = count_allocator_calls(operation);
    assert_eq!(
        calls, 0,
        "la section simulant le callback audio a appelé l'allocateur"
    );
    result
}

#[test]
fn vide_plein_et_bouclage() {
    let rb = RingBuf::new(4);
    let mut out = [0.0f32; 4];

    // Vide : rien à lire, et `pop` ne doit pas mentir sur le compte.
    assert_eq!(rb.available(), 0);
    assert_eq!(rb.pop(&mut out), 0);

    // Plein : la capacité borne l'écriture, le surplus est refusé.
    assert_eq!(rb.push(&[1.0, 2.0, 3.0, 4.0, 5.0]), 4);
    assert_eq!(rb.available(), 4);
    assert_eq!(rb.push(&[9.0]), 0, "un tampon plein n'accepte rien");

    assert_eq!(rb.pop(&mut out), 4);
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);

    // Bouclage : on repart au début du stockage sans perdre l'ordre.
    assert_eq!(rb.push(&[5.0, 6.0, 7.0]), 3);
    let mut deux = [0.0f32; 2];
    assert_eq!(rb.pop(&mut deux), 2);
    assert_eq!(deux, [5.0, 6.0]);
    assert_eq!(rb.push(&[8.0, 9.0, 10.0]), 3);
    let mut reste = [0.0f32; 4];
    assert_eq!(rb.pop(&mut reste), 4);
    assert_eq!(reste, [7.0, 8.0, 9.0, 10.0]);
}

#[test]
fn clear_remet_a_zero_les_curseurs_et_le_stockage() {
    let rb = RingBuf::new(8);
    rb.push(&[1.0, 2.0, 3.0]);
    rb.clear();
    assert_eq!(rb.available(), 0);
    let mut out = [42.0f32; 3];
    assert_eq!(rb.pop(&mut out), 0, "rien ne doit survivre a un clear");
}

/// #2206 — les six familles de callbacks ASIO/WASAPI reposent sur ces
/// trois primitives. Le compteur est local au thread du test afin que les
/// allocations des autres tests parallèles ne puissent pas fabriquer un
/// faux échec.
#[test]
fn drains_temps_reel_ne_font_aucune_allocation() {
    let float_ring = RingBuf::new(8);
    assert_eq!(float_ring.push(&[0.25, -0.5, 0.75]), 3);
    let mut i16_out = [0i16; 4];
    let read = assert_no_allocation(|| {
        float_ring.pop_mapped(&mut i16_out, |sample| {
            (f64::from(sample) * 32_768.0)
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16
        })
    });
    assert_eq!(read, 3);
    assert_eq!(i16_out[..3], [8192, -16384, 24576]);

    let native_ring = NativePcmRing::new(8);
    assert_eq!(native_ring.push(&[0x1234_0000, -0x1234_0000]), 2);
    let mut native_i16 = [0i16; 4];
    let read = assert_no_allocation(|| {
        native_ring.pop_mapped(&mut native_i16, |sample| (sample >> 16) as i16)
    });
    assert_eq!(read, 2);
    assert_eq!(native_i16[..2], [0x1234, -0x1234]);

    assert_eq!(native_ring.push(&[0x1234_5600, -0x1234_5600]), 2);
    let zero = cpal::I24::new(0).unwrap();
    let mut native_i24 = [zero; 4];
    let read = assert_no_allocation(|| {
        native_ring.pop_mapped(&mut native_i24, |sample| {
            cpal::I24::new(sample >> 8).unwrap()
        })
    });
    assert_eq!(read, 2);
    assert_eq!(native_i24[0].inner(), 0x123456);

    assert_eq!(native_ring.push(&[0x1234_5600, -0x1234_5600]), 2);
    let mut pcm = [0xAAu8; 12];
    let written = assert_no_allocation(|| native_ring.pop_pcm_bytes(&mut pcm, 24));
    assert_eq!(written, 6);
    assert_eq!(&pcm[..3], &[0x56, 0x34, 0x12]);
}

/// #2218 — le repli entier local **partagé** (cpal shared) convertissait à
/// travers un `Vec<f32>` capturé par la fermeture, qu'il faisait grandir dès
/// que la période s'allongeait : `scratch.resize(n, 0.0)` appelait
/// l'allocateur **dans la période audio**.
///
/// La garde instrumente TOUTE la période — amorçage, pondération, rampe,
/// conversion, comblement — et enchaîne des périodes qui GRANDISSENT (8, puis
/// 2, puis 8, puis 32). C'est exactement ce que l'ancien `scratch` ne savait
/// pas faire sans allouer.
#[test]
fn le_rappel_entier_local_partage_rend_une_periode_sans_allouer() {
    let ring = RingBuf::new(64);
    let volume = AtomicU32::new(1000);
    let paused = AtomicBool::new(false);
    let silent = AtomicBool::new(false);
    let data_started = AtomicBool::new(false);
    let mut ramp = SoftMuteRamp::new(44_100, 2);
    assert_eq!(ring.push(&[0.25, -0.5, 0.75, -1.0]), 4);

    // Amorçage : sous le seuil, la période est comblée de silence et le ring
    // n'est pas entamé.
    let mut avant_amorcage = [7i16; 8];
    let lus = assert_no_allocation(|| {
        render_local_shared_integer_callback(
            &ring,
            &volume,
            &paused,
            &silent,
            &data_started,
            &mut ramp,
            0,
            8,
            0i16,
            &mut avant_amorcage,
        )
    });
    assert_eq!(lus, 0);
    assert_eq!(avant_amorcage, [0i16; 8]);
    assert_eq!(ring.available(), 4, "l'amorçage ne consomme rien");

    assert_eq!(ring.push(&[0.5, -0.25, 1.0, -0.75]), 4);

    // Première période servie, rampe désarmée : volume plein, conversion
    // directe dans le tampon du backend.
    let mut petite = [0i16; 2];
    let lus = assert_no_allocation(|| {
        render_local_shared_integer_callback(
            &ring,
            &volume,
            &paused,
            &silent,
            &data_started,
            &mut ramp,
            0,
            8,
            0i16,
            &mut petite,
        )
    });
    assert_eq!(lus, 2);
    assert_eq!(petite, [8192, -16384]);

    // La période QUADRUPLE et le volume change : c'est le cas qui faisait
    // grandir l'ancien scratch. Le reliquat est comblé de zéros.
    volume.store(500, Ordering::Relaxed);
    let mut grande = [7i16; 8];
    let lus = assert_no_allocation(|| {
        render_local_shared_integer_callback(
            &ring,
            &volume,
            &paused,
            &silent,
            &data_started,
            &mut ramp,
            0,
            8,
            0i16,
            &mut grande,
        )
    });
    assert_eq!(lus, 6);
    assert_eq!(grande, [12288, -16384, 8192, -4096, 16384, -12288, 0, 0]);

    // Rampe ARMÉE et descendante : le facteur bouge à chaque trame, toujours
    // sans allouer, et il s'applique AVANT la conversion.
    volume.store(1000, Ordering::Relaxed);
    paused.store(true, Ordering::Relaxed);
    assert_eq!(ring.push(&[0.5f32; 32]), 32);
    let mut periode_rampee = [0i16; 32];
    let lus = assert_no_allocation(|| {
        render_local_shared_integer_callback(
            &ring,
            &volume,
            &paused,
            &silent,
            &data_started,
            &mut ramp,
            50,
            8,
            0i16,
            &mut periode_rampee,
        )
    });
    assert_eq!(lus, 32);
    assert_eq!(
        periode_rampee[0], 16384,
        "la première trame est encore à pleine amplitude"
    );
    assert_eq!(
        periode_rampee[0], periode_rampee[1],
        "les deux canaux d'une trame partagent le facteur"
    );
    assert!(
        periode_rampee[31] < periode_rampee[0] && periode_rampee[31] > 0,
        "la rampe doit descendre sans atteindre le silence : {:?}",
        periode_rampee
    );
    assert!(ramp.gain() < 1.0, "la rampe a avancé pendant la période");
}

/// Contre-épreuve de l'instrument lui-même : l'ancien `scratch.resize(n, 0.0)`
/// appelle bel et bien l'allocateur. Sans ce témoin, un compteur muet rendrait
/// la garde ci-dessus verte contre n'importe quel code.
#[test]
fn le_compteur_d_allocations_voit_l_ancien_scratch_resize() {
    let mut ancien_scratch = Vec::<f32>::new();
    let (_, appels) = count_allocator_calls(|| ancien_scratch.resize(64, 0.0));
    assert!(
        appels > 0,
        "le resize de l'ancien scratch doit appeler l'allocateur"
    );
}

#[test]
fn duree_wasapi_alignee_suit_le_nombre_de_frames_du_pilote() {
    assert_eq!(wasapi_aligned_duration_100ns(480, 48_000).unwrap(), 100_000);
    assert_eq!(wasapi_aligned_duration_100ns(441, 44_100).unwrap(), 100_000);
    assert_eq!(wasapi_aligned_duration_100ns(1, 44_100).unwrap(), 227);
    assert!(wasapi_aligned_duration_100ns(0, 48_000).is_err());
    assert!(wasapi_aligned_duration_100ns(480, 0).is_err());
}

#[test]
fn seul_le_hresult_d_alignement_autorise_une_seconde_initialisation() {
    assert_eq!(wasapi_init_decision(0), WasapiInitDecision::Ready);
    assert_eq!(
        wasapi_init_decision(AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED_HRESULT),
        WasapiInitDecision::RetryWithAlignedBuffer
    );
    assert_eq!(
        wasapi_init_decision(0x8000_4005u32 as i32),
        WasapiInitDecision::Fail(0x8000_4005u32 as i32)
    );
}

/// Le vrai contrat : un producteur, un consommateur, aucune perte, aucun
/// doublon, aucun desordre. C'est ce qu'un tampon SPSC promet, et c'est
/// exactement ce qu'un comportement indéfini peut casser silencieusement.
#[test]
fn un_producteur_un_consommateur_ne_perdent_ni_ne_reordonnent_rien() {
    const N: usize = 100_000;
    let rb = Arc::new(RingBuf::new(1024));

    let prod = {
        let rb = rb.clone();
        std::thread::spawn(move || {
            let mut envoye = 0usize;
            while envoye < N {
                let lot: Vec<f32> = (envoye..(envoye + 64).min(N)).map(|i| i as f32).collect();
                let mut offset = 0;
                while offset < lot.len() {
                    let n = rb.push(&lot[offset..]);
                    offset += n;
                    if n == 0 {
                        std::thread::yield_now();
                    }
                }
                envoye += lot.len();
            }
        })
    };

    let mut recu = Vec::with_capacity(N);
    let mut tampon = [0.0f32; 128];
    while recu.len() < N {
        let n = rb.pop(&mut tampon);
        if n == 0 {
            std::thread::yield_now();
            continue;
        }
        recu.extend_from_slice(&tampon[..n]);
    }
    prod.join().unwrap();

    assert_eq!(recu.len(), N);
    for (i, v) in recu.iter().enumerate() {
        assert_eq!(*v, i as f32, "echantillon {i} perdu, duplique ou reordonne");
    }
}
