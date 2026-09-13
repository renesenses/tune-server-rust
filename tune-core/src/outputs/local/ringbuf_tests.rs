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

// ---------------------------------------------------------------------------
// REF-8 (#2219) — la chaîne de PRODUCTION du puits natif, de bout en bout.
//
// Les empreintes de `empreinte_wasapi_f70496.rs` et `empreinte_asio_f70496.rs`
// s'arrêtent à `CaptureOutputNatif` : un puits de TEST. Les témoins
// `native_windows_ring_*` de `tests.rs` partent, eux, de mots `i32` écrits à
// la main. Entre les deux il restait un trou, et c'est le trou par lequel le
// son passe réellement sous Windows :
//
//     EtageNatif → PuitsAnneauNatif → NativePcmRing → pop_pcm_bytes
//
// C'est-à-dire le puits que `BackendWasapi::puits` et `BackendAsio::puits`
// rendent VRAIMENT (`Puits::Natif(PuitsAnneauNatif::sur(…))`), posé sur
// l'anneau que le fil de rendu draine. Personne ne le montait dans un test.
// Ces gardes le montent, et vérifient que les octets source ressortent à
// l'identique de l'autre côté de l'anneau.
// ---------------------------------------------------------------------------

use super::etage_natif::{EtageNatif, PuitsAnneauNatif, spec_du_puits_natif};
use crate::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm, PuitsNatif};

/// Le DSP au repos : ni EQ, ni convolveur, ni crossfeed, volume à l'unité.
/// L'étage doit alors conserver les octets source bit à bit.
struct DspAuReposNatif {
    eq: std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: AtomicBool,
    mono_downmix: AtomicBool,
    volume: AtomicU32,
}

impl DspAuReposNatif {
    fn neuf() -> Self {
        Self {
            eq: std::sync::Mutex::new(None),
            convolver: std::sync::Mutex::new(None),
            crossfeed: std::sync::Mutex::new(None),
            pure_bypass: AtomicBool::new(false),
            mono_downmix: AtomicBool::new(false),
            volume: AtomicU32::new(1000),
        }
    }

    fn etage(&self, spec: AudioSpec) -> EtageNatif<'_> {
        EtageNatif::monter(
            spec,
            FormatOuvert::new(spec.cadence(), spec.canaux()),
            &self.volume,
            &self.eq,
            &self.convolver,
            &self.crossfeed,
            &self.pure_bypass,
            &self.mono_downmix,
        )
    }
}

fn spec_stereo(profondeur: ProfondeurPcm) -> AudioSpec {
    AudioSpec::nouvelle(44_100, profondeur, 2).expect("stéréo 44,1 kHz")
}

/// Monte la chaîne de production réelle et rend ce que le fil de rendu
/// lirait : `octets` traversent l'étage, le puits d'anneau, l'anneau, puis
/// `pop_pcm_bytes(bit_depth)` — la resérialisation par les octets HAUTS.
fn a_travers_l_anneau(octets: &[u8], profondeur: ProfondeurPcm) -> Vec<u8> {
    let spec = spec_stereo(profondeur);
    let dsp = DspAuReposNatif::neuf();
    let mut etage = dsp.etage(spec);

    // L'anneau et le puits que les deux bras Windows montent réellement.
    let anneau = Arc::new(NativePcmRing::new(octets.len() * 4));
    let (_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    let force_silent = AtomicBool::new(false);
    let mut puits = PuitsAnneauNatif::sur(
        anneau.clone(),
        spec_du_puits_natif(spec),
        &stop_rx,
        &paused,
        &force_silent,
    );

    let ecriture = etage.decoder_et_pousser(octets, &mut puits);
    assert!(
        !matches!(
            ecriture,
            super::etage_natif::EcritureNative::PuitsMort { .. }
        ),
        "le puits d'anneau natif est mort pendant la poussée : l'anneau n'a pas \
         accepté les mots de l'étage (REF-8, #2219)"
    );
    // La quarantaine 24 bits garde ses 32 premières trames tant que la sonde
    // DoP n'a pas conclu ; le reliquat part brut, comme en production.
    etage.vider(&mut puits);

    let mut sortie = vec![0u8; octets.len()];
    let ecrits = anneau.pop_pcm_bytes(&mut sortie, profondeur.bits_declares());
    sortie.truncate(ecrits);
    sortie
}

/// 16 bits identité : l'anneau rend les octets source, mot pour mot.
#[test]
fn la_chaine_de_production_natif_rend_le_16_bits_a_l_identique() {
    let source: Vec<u8> = (0..256u32)
        .flat_map(|i| ((i * 257) as u16).to_le_bytes())
        .collect();
    assert_eq!(
        a_travers_l_anneau(&source, ProfondeurPcm::Entier16),
        source,
        "EtageNatif → PuitsAnneauNatif → NativePcmRing → pop_pcm_bytes a changé un octet \
         en 16 bits : le rendu WASAPI/ASIO natif n'est plus bit-perfect (REF-8, #2219)"
    );
}

/// 24 bits identité : trois octets par mot, alignés à gauche dans l'anneau,
/// resérialisés par les octets HAUTS.
#[test]
fn la_chaine_de_production_natif_rend_le_24_bits_a_l_identique() {
    let source: Vec<u8> = (0..256u32)
        .flat_map(|i| {
            let mot = i * 65_793;
            [mot as u8, (mot >> 8) as u8, (mot >> 16) as u8]
        })
        .collect();
    assert_eq!(
        a_travers_l_anneau(&source, ProfondeurPcm::Entier24),
        source,
        "EtageNatif → PuitsAnneauNatif → NativePcmRing → pop_pcm_bytes a changé un octet \
         en 24 bits : le rendu WASAPI/ASIO natif n'est plus bit-perfect (REF-8, #2219)"
    );
}

/// Le porteur DoP versionné traverse la chaîne de production SANS qu'un
/// marqueur bouge. C'est la fixture réelle de l'encodeur
/// (`versioned_dop_fixture_is_the_real_encoder_output_byte_for_byte`), pas un
/// signal fabriqué ici.
#[test]
fn la_chaine_de_production_natif_porte_le_dop_intact() {
    let source: Vec<u8> = include_str!("../../../tests/fixtures/dop_stereo_24le_64frames.hex")
        .split_ascii_whitespace()
        .map(|octet| u8::from_str_radix(octet, 16).expect("fixture DoP hexadécimale valide"))
        .collect();
    assert_eq!(
        a_travers_l_anneau(&source, ProfondeurPcm::Entier24),
        source,
        "un marqueur DoP a bougé entre l'étage natif et `pop_pcm_bytes` : la route native \
         DÉTRUIT le porteur au lieu de le porter (REF-8, #2219)"
    );
}

/// CONTRE-ÉPREUVE du contrat de puits : un bloc dont le format contredit
/// celui de l'ouverture doit TUER le puits (`ecrire` rend `false`), et non
/// être poussé en silence dans l'anneau. Sans cette garde, un étage qui
/// livrerait des mots 16 bits à un puits ouvert en `Entier32` remplirait
/// l'anneau de bruit sans un mot dans le journal.
#[test]
fn le_puits_d_anneau_natif_refuse_un_bloc_qui_contredit_le_format_ouvert() {
    let spec = spec_stereo(ProfondeurPcm::Entier24);
    let anneau = Arc::new(NativePcmRing::new(1024));
    let (_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    let force_silent = AtomicBool::new(false);
    let mut puits = PuitsAnneauNatif::sur(
        anneau.clone(),
        spec_du_puits_natif(spec),
        &stop_rx,
        &paused,
        &force_silent,
    );

    // Le bon format passe.
    let mots = [0x1234_5600u32 as i32, -0x1234_5600i32];
    let octets: Vec<u8> = mots.iter().flat_map(|m| m.to_le_bytes()).collect();
    let bon = spec_du_puits_natif(spec);
    assert!(
        puits.ecrire(bon.bloc(&octets)),
        "le puits doit accepter un bloc au format qu'il a ouvert"
    );
    assert_eq!(
        anneau.available(),
        2,
        "les deux mots doivent être dans l'anneau"
    );

    // Le mauvais format tue le puits, et n'ajoute RIEN.
    let mauvais = spec_stereo(ProfondeurPcm::Entier16);
    assert!(
        !puits.ecrire(mauvais.bloc(&octets)),
        "un bloc qui contredit le format ouvert doit TUER le puits natif, pas être poussé \
         en silence (REF-8, #2219)"
    );
    assert_eq!(
        anneau.available(),
        2,
        "le bloc refusé ne doit pas avoir été poussé dans l'anneau"
    );
    // Le puits reste mort : il ne se rouvre pas au bloc suivant, même correct.
    assert!(
        !puits.ecrire(bon.bloc(&octets)),
        "un puits tué par une rupture de contrat doit le RESTER"
    );
}
