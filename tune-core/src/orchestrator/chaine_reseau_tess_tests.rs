//! TESS T04 — preuves de la chaîne réseau en `f64` à quantification unique.
//!
//! Sortie factice seulement : aucun son. Les signaux sont pseudo-aléatoires,
//! fixés par une graine.

use sha2::{Digest, Sha256};

use super::super::StreamingDsp;
use super::Passage;
use crate::audio::compensation_reseau::CompensationReseau;
use crate::audio::convolver::Convolver;
use crate::audio::crossfeed::CrossfeedProcessor;
use crate::tess::{EtapeFinale, FormatFlux, QuantificationPcm, SortieFinale};

/// La sonde : ce qui entre dans la quantification unique, tel quel, en `f64`.
pub(in crate::orchestrator) mod sonde {
    use std::cell::RefCell;

    thread_local! {
        static VU: RefCell<Option<Vec<f64>>> = const { RefCell::new(None) };
    }

    pub(in crate::orchestrator) fn noter(y: &[f64]) {
        VU.with(|v| {
            if let Some(v) = v.borrow_mut().as_mut() {
                v.extend_from_slice(y);
            }
        });
    }

    /// Arme la sonde, exécute `f`, rend ce qu'elle a vu.
    pub(super) fn voir(f: impl FnOnce()) -> Vec<f64> {
        VU.with(|v| *v.borrow_mut() = Some(Vec::new()));
        f();
        VU.with(|v| v.borrow_mut().take().unwrap_or_default())
    }
}

/// Générateur reproductible (xorshift).
struct Alea(u64);
impl Alea {
    fn suivant(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// `n` échantillons entiers (stéréo entrelacée) à `bits`, amplitude ≤ `ampl`.
fn source(bits: u16, n: usize, ampl: f64, graine: u64) -> Vec<u8> {
    let mut a = Alea(graine | 1);
    let echelle = (1i64 << (bits - 1)) as f64;
    let mut o = Vec::with_capacity(n * usize::from(bits / 8));
    for i in 0..n {
        // Un sinus et du bruit : un signal L ≠ R, jamais silencieux.
        let t = i as f64 / 2.0;
        let x = ampl * (0.6 * (t * 0.031).sin() + 0.4 * a.suivant());
        let v = (x * echelle).round() as i64;
        match bits {
            16 => o.extend_from_slice(&(v as i16).to_le_bytes()),
            24 => o.extend_from_slice(&(v as i32).to_le_bytes()[..3]),
            _ => o.extend_from_slice(&(v as i32).to_le_bytes()),
        }
    }
    o
}

fn lire(octets: &[u8], bits: u16) -> Vec<f64> {
    let w = usize::from(bits / 8);
    let mut v = vec![0.0; octets.len() / w];
    super::decoder_f64(octets, bits, &mut v);
    v
}

fn sha(o: &[u8]) -> [u8; 32] {
    Sha256::digest(o).into()
}

fn ecart_max(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len(), "longueurs différentes");
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f64::max)
}

fn porteur(replaygain: Option<f64>) -> StreamingDsp {
    StreamingDsp {
        replaygain,
        channels: 2,
        sample_rate: 48_000,
        ..Default::default()
    }
}

/// Le chemin d'AVANT T04, recopié : chaque étage relit et réécrit les octets.
fn ancien_chemin_gain_puis_compensation(
    pcm: &mut [u8],
    bits: u16,
    facteur: f64,
    comp: &mut CompensationReseau,
) {
    crate::audio::replaygain::apply_gain_pcm(pcm, bits, facteur);
    comp.process_pcm(pcm, bits);
}

/// Socle — bit-perfect à l'octet. Chaîne vide (aucun étage) : pas un octet
/// touché. Chaîne qui ne change aucun échantillon (gain unité) : elle passe
/// RÉELLEMENT par le décodage `f64`, l'exécuteur et la quantification, et la
/// sortie a le même sha256 que la source, en 16, 24 et 32 bits, piste entière
/// (fichier ré-encodé, DLNA) et flux (relais progressif, OAAT). Contre-épreuve :
/// un gain de −1 dB change le hash.
#[test]
fn bit_perfect_a_l_octet_chaine_vide_et_chaine_neutre() {
    for bits in [16u16, 24, 32] {
        let src = source(bits, 20_000, 0.9, 7);
        // Chaîne vide.
        let mut pcm = src.clone();
        let mut vide = StreamingDsp::default();
        assert!(!vide.is_active());
        vide.process(&mut pcm, bits);
        assert_eq!(sha(&pcm), sha(&src), "{bits} bits : chaîne vide");
        // Chaîne neutre, piste entière : elle passe par la quantification.
        let mut neutre = porteur(Some(1.0));
        assert!(neutre.is_active());
        let mut pcm = src.clone();
        let vu = sonde::voir(|| neutre.process(&mut pcm, bits));
        assert_eq!(
            vu.len(),
            src.len() / usize::from(bits / 8),
            "la chaîne a tourné"
        );
        assert_eq!(sha(&pcm), sha(&src), "{bits} bits : chaîne neutre, piste");
        // Chaîne neutre, flux en tampons successifs.
        let mut neutre = porteur(Some(1.0));
        let mut sortie = Vec::new();
        for morceau in src.chunks(4 * 1000 * usize::from(bits / 8) / 2) {
            let mut m = morceau.to_vec();
            neutre.process_flux(&mut m, bits);
            sortie.extend_from_slice(&m);
        }
        sortie.extend(neutre.terminer_flux(bits));
        assert_eq!(sha(&sortie), sha(&src), "{bits} bits : chaîne neutre, flux");
        // Contre-épreuve : un étage réel.
        let mut moins_1db = porteur(Some(10f64.powf(-1.0 / 20.0)));
        let mut pcm = src.clone();
        moins_1db.process(&mut pcm, bits);
        assert_ne!(
            sha(&pcm),
            sha(&src),
            "{bits} bits : −1 dB doit changer le hash"
        );
    }
}

/// « 64 bits réels » — ReplayGain puis compensation (deux étages `f64`
/// natifs) sur une source 24 bits : ce qui entre dans la quantification est
/// la référence calculée hors ligne en double précision, à moins de 1e-12.
/// Contre-épreuve dans le test : l'ancien chemin (chaque étage requantifie à
/// 24 bits) s'en écarte de plus de 1e-9.
#[test]
fn soixante_quatre_bits_reels_sur_la_chaine_reseau() {
    let bits = 24;
    let src = source(bits, 30_000, 0.95, 11);
    let x = lire(&src, bits);
    let facteur = 0.7;
    let mut chaine = porteur(Some(facteur));
    chaine.compensation = CompensationReseau::depuis_db(2.5);
    let mut pcm = src.clone();
    let vu = sonde::voir(|| chaine.process(&mut pcm, bits));

    // Référence hors ligne : gain, crête de la piste, gain de compensation.
    let apres_rg: Vec<f64> = x.iter().map(|v| v * facteur).collect();
    let mut comp = CompensationReseau::depuis_db(2.5).unwrap();
    comp.retenir_pour_crete(apres_rg.iter().fold(0.0, |m, v| m.max(v.abs())));
    let g = comp.gain_lineaire();
    let reference: Vec<f64> = apres_rg.iter().map(|v| v * g).collect();
    let e = ecart_max(&vu, &reference);
    assert!(
        e < 1e-12,
        "écart {e:e} : la chaîne réseau n'est pas en 64 bits"
    );

    // Contre-épreuve : l'ancien chemin d'octets.
    let mut ancien = src.clone();
    let mut comp_ancien = CompensationReseau::depuis_db(2.5).unwrap();
    ancien_chemin_gain_puis_compensation(&mut ancien, bits, facteur, &mut comp_ancien);
    let e_ancien = ecart_max(&lire(&ancien, bits), &reference);
    assert!(
        e_ancien > 1e-9,
        "l'ancien chemin devait s'écarter de la référence (écart {e_ancien:e}) : le banc ne discrimine pas"
    );
}

/// Une SEULE quantification : la sortie est, bit pour bit, la quantification
/// unique (TPDF, `QuantificationPcm` neuve) de la référence `f64` hors ligne.
/// Deux requantifications (l'ancien chemin) ne la rendent pas.
#[test]
fn une_seule_quantification_a_la_fin() {
    for bits in [16u16, 24] {
        let src = source(bits, 24_000, 0.8, 23);
        let x = lire(&src, bits);
        let facteur = 0.61;
        let mut chaine = porteur(Some(facteur));
        chaine.compensation = CompensationReseau::depuis_db(1.5);
        let mut pcm = src.clone();
        chaine.process(&mut pcm, bits);

        let apres_rg: Vec<f64> = x.iter().map(|v| v * facteur).collect();
        let mut comp = CompensationReseau::depuis_db(1.5).unwrap();
        comp.retenir_pour_crete(apres_rg.iter().fold(0.0, |m, v| m.max(v.abs())));
        let g = comp.gain_lineaire();
        let reference: Vec<f64> = apres_rg.iter().map(|v| v * g).collect();
        let format = FormatFlux {
            channels: 2,
            sample_rate_hz: 48_000,
        };
        let max = super::pas_de_tranche(2) / 2;
        let mut q = QuantificationPcm::new(true);
        q.preparer(format, max).unwrap();
        let mut attendu = vec![0u8; src.len()];
        let mut mots = vec![0i32; reference.len()];
        q.finaliser(
            &reference,
            reference.len() / 2,
            SortieFinale::Pcm {
                echantillons: &mut mots,
                bits: bits as u8,
            },
        )
        .unwrap();
        super::ecrire_mots(&mots, bits, &mut attendu);
        assert_eq!(
            sha(&pcm),
            sha(&attendu),
            "{bits} bits : la sortie n'est pas UNE quantification de la référence"
        );

        let mut ancien = src.clone();
        let mut comp_ancien = CompensationReseau::depuis_db(1.5).unwrap();
        ancien_chemin_gain_puis_compensation(&mut ancien, bits, facteur, &mut comp_ancien);
        assert_ne!(
            sha(&ancien),
            sha(&attendu),
            "{bits} bits : l'ancien chemin requantifie deux fois"
        );
    }
}

/// Aucune requantification entre deux étages : derrière le ReplayGain et un
/// convolveur réel (adaptateur `f32` provisoire), ce qui atteint la
/// quantification n'est PAS sur la grille 24 bits. Contre-épreuve : l'ancien
/// chemin, qui réécrivait des octets après chaque étage, y est partout.
#[test]
fn aucune_requantification_entre_deux_etages() {
    let bits = 24;
    let src = source(bits, 16_384, 0.7, 31);
    let ir = vec![
        (0..96).map(|i| 0.5f32 * 0.9f32.powi(i)).collect::<Vec<_>>(),
        (0..96)
            .map(|i| 0.4f32 * 0.85f32.powi(i))
            .collect::<Vec<_>>(),
    ];
    let mut chaine = porteur(Some(0.83));
    chaine.convolver = Some(Convolver::new(&ir, 64));
    let mut pcm = src.clone();
    let vu = sonde::voir(|| chaine.process(&mut pcm, bits));
    let hors_grille = |v: &[f64]| {
        v.iter()
            .filter(|s| (*s * 8_388_608.0).fract() != 0.0)
            .count()
    };
    let fins = hors_grille(&vu);
    assert!(
        fins > vu.len() * 9 / 10,
        "{fins} échantillons sur {} plus fins que 24 bits : un étage a requantifié",
        vu.len()
    );
    // Contre-épreuve : l'ancien chemin (octets après le gain, puis après le
    // convolveur).
    let mut ancien = src.clone();
    crate::audio::replaygain::apply_gain_pcm(&mut ancien, bits, 0.83);
    Convolver::new(&ir, 64).process_pcm(&mut ancien, bits);
    assert_eq!(hors_grille(&lire(&ancien, bits)), 0);
}

/// La latence du convolveur : la piste entière et le même signal livré en
/// tampons successifs (relais progressif) rendent les MÊMES octets, de la
/// même longueur, queue comprise — avec le convolveur, puis le crossfeed qui
/// le suit (la queue passe par lui).
#[test]
fn le_flux_en_tampons_rend_la_piste_entiere() {
    for bits in [16u16, 24] {
        let src = source(bits, 2 * 5_000, 0.6, 43);
        let ir = vec![
            (0..300)
                .map(|i| 0.3f32 * 0.98f32.powi(i))
                .collect::<Vec<_>>(),
            (0..300)
                .map(|i| 0.25f32 * 0.97f32.powi(i))
                .collect::<Vec<_>>(),
        ];
        let neuf = || {
            let mut d = porteur(Some(0.9));
            d.convolver = Some(Convolver::new(&ir, 64));
            d.crossfeed = Some(CrossfeedProcessor::new(48_000, 0.3, 0.3));
            d
        };
        let mut piste = src.clone();
        neuf().process(&mut piste, bits);
        assert_eq!(piste.len(), src.len());

        let trame = 2 * usize::from(bits / 8);
        let mut flux = neuf();
        let mut sortie = Vec::new();
        let mut reste: &[u8] = &src;
        for frames in [37usize, 1024, 513, 64, 2000].iter().cycle() {
            if reste.is_empty() {
                break;
            }
            let n = (frames * trame).min(reste.len());
            let mut m = reste[..n].to_vec();
            reste = &reste[n..];
            flux.process_flux(&mut m, bits);
            sortie.extend_from_slice(&m);
        }
        sortie.extend(flux.terminer_flux(bits));
        assert_eq!(sortie.len(), src.len(), "{bits} bits : longueur du flux");
        assert_eq!(sha(&sortie), sha(&piste), "{bits} bits : flux ≠ piste");
        assert_ne!(sha(&piste), sha(&src));
    }
}

/// Le porteur réutilisé pour une piste suivante (`process` appelé deux fois)
/// rend deux fois la même chose : moteurs remis à zéro et latence réarmée
/// après chaque piste. Seul le dither continue (générateur continu).
#[test]
fn deux_pistes_de_suite_ne_se_melangent_pas() {
    let bits = 16;
    let src = source(bits, 2 * 3_000, 0.5, 59);
    let ir = vec![vec![0.5f32, 0.25, 0.125], vec![0.5f32, 0.25, 0.125]];
    let mut d = porteur(None);
    d.convolver = Some(Convolver::new(&ir, 64));
    let mut a = src.clone();
    let vu_a = sonde::voir(|| d.process(&mut a, bits));
    let mut b = src.clone();
    let vu_b = sonde::voir(|| d.process(&mut b, bits));
    assert_eq!(vu_a, vu_b, "la seconde piste a hérité de la première");
    assert_eq!(a.len(), src.len());
}

/// Socle OAAT — le relais progressif (`spawn_streaming_dsp_relay`, celui des
/// bras WAV vers OAAT et DLNA) : en-tête intact, puis PCM bit-perfect à
/// l'octet avec une chaîne neutre ; contre-épreuve : −1 dB change le hash.
#[tokio::test]
async fn relais_progressif_bit_perfect_puis_contre_epreuve() {
    for (facteur, identique) in [(1.0, true), (10f64.powf(-1.0 / 20.0), false)] {
        let src = source(24, 2 * 6_000, 0.9, 71);
        let entete = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let amont = super::super::spawn_streaming_dsp_relay(porteur(Some(facteur)), 24, true, tx);
        amont.send(entete.clone()).await.unwrap();
        for m in src.chunks(6 * 1000) {
            amont.send(m.to_vec()).await.unwrap();
        }
        drop(amont);
        let mut recu = Vec::new();
        while let Some(m) = rx.recv().await {
            recu.push(m);
        }
        assert_eq!(recu[0], entete, "l'en-tête WAV ne passe pas par la chaîne");
        let pcm: Vec<u8> = recu[1..].concat();
        assert_eq!(pcm.len(), src.len());
        assert_eq!(sha(&pcm) == sha(&src), identique, "facteur {facteur}");
    }
}

/// Socle DLNA — le transcodage de fichier (`transcode_source_to_file`, FLAC
/// ou WAV ré-encodé pour un renderer) : sans étage, le PCM rendu est la source
/// à l'octet ; avec un gain unité (chaîne neutre, active), aussi ; avec
/// −1 dB, non.
#[tokio::test]
async fn transcodage_de_fichier_bit_perfect_puis_contre_epreuve() {
    let dossier = tempfile::tempdir().unwrap();
    let src = source(16, 2 * 8_000, 0.9, 83);
    let wav = dossier.path().join("source.wav");
    let mut f = Vec::new();
    let taille = src.len() as u32;
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + taille).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&44_100u32.to_le_bytes());
    f.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&taille.to_le_bytes());
    f.extend_from_slice(&src);
    std::fs::write(&wav, &f).unwrap();
    for (i, (rg, identique)) in [
        (None, true),
        (Some(1.0), true),
        (Some(10f64.powf(-1.0 / 20.0)), false),
    ]
    .into_iter()
    .enumerate()
    {
        let dest = dossier.path().join(format!("sortie-{i}.wav"));
        let (_, pcm, bd) = super::super::transcodage::transcode_source_to_file(
            wav.to_string_lossy().into_owned(),
            44_100,
            2,
            16,
            "wav".into(),
            None,
            None,
            rg,
            dest.to_string_lossy().into_owned(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(bd, 16);
        assert_eq!(pcm.len(), src.len());
        assert_eq!(sha(&pcm) == sha(&src), identique, "ReplayGain {rg:?}");
    }
}

/// Le passage par `Passage::Fin` sans convolveur ne rend rien, et une
/// profondeur inconnue laisse le tampon intact.
#[test]
fn cas_limites() {
    let mut d = porteur(Some(0.5));
    assert!(d.terminer_flux(16).is_empty());
    let src = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    let mut pcm = src.clone();
    d.process(&mut pcm, 20);
    assert_eq!(pcm, src);
    let _ = Passage::Piste;
}
