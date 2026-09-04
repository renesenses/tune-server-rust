use super::{
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED_HRESULT, NativePcmRing, RingBuf, WasapiInitDecision,
    wasapi_aligned_duration_100ns, wasapi_init_decision,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct AllocationTracker;

thread_local! {
    static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
}
static TRACKED_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for AllocationTracker {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        TRACK_ALLOCATIONS.with(|tracking| {
            if tracking.get() {
                TRACKED_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        TRACK_ALLOCATIONS.with(|tracking| {
            if tracking.get() {
                TRACKED_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        });
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        TRACK_ALLOCATIONS.with(|tracking| {
            if tracking.get() {
                TRACKED_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        });
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static TEST_ALLOCATOR: AllocationTracker = AllocationTracker;

fn assert_no_allocation<T>(operation: impl FnOnce() -> T) -> T {
    // Initialise le TLS avant d'armer la mesure : son premier accès peut
    // appartenir à l'infrastructure de test, pas au chemin temps réel.
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
    TRACKED_ALLOCATIONS.store(0, Ordering::SeqCst);
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
    let result = operation();
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
    assert_eq!(
        TRACKED_ALLOCATIONS.load(Ordering::SeqCst),
        0,
        "la section simulant le callback audio a alloué"
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
