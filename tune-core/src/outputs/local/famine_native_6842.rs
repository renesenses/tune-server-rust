//! #6842 — les deux défauts que REF-8 avait conservés dans le bras WASAPI
//! exclusif (`bras_wasapi.rs`), jugés sans carte son, sur Linux :
//!
//! 1. **famine muette** : la chaîne native (`chaine_native.rs`, la boucle du
//!    bras WASAPI et du bras ALSA S32) traitait un puits mort comme une
//!    poussée et lisait le reste du flux dans le vide. Elle s'arrête
//!    désormais sur le premier bloc refusé et le dit (`FinDeChaine::PuitsMort`),
//!    ce qui permet au bras de rapporter la famine ;
//! 2. **vidage sans borne** : le vidage du bras ignorait la borne de
//!    `drain_deadline_for` et, face à un rappel de rendu mort, ne finissait
//!    jamais. Il passe par `attendre_le_vidage`, borné comme celui d'ASIO.
//!
//! Le bras lui-même ne se compile que sous Windows (CI `windows-pr`) : ses
//! deux branchements sont tenus ici par une garde de texte.

use std::io::{Cursor, Read};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use super::chaine_native::{
    FinDeChaine, FinDuVidage, HoteDeChaineNative, ReserveDeLaChaine, Suivante, attendre_le_vidage,
    jouer_la_chaine_native,
};
use super::etage_natif::EtageNatif;
use super::*;
use crate::outputs::traits::{ProfondeurPcm, PuitsNatif};

// ─── Sorties factices ───────────────────────────────────────────────────────

struct Dsp {
    eq: std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: AtomicBool,
    mono_downmix: AtomicBool,
    volume: AtomicU32,
}

impl Dsp {
    fn au_repos() -> Self {
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

/// Un puits qui accepte `vivant_pour` blocs, puis meurt — comme
/// `PuitsAnneauNatif` quand le rappel de rendu ne draine plus l'anneau
/// pendant cinq secondes. Une fois mort, il refuse tout.
struct PuitsQuiMeurt {
    vivant_pour: usize,
    acceptes: usize,
    refuses: usize,
}

impl PuitsNatif for PuitsQuiMeurt {
    fn ecrire(&mut self, _bloc: BlocPcm<'_>) -> bool {
        if self.acceptes < self.vivant_pour {
            self.acceptes += 1;
            true
        } else {
            self.refuses += 1;
            false
        }
    }
}

/// Le flux HTTP de la piste, qui compte ce qu'on lui a lu.
struct FluxCompte<'a> {
    octets: Cursor<Vec<u8>>,
    lus: &'a AtomicUsize,
}

impl Read for FluxCompte<'_> {
    fn read(&mut self, tampon: &mut [u8]) -> std::io::Result<usize> {
        let n = self.octets.read(tampon)?;
        self.lus.fetch_add(n, Ordering::SeqCst);
        Ok(n)
    }
}

/// Un hôte sans réserve : une seule piste, aucun arrêt.
struct HoteSeul<'a> {
    _flux: std::marker::PhantomData<&'a ()>,
    positions: Vec<u64>,
}

impl<'a> ReserveDeLaChaine for HoteSeul<'a> {
    type Lecteur = FluxCompte<'a>;

    fn silence_force(&self) -> bool {
        false
    }

    fn preparer_la_suivante(&mut self) -> Suivante<Self::Lecteur> {
        Suivante::Aucune
    }

    fn piste_enchainee(&mut self) {}
}

impl HoteDeChaineNative for HoteSeul<'_> {
    fn arret_recu(&mut self) -> bool {
        false
    }

    fn publier_le_verdict(&mut self, _dop: bool, _bit_perfect: bool) {}

    fn publier_la_position(&mut self, position_ms: u64) {
        self.positions.push(position_ms);
    }
}

// ─── 1. La famine n'est plus muette ─────────────────────────────────────────

/// LE témoin de la famine : le puits meurt après deux blocs ; la chaîne doit
/// s'arrêter au premier bloc refusé, le dire, et ne pas lire le reste du
/// flux.
///
/// Avant #6842, `pousser` traitait `PuitsMort` comme une poussée : la chaîne
/// lisait les 4 Mio du flux jusqu'au bout, refusés bloc par bloc, rendait
/// `RienEnReserve` avec `http_eof = true` — une « fin naturelle » — et le
/// bras ne rapportait rien.
#[test]
fn la_chaine_s_arrete_sur_un_puits_mort_et_le_dit_6842() {
    let dsp = Dsp::au_repos();
    let spec = AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier16, 2).unwrap();
    let mut etage = dsp.etage(spec);
    let mut puits = PuitsQuiMeurt {
        vivant_pour: 2,
        acceptes: 0,
        refuses: 0,
    };
    // 4 Mio de PCM 16 bits stéréo (~22 s) : soixante-quatre lectures de
    // 64 Kio, quand la chaîne lit jusqu'au bout.
    let total = 4 * 1024 * 1024;
    let lus = AtomicUsize::new(0);
    let lecteur = FluxCompte {
        octets: Cursor::new(vec![0x11u8; total]),
        lus: &lus,
    };
    let mut hote = HoteSeul {
        _flux: std::marker::PhantomData,
        positions: Vec::new(),
    };

    let issue = jouer_la_chaine_native(&mut hote, lecteur, &mut etage, &mut puits, 0, 0);

    assert_eq!(
        issue.fin,
        FinDeChaine::PuitsMort,
        "#6842 : un puits mort doit arrêter la chaîne et être DIT, pour que \
         le bras rapporte la famine (record_feed_stall_failure) — la chaîne \
         a rendu {:?} après {} octets lus sur {total}",
        issue.fin,
        lus.load(Ordering::SeqCst)
    );
    assert!(
        !issue.http_eof,
        "un puits mort n'est pas une fin naturelle : le sondeur ne doit pas \
         avancer la file comme si la piste avait été jouée"
    );
    assert_eq!(
        puits.refuses, 1,
        "la chaîne s'arrête au PREMIER bloc refusé, elle ne pousse plus dans \
         le vide"
    );
    assert!(
        lus.load(Ordering::SeqCst) < total,
        "le reste du flux n'est pas lu : {} octets lus sur {total}",
        lus.load(Ordering::SeqCst)
    );
}

/// Le chemin vivant ne change pas : sans famine, la piste est lue jusqu'au
/// bout et finit naturellement.
#[test]
fn sans_famine_la_piste_finit_naturellement_6842() {
    let dsp = Dsp::au_repos();
    let spec = AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier16, 2).unwrap();
    let mut etage = dsp.etage(spec);
    let mut puits = PuitsQuiMeurt {
        vivant_pour: usize::MAX,
        acceptes: 0,
        refuses: 0,
    };
    let total = 256 * 1024;
    let lus = AtomicUsize::new(0);
    let lecteur = FluxCompte {
        octets: Cursor::new(vec![0x22u8; total]),
        lus: &lus,
    };
    let mut hote = HoteSeul {
        _flux: std::marker::PhantomData,
        positions: Vec::new(),
    };

    let issue = jouer_la_chaine_native(&mut hote, lecteur, &mut etage, &mut puits, 0, 0);

    assert_eq!(issue.fin, FinDeChaine::RienEnReserve);
    assert!(issue.http_eof);
    assert_eq!(puits.refuses, 0);
    assert_eq!(lus.load(Ordering::SeqCst), total);
    assert_eq!(issue.trames, (total / 4) as u64);
}

// ─── 2. Le vidage est borné ─────────────────────────────────────────────────

const PAS: Duration = Duration::from_millis(5);

/// LE témoin du vidage : un anneau que plus rien ne draine (rappel de rendu
/// mort) ne retient pas le fil de lecture au-delà de l'absence de progrès.
///
/// Avant #6842, le bras WASAPI bouclait « tant que l'anneau n'est pas vide »
/// et ce vidage ne finissait jamais (#3108).
#[test]
fn un_anneau_que_rien_ne_draine_rend_la_main_6842() {
    let debut = Instant::now();
    let fin = attendre_le_vidage(
        || 4_096,
        || false,
        || false,
        Duration::from_secs(30),
        Duration::from_millis(60),
        PAS,
    );
    assert_eq!(
        fin,
        FinDuVidage::SansProgres { restant: 4_096 },
        "#6842 : un anneau immobile doit rendre la main après l'absence de \
         progrès, pas attendre indéfiniment qu'il se vide"
    );
    assert!(
        debut.elapsed() < Duration::from_secs(5),
        "le vidage a duré {:?}",
        debut.elapsed()
    );
}

/// La borne de `drain_deadline_for` est respectée, même quand l'anneau
/// progresse encore (rappel très lent).
#[test]
fn la_borne_du_vidage_est_respectee_6842() {
    let restant = std::cell::Cell::new(1_000_000usize);
    let debut = Instant::now();
    let fin = attendre_le_vidage(
        || {
            restant.set(restant.get() - 1);
            restant.get()
        },
        || false,
        || false,
        Duration::from_millis(80),
        Duration::from_secs(30),
        PAS,
    );
    assert!(
        matches!(fin, FinDuVidage::BorneAtteinte { .. }),
        "#6842 : la borne du vidage doit être respectée — rendu {fin:?}"
    );
    assert!(debut.elapsed() < Duration::from_secs(5));
}

/// Un anneau qui se vide rend `Vide` ; un arrêt rend `Interrompu`.
#[test]
fn un_anneau_qui_se_vide_et_un_arret_6842() {
    let restant = std::cell::Cell::new(10usize);
    let fin = attendre_le_vidage(
        || {
            let r = restant.get().saturating_sub(1);
            restant.set(r);
            r
        },
        || false,
        || false,
        Duration::from_secs(30),
        Duration::from_millis(100),
        PAS,
    );
    assert_eq!(fin, FinDuVidage::Vide);

    let fin = attendre_le_vidage(
        || 4_096,
        || true,
        || false,
        Duration::from_secs(30),
        Duration::from_secs(30),
        PAS,
    );
    assert_eq!(fin, FinDuVidage::Interrompu);
}

/// La pause ne compte pas : un rappel en pause ne draine pas, il n'est pas
/// mort. Le vidage attend tant que la pause dure, puis reprend sa borne.
#[test]
fn la_pause_ne_compte_pas_dans_la_borne_6842() {
    let tours = std::cell::Cell::new(0u32);
    let fin = attendre_le_vidage(
        || 4_096,
        || false,
        || {
            tours.set(tours.get() + 1);
            // 40 pas en pause (bien plus que les 60 ms sans progrès), puis
            // le rappel reste immobile hors pause.
            tours.get() <= 40
        },
        Duration::from_secs(30),
        Duration::from_millis(60),
        PAS,
    );
    assert_eq!(fin, FinDuVidage::SansProgres { restant: 4_096 });
    assert!(
        tours.get() > 40,
        "le vidage a rendu la main PENDANT la pause ({} tours)",
        tours.get()
    );
}

// ─── 3. Le bras WASAPI est branché (garde de texte) ─────────────────────────

/// Les lignes de code (hors commentaires) d'un fichier.
fn code(source: &str) -> String {
    source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `bras_wasapi.rs` ne se compile que sous Windows : cette garde tient ses
/// deux branchements — le vidage passe par `attendre_le_vidage` avec la
/// borne, et la famine est rapportée au nom de « WASAPI ».
#[test]
fn le_bras_wasapi_borne_son_vidage_et_dit_sa_famine_6842() {
    let bras = code(include_str!("bras_wasapi.rs"));
    assert!(
        !bras.contains("_borne: std::time::Duration"),
        "#6842 : le vidage du bras WASAPI ignore encore sa borne"
    );
    assert_eq!(
        bras.matches("attendre_le_vidage(").count(),
        1,
        "#6842 : le vidage du bras WASAPI doit passer par attendre_le_vidage"
    );
    assert!(
        bras.contains("record_feed_stall_failure(\n            \"WASAPI\""),
        "#6842 : la famine du bras WASAPI doit être rapportée au nom du backend"
    );
    assert!(
        !bras.contains("| EcritureNative::PuitsMort"),
        "#6842 : l'amorce du bras WASAPI traite encore un puits mort comme une \
         poussée"
    );
}
