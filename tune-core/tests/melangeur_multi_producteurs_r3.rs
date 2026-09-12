//! #2219, tranche R3 — le mélangeur PCM rend un MÉLANGE, ou une erreur nommée.
//!
//! Deux témoins, deux défauts mesurés sur `main` (2b22a394) :
//!
//! 1. `PcmMixer::mix_buffers` répondait au bras `_` en rendant le PREMIER
//!    tampon. En 32 bits, le second producteur était **jeté en silence** :
//!    aucune erreur, aucune ligne de journal. Le défaut est dormant tant que
//!    l'unique appelant (`playback::dj_player`) fixe 16 bits, et il devient
//!    fatal au premier second producteur — c'est la tranche R4.
//! 2. `mix_buffers` allouait un `Vec<u8>` à CHAQUE appel, ce qui l'interdit
//!    dans un rappel temps réel. `mix_into` écrit dans un tampon fourni par
//!    l'appelant ; ce fichier compte ses allocations et exige zéro.
//!
//! Ce fichier est un binaire à lui seul parce qu'il pose un
//! `#[global_allocator]` : la caisse en porte déjà un dans la cible d'essais de
//! la bibliothèque (`outputs::local::ringbuf_tests`), et deux allocateurs
//! globaux dans le même binaire ne compilent pas — la porte clippy compile
//! justement `--all-targets` avec `local-audio`.
//!
//! `tune-core` porte `autotests = false` : sans la cible `[[test]]` déclarée
//! au manifeste, ce fichier ne serait jamais compilé et la porte rendrait un
//! vert contre rien.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use tune_core::audio::mixer::{MixError, PcmMixer};

// ---------------------------------------------------------------------------
// Compteur d'allocations
// ---------------------------------------------------------------------------

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

/// Exécute `operation` en comptant les allocations du THREAD courant.
///
/// Le drapeau est local au thread pour que les allocations des autres témoins,
/// joués en parallèle par le harnais, ne puissent pas fabriquer un faux rouge.
fn compter_les_allocations<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    // Toucher le TLS AVANT d'armer : son premier accès peut appartenir à
    // l'infrastructure de test, pas au chemin mesuré.
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
    TRACKED_ALLOCATIONS.store(0, Ordering::SeqCst);
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
    let resultat = operation();
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
    (resultat, TRACKED_ALLOCATIONS.load(Ordering::SeqCst))
}

// ---------------------------------------------------------------------------
// Témoin 1 — le second producteur n'est plus jeté en 32 bits
// ---------------------------------------------------------------------------

fn lire_i32(octets: &[u8], indice: usize) -> i32 {
    let o = indice * 4;
    i32::from_le_bytes([octets[o], octets[o + 1], octets[o + 2], octets[o + 3]])
}

#[test]
fn melange_32bits_ne_jette_pas_le_second_producteur() {
    let mixer = PcmMixer::new(1, 32, 44100);

    // Deux producteurs DISTINCTS : trois échantillons chacun, valeurs choisies
    // hors saturation pour que la somme soit vérifiable au bit près.
    let mut premier = Vec::new();
    let mut second = Vec::new();
    for (a, b) in [(1_000_000i32, 2_000_000i32), (-500_000, 250_000), (7, -7)] {
        premier.extend_from_slice(&a.to_le_bytes());
        second.extend_from_slice(&b.to_le_bytes());
    }

    let melange = mixer
        .mix_buffers(&[&premier, &second], &[1.0, 1.0])
        .expect("32 bits est une profondeur du contrat, le mélange doit aboutir");

    assert_ne!(
        melange, premier,
        "le mélangeur 32 bits a rendu le PREMIER tampon tel quel : le second \
         producteur est jeté en silence (bras `_` de mix_buffers)"
    );
    assert_eq!(
        melange.len(),
        premier.len(),
        "le mélange doit couvrir les mêmes échantillons que l'entrée"
    );

    for (i, attendu) in [3_000_000i32, -250_000, 0].into_iter().enumerate() {
        assert_eq!(
            lire_i32(&melange, i),
            attendu,
            "échantillon 32 bits n°{i} : le mélange doit être la SOMME des deux \
             producteurs, pas la copie du premier"
        );
    }
}

#[test]
fn melange_32bits_sature_au_lieu_de_boucler() {
    let mixer = PcmMixer::new(1, 32, 44100);
    let a = 2_000_000_000i32.to_le_bytes().to_vec();
    let b = 2_000_000_000i32.to_le_bytes().to_vec();

    let melange = mixer
        .mix_buffers(&[&a, &b], &[1.0, 1.0])
        .expect("32 bits doit être mélangé");

    assert_eq!(
        lire_i32(&melange, 0),
        i32::MAX,
        "la somme déborde : elle doit SATURER, jamais reboucler par le bas"
    );
}

#[test]
fn une_profondeur_hors_contrat_se_refuse_par_un_motif_nomme() {
    let mixer = PcmMixer::new(1, 8, 44100);
    let a = vec![10u8, 20, 30, 40];
    let b = vec![1u8, 2, 3, 4];

    let issue = mixer.mix_buffers(&[&a, &b], &[1.0, 1.0]);

    assert_eq!(
        issue,
        Err(MixError::UnsupportedBitDepth(8)),
        "8 bits n'est pas mélangeable ici : le mélangeur doit REFUSER, pas \
         rendre le premier tampon en silence"
    );

    let mut sortie = vec![0u8; 4];
    assert_eq!(
        mixer.mix_into(&[&a, &b], &[1.0, 1.0], &mut sortie),
        Err(MixError::UnsupportedBitDepth(8)),
        "mix_into doit refuser la même profondeur que mix_buffers"
    );
    assert_eq!(
        sortie,
        vec![0u8; 4],
        "un refus ne doit rien écrire dans le tampon de l'appelant"
    );
}

#[test]
fn un_tampon_de_sortie_trop_court_se_refuse_au_lieu_de_tronquer() {
    let mixer = PcmMixer::new(1, 32, 44100);
    let a = 1_000i32.to_le_bytes().to_vec();
    let b = 2_000i32.to_le_bytes().to_vec();

    let mut trop_court = [0u8; 3];
    assert_eq!(
        mixer.mix_into(&[&a, &b], &[1.0, 1.0], &mut trop_court),
        Err(MixError::OutputTooSmall {
            needed: 4,
            provided: 3
        }),
        "un tampon trop court doit être refusé, pas rempli à moitié"
    );
}

// ---------------------------------------------------------------------------
// Témoin 2 — la variante sans allocation n'alloue pas
// ---------------------------------------------------------------------------

#[test]
fn aucune_allocation_dans_mix_into() {
    let mixer = PcmMixer::new(2, 32, 44100);

    // 1 024 trames stéréo 32 bits : la taille d'un rappel temps réel réaliste.
    let echantillons = 1024 * 2;
    let mut premier = Vec::with_capacity(echantillons * 4);
    let mut second = Vec::with_capacity(echantillons * 4);
    for i in 0..echantillons {
        premier.extend_from_slice(&((i as i32) * 1_000).to_le_bytes());
        second.extend_from_slice(&((i as i32) * -3).to_le_bytes());
    }

    let entrees: [&[u8]; 2] = [&premier, &second];
    let gains = [0.5f32, 0.5];
    let taille = mixer
        .mixed_len(&entrees)
        .expect("32 bits est une profondeur du contrat");
    let mut sortie = vec![0u8; taille];

    // Un tour à blanc AVANT la mesure : ce qui s'initialiserait une seule fois
    // ne doit pas être imputé au chemin temps réel.
    mixer
        .mix_into(&entrees, &gains, &mut sortie)
        .expect("le tour à blanc doit aboutir");

    let (ecrits, allocations) =
        compter_les_allocations(|| mixer.mix_into(&entrees, &gains, &mut sortie));

    assert_eq!(
        allocations, 0,
        "mix_into a alloué {allocations} fois : la variante sans allocation \
         reste inutilisable dans un rappel temps réel"
    );
    assert_eq!(
        ecrits,
        Ok(taille),
        "mix_into doit avoir écrit tout le mélange"
    );

    // Et le contenu est bien un mélange, pas un tampon resté vierge.
    let dernier = echantillons as i32 - 1;
    let attendu_dernier = ((dernier * 1_000) as f64 * 0.5 + (dernier * -3) as f64 * 0.5) as i32;
    assert_eq!(
        lire_i32(&sortie, echantillons - 1),
        attendu_dernier,
        "le dernier échantillon doit porter la somme pondérée des DEUX entrées"
    );
}

#[test]
fn mix_into_et_mix_buffers_rendent_le_meme_melange() {
    for profondeur in [16u16, 24, 32] {
        let mixer = PcmMixer::new(2, profondeur, 44100);
        let largeur = (profondeur / 8) as usize;
        let premier: Vec<u8> = (0..largeur * 64).map(|i| (i % 251) as u8).collect();
        let second: Vec<u8> = (0..largeur * 64).map(|i| ((i * 7) % 241) as u8).collect();
        let entrees: [&[u8]; 2] = [&premier, &second];

        let alloue = mixer
            .mix_buffers(&entrees, &[0.75, 0.25])
            .unwrap_or_else(|e| panic!("{profondeur} bits refusé par mix_buffers : {e}"));
        let mut sortie = vec![0u8; alloue.len()];
        let ecrits = mixer
            .mix_into(&entrees, &[0.75, 0.25], &mut sortie)
            .unwrap_or_else(|e| panic!("{profondeur} bits refusé par mix_into : {e}"));

        assert_eq!(ecrits, alloue.len());
        assert_eq!(
            sortie, alloue,
            "les deux variantes doivent rendre le MÊME mélange en {profondeur} bits"
        );
    }
}
