//! #7264 — **le volume logiciel appliqué au moment du rendu**, après l'anneau
//! natif, pour le WASAPI exclusif.
//!
//! # Le défaut
//!
//! Sous Windows, en WASAPI exclusif avec un volume logiciel (rc8.1, JeanPitre,
//! fil « le curseur de volume est bizarre »), un geste sur le curseur ne
//! s'entendait qu'environ **deux secondes** après. Le curseur et la commande
//! (`device_set_volume_sending`) partaient tout de suite ; c'était le SON qui
//! suivait en retard.
//!
//! La cause : l'étage natif ([`super::etage_natif::EtageNatif`]) pondérait les
//! mots par le volume **dans le producteur**, AVANT l'anneau natif
//! ([`super::NativePcmRing`]), et l'anneau du bras WASAPI
//! (`bras_wasapi.rs`, `anneau_pour`) contient `cadence × voies × 2`
//! échantillons, soit **deux secondes**. Le fil de rendu WASAPI ne faisait que
//! resérialiser des mots déjà pondérés : le nouveau gain attendait que les deux
//! secondes déjà pondérées à l'ancien gain soient jouées. Le tampon du pilote
//! (448 trames, ~10 ms dans le rapport du 27/10) n'y est pour rien.
//!
//! # Le remède
//!
//! Le même que celui du chemin natif ALSA S32 (#6734, `bras_alsa_natif.rs`) :
//! l'étage reçoit un volume constant à l'unité, et le **fil de rendu** pondère
//! les mots qu'il tire de l'anneau, période par période, avec le volume relu
//! à chaque réveil. Un geste prend effet à la période suivante du pilote, puis
//! glisse vers la nouvelle consigne en [`RAMPE_DU_VOLUME_MS`] — sans marche,
//! donc sans clic. Délai audible borné : une ou deux périodes de pilote
//! (~10 à 20 ms) plus la rampe, quelle que soit la profondeur de l'anneau.
//!
//! # Ce qui reste intact, au bit près
//!
//! * **Volume à 100 % et rampe au repos** : les mots sont resérialisés tels
//!   quels, exactement comme `pop_pcm_bytes` — le bit-perfect est entier.
//! * **DoP et reliquat forcé brut** : un porteur DoP ne supporte aucun facteur
//!   autre que 1. Le volume y est déjà à l'unité (`sync_volume_to_dop`), mais
//!   l'état DoP de la zone change au moment où le PRODUCTEUR le découvre,
//!   jusqu'à deux secondes avant que ces mots soient rendus : à un
//!   enchaînement DoP ↔ PCM dans le même flux, le rendu appliquerait le gain
//!   de l'un aux mots de l'autre. Le producteur **marque** donc dans l'anneau
//!   la position exacte où commence un passage intact (DoP) et celle où il
//!   finit ([`super::NativePcmRing::marquer_passage_intact`]) ; le rendu ne
//!   touche jamais un mot d'un passage intact, et reprend le PCM qui suit
//!   directement à la consigne courante (aucune rampe depuis l'unité).
//! * **Jamais plus fort que la consigne** : le gain est borné à `[0, 1]`, et
//!   chaque mot pondéré est arrondi à la profondeur déclarée puis borné à sa
//!   pleine échelle.

// Seul le fil de rendu WASAPI s'en sert hors des témoins.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Durée de la rampe d'un changement de volume au rendu. 20 ms : la même
/// durée que la rampe anti-« ploc » par défaut (#1590,
/// [`crate::audio::soft_mute::SOFT_MUTE_DEFAULT_MS`]) — à 44,1 kHz, 882
/// trames, soit un pas de gain sous −58 dB même pour un saut de 0 à 100 %, et
/// bien sous le seuil où un geste paraît mou.
pub(crate) const RAMPE_DU_VOLUME_MS: u32 = 20;

/// Le volume effectif qui laisse les mots intacts (`1000` = 100 %).
const UNITE: u32 = 1000;

/// L'état du gain tenu par un fil de rendu natif. Ni allocation ni verrou :
/// trois `f64`, un compteur de trames et deux drapeaux.
pub(crate) struct GainAuRendu {
    /// Le volume effectif de la sortie (utilisateur × ReplayGain, plafonné à
    /// l'unité par `effective_volume_units`), en millièmes. Relu à chaque
    /// période.
    volume: Arc<AtomicU32>,
    /// La consigne courante, dans `[0, 1]`.
    cible: f64,
    /// Le gain appliqué à la trame en cours.
    courant: f64,
    /// L'incrément par trame de la rampe en cours (toujours positif).
    pas: f64,
    trames_de_rampe: f64,
    canaux: u64,
    /// Le passage en cours de rendu est-il intact (DoP, reliquat brut) ?
    intact: bool,
}

impl GainAuRendu {
    /// Un gain au repos à la consigne courante : l'ouverture d'un flux ne
    /// fond jamais depuis l'unité (ce serait un éclat de la durée de la
    /// rampe à 100 %).
    pub(crate) fn nouveau(volume: Arc<AtomicU32>, cadence: u32, canaux: u32) -> Self {
        let cible = gain_de(volume.load(Ordering::Relaxed));
        let trames_de_rampe =
            (f64::from(cadence.max(1)) * f64::from(RAMPE_DU_VOLUME_MS) / 1000.0).max(1.0);
        Self {
            volume,
            cible,
            courant: cible,
            pas: 0.0,
            trames_de_rampe,
            canaux: u64::from(canaux.max(1)),
            intact: false,
        }
    }

    /// Le gain appliqué en ce moment (pour les témoins).
    #[cfg(test)]
    pub(crate) fn courant(&self) -> f64 {
        self.courant
    }

    /// Relit la consigne ; une consigne nouvelle arme une rampe de
    /// [`RAMPE_DU_VOLUME_MS`] depuis le gain atteint.
    pub(crate) fn relire_la_consigne(&mut self) {
        let cible = gain_de(self.volume.load(Ordering::Relaxed));
        if cible != self.cible {
            self.cible = cible;
            self.pas = (cible - self.courant).abs() / self.trames_de_rampe;
        }
    }

    /// Entrée dans un passage marqué par le producteur. Sortir d'un passage
    /// intact reprend directement à la consigne : le PCM qui suit un DoP
    /// n'est jamais rendu, même 20 ms, au-dessus de ce que l'utilisateur a
    /// demandé.
    pub(crate) fn entrer_dans_le_passage(&mut self, intact: bool) {
        if self.intact && !intact {
            self.courant = self.cible;
            self.pas = 0.0;
        }
        self.intact = intact;
    }

    /// Le mot rendu pour `mot` (aligné à gauche, `bits` déclarés), à la
    /// position absolue `position` de l'anneau. La rampe avance aux
    /// frontières de trame : tous les canaux d'une trame reçoivent le même
    /// facteur.
    #[inline]
    pub(crate) fn rendre(&mut self, mot: i32, bits: u16, position: u64) -> i32 {
        if self.intact {
            return mot;
        }
        let rendu = if self.courant == 1.0 {
            mot
        } else {
            ponderer(mot, bits, self.courant)
        };
        if position % self.canaux == self.canaux - 1 && self.courant != self.cible {
            self.courant = if self.cible > self.courant {
                (self.courant + self.pas).min(self.cible)
            } else {
                (self.courant - self.pas).max(self.cible)
            };
        }
        rendu
    }
}

/// Les millièmes de volume en gain, bornés à `[0, 1]` : jamais plus fort que
/// la pleine échelle, quel que soit ce qui est stocké.
fn gain_de(volume_units: u32) -> f64 {
    f64::from(volume_units.min(UNITE)) / f64::from(UNITE)
}

/// Pondère un mot aligné à gauche à sa profondeur déclarée : le mot utile est
/// multiplié en `f64` (exact pour 32 bits), arrondi, borné à la pleine
/// échelle de `bits`, puis réaligné à gauche — les bits bas inoccupés restent
/// nuls, comme `pop_pcm_bytes` l'attend.
#[inline]
fn ponderer(mot: i32, bits: u16, gain: f64) -> i32 {
    let bits = u32::from(bits.clamp(8, 32));
    let decalage = 32 - bits;
    let utile = f64::from(mot >> decalage);
    let max = ((1i64 << (bits - 1)) - 1) as f64;
    let min = -max - 1.0;
    let v = (utile * gain).round().clamp(min, max) as i32;
    v << decalage
}

#[cfg(test)]
mod tests {
    use super::super::NativePcmRing;
    use super::*;

    const CADENCE: u32 = 44_100;
    const VOIES: u32 = 2;
    /// Une période du pilote WASAPI du rapport du 27/10 : 448 trames.
    const PERIODE: usize = 448;

    /// Un mot 24 bits constant, aligné à gauche : `0x40_0000` (−6 dBFS).
    const MOT_24: i32 = 0x40_0000 << 8;

    fn anneau_de_deux_secondes() -> NativePcmRing {
        NativePcmRing::new((CADENCE * VOIES * 2) as usize)
    }

    /// Remplit l'anneau de `trames` trames du mot constant.
    fn remplir(anneau: &NativePcmRing, trames: usize) {
        let mots = vec![MOT_24; trames * VOIES as usize];
        assert_eq!(anneau.push(&mots), mots.len(), "l'anneau doit tout prendre");
    }

    /// Rend une période et la relit en mots 24 bits signés (valeur utile).
    fn rendre_une_periode(anneau: &NativePcmRing, gain: &mut GainAuRendu) -> Vec<i32> {
        let mut octets = vec![0xAAu8; PERIODE * VOIES as usize * 3];
        let ecrits = anneau.pop_pcm_bytes_au_gain(&mut octets, 24, gain);
        octets[ecrits..].fill(0);
        octets
            .chunks_exact(3)
            .map(|o| i32::from_le_bytes([0, o[0], o[1], o[2]]) >> 8)
            .collect()
    }

    /// LE contrat de #7264 : un geste sur le volume s'entend en 50 ms au plus
    /// — le gain demandé est atteint sur les échantillons rendus — que
    /// l'anneau soit presque vide ou plein de ses deux secondes.
    #[test]
    fn un_geste_sur_le_volume_s_entend_en_moins_de_50_ms_quelle_que_soit_la_profondeur_de_l_anneau()
    {
        for trames_en_attente in [CADENCE as usize / 10, (CADENCE * 2) as usize] {
            let anneau = anneau_de_deux_secondes();
            remplir(&anneau, trames_en_attente);
            let volume = Arc::new(AtomicU32::new(1000));
            let mut gain = GainAuRendu::nouveau(volume.clone(), CADENCE, VOIES);

            let avant = rendre_une_periode(&anneau, &mut gain);
            assert!(avant.iter().all(|&m| m == 0x40_0000), "à 100 % : intact");

            // Le geste : 100 % → 50 %.
            volume.store(500, Ordering::Relaxed);
            let borne_trames = (CADENCE as usize * 50) / 1000;
            let mut rendues = 0usize;
            let mut premiere_periode = None;
            let mut atteint_a = None;
            while rendues < borne_trames + PERIODE {
                let periode = rendre_une_periode(&anneau, &mut gain);
                if premiere_periode.is_none() {
                    premiere_periode = Some(periode.clone());
                }
                for (trame, voies) in periode.chunks_exact(VOIES as usize).enumerate() {
                    if atteint_a.is_none() && voies.iter().all(|&m| m == 0x20_0000) {
                        atteint_a = Some(rendues + trame);
                    }
                }
                rendues += PERIODE;
            }
            let premiere = premiere_periode.expect("une période rendue");
            assert!(
                premiere[premiere.len() - 1] < 0x40_0000,
                "anneau de {trames_en_attente} trames : la baisse commence dès la période suivante"
            );
            let atteint_a = atteint_a.unwrap_or_else(|| {
                panic!("anneau de {trames_en_attente} trames : 50 % jamais atteint en 50 ms")
            });
            assert!(
                atteint_a <= borne_trames,
                "anneau de {trames_en_attente} trames : 50 % atteint après {atteint_a} trames \
                 (> {borne_trames}, soit 50 ms)"
            );
        }
    }

    /// La rampe ne fait jamais de marche : d'une trame à la suivante, l'écart
    /// est d'au plus un pas de rampe (+1 d'arrondi), monotone vers la cible.
    #[test]
    fn la_rampe_du_volume_glisse_sans_marche() {
        let anneau = anneau_de_deux_secondes();
        remplir(&anneau, CADENCE as usize);
        let volume = Arc::new(AtomicU32::new(1000));
        let mut gain = GainAuRendu::nouveau(volume.clone(), CADENCE, VOIES);
        volume.store(0, Ordering::Relaxed);
        let mut gauche = Vec::new();
        for _ in 0..4 {
            gauche.extend(
                rendre_une_periode(&anneau, &mut gain)
                    .chunks_exact(2)
                    .map(|t| {
                        assert_eq!(t[0], t[1], "même facteur pour les voies d'une trame");
                        t[0]
                    }),
            );
        }
        let trames_de_rampe = (CADENCE * RAMPE_DU_VOLUME_MS / 1000) as f64;
        let pas_max = (f64::from(0x40_0000) / trames_de_rampe).ceil() as i32 + 1;
        for fenetre in gauche.windows(2) {
            assert!(fenetre[1] <= fenetre[0], "monotone vers le silence");
            assert!(
                fenetre[0] - fenetre[1] <= pas_max,
                "marche de {} > {pas_max}",
                fenetre[0] - fenetre[1]
            );
        }
        assert_eq!(*gauche.last().unwrap(), 0, "le silence est atteint");
    }

    /// Bit-perfect : à 100 %, le rendu au gain est octet pour octet celui de
    /// `pop_pcm_bytes`, en 16, 24 et 32 bits, sur des mots quelconques.
    #[test]
    fn a_cent_pour_cent_le_rendu_est_celui_de_pop_pcm_bytes_au_bit_pres() {
        for bits in [16u16, 24, 32] {
            let mut etat: u32 = 0x2545_f491;
            let decalage = 32 - u32::from(bits);
            let mots: Vec<i32> = (0..4096)
                .map(|_| {
                    etat = etat.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    ((etat as i32) >> decalage) << decalage
                })
                .collect();
            let reference = NativePcmRing::new(mots.len());
            let au_gain = NativePcmRing::new(mots.len());
            reference.push(&mots);
            au_gain.push(&mots);
            let octets = mots.len() * usize::from(bits / 8);
            let mut attendu = vec![0u8; octets];
            let mut obtenu = vec![0u8; octets];
            assert_eq!(reference.pop_pcm_bytes(&mut attendu, bits), octets);
            let mut gain = GainAuRendu::nouveau(Arc::new(AtomicU32::new(1000)), CADENCE, VOIES);
            assert_eq!(
                au_gain.pop_pcm_bytes_au_gain(&mut obtenu, bits, &mut gain),
                octets
            );
            assert_eq!(obtenu, attendu, "{bits} bits : un octet a changé à 100 %");
        }
    }

    /// Un passage DoP marqué intact n'est jamais pondéré, même avec un volume
    /// bas ; le PCM qui le précède et celui qui le suit le sont, à la
    /// consigne, à la position exacte de la marque — sans rampe depuis
    /// l'unité.
    #[test]
    fn un_passage_intact_n_est_jamais_pondere_et_le_pcm_autour_l_est_au_mot_pres() {
        let anneau = anneau_de_deux_secondes();
        let pcm = vec![MOT_24; 10 * VOIES as usize];
        let dop = vec![0x05_1234 << 8, 0x05_5678 << 8, (0xFA_1234_u32 << 8) as i32];
        let dop: Vec<i32> = dop
            .iter()
            .cycle()
            .take(6 * VOIES as usize)
            .copied()
            .collect();
        anneau.marquer_passage_intact(false);
        anneau.push(&pcm);
        anneau.marquer_passage_intact(true);
        anneau.push(&dop);
        anneau.marquer_passage_intact(false);
        anneau.push(&pcm);

        let volume = Arc::new(AtomicU32::new(300));
        let mut gain = GainAuRendu::nouveau(volume, CADENCE, VOIES);
        let total = pcm.len() * 2 + dop.len();
        let mut octets = vec![0u8; total * 4];
        assert_eq!(
            anneau.pop_pcm_bytes_au_gain(&mut octets, 32, &mut gain),
            total * 4
        );
        let rendus: Vec<i32> = octets
            .chunks_exact(4)
            .map(|o| i32::from_le_bytes([o[0], o[1], o[2], o[3]]))
            .collect();
        let pondere = ponderer(MOT_24, 32, 0.3);
        assert!(rendus[..pcm.len()].iter().all(|&m| m == pondere));
        assert_eq!(
            &rendus[pcm.len()..pcm.len() + dop.len()],
            &dop[..],
            "DoP intact"
        );
        assert!(
            rendus[pcm.len() + dop.len()..]
                .iter()
                .all(|&m| m == pondere),
            "le PCM après le DoP reprend à 30 %, pas à 100 %"
        );
    }

    /// Une marque posée pendant qu'une période est déjà en cours de rendu
    /// (position future) n'agit qu'à sa position.
    #[test]
    fn une_marque_au_dela_de_la_periode_attend_sa_position() {
        let anneau = anneau_de_deux_secondes();
        anneau.push(&vec![MOT_24; 8]);
        anneau.marquer_passage_intact(true);
        let volume = Arc::new(AtomicU32::new(500));
        let mut gain = GainAuRendu::nouveau(volume, CADENCE, VOIES);
        let mut octets = vec![0u8; 4 * 4];
        anneau.pop_pcm_bytes_au_gain(&mut octets, 32, &mut gain);
        anneau.push(&vec![MOT_24; 4]);
        let mut suite = vec![0u8; 8 * 4];
        anneau.pop_pcm_bytes_au_gain(&mut suite, 32, &mut gain);
        let mots = |o: &[u8]| -> Vec<i32> {
            o.chunks_exact(4)
                .map(|o| i32::from_le_bytes([o[0], o[1], o[2], o[3]]))
                .collect()
        };
        let demi = ponderer(MOT_24, 32, 0.5);
        assert_eq!(mots(&octets), vec![demi; 4]);
        assert_eq!(
            mots(&suite),
            vec![demi, demi, demi, demi, MOT_24, MOT_24, MOT_24, MOT_24]
        );
    }

    /// Jamais plus fort que la pleine échelle ni que la consigne : un volume
    /// stocké au-dessus de 1000 est rendu à l'unité, un mot pondéré reste
    /// dans la pleine échelle et garde ses bits bas nuls.
    #[test]
    fn le_gain_est_borne_et_le_mot_reste_aligne() {
        let mut gain = GainAuRendu::nouveau(Arc::new(AtomicU32::new(1500)), CADENCE, VOIES);
        assert_eq!(gain.courant(), 1.0);
        assert_eq!(gain.rendre(i32::MIN, 32, 0), i32::MIN);
        for bits in [16u16, 24, 32] {
            let decalage = 32 - u32::from(bits);
            let max = i32::MAX >> decalage << decalage;
            for g in [0.0, 0.1, 0.5, 0.999] {
                for mot in [max, i32::MIN, 0, 1 << decalage, -(1 << decalage)] {
                    let p = ponderer(mot, bits, g);
                    assert_eq!(p & ((1i64 << decalage) - 1) as i32, 0, "bits bas nuls");
                    assert!(
                        i64::from(p).abs() <= i64::from(mot).abs(),
                        "jamais plus fort"
                    );
                }
            }
        }
        assert_eq!(ponderer(MOT_24, 24, 0.5), 0x20_0000 << 8);
    }

    /// Le défaut, tenu pour qu'il ne revienne pas : le bras WASAPI monte son
    /// étage à l'UNITÉ et le fil de rendu applique le volume par
    /// `pop_pcm_bytes_au_gain`, sur le tampon amorcé comme dans la boucle.
    /// `bras_wasapi.rs` et le fil de rendu ne compilent que sous Windows :
    /// leur texte est la seule chose que Shrek puisse juger.
    #[test]
    fn le_bras_wasapi_monte_l_etage_a_l_unite_et_rend_le_volume_au_rendu() {
        let bras = include_str!("bras_wasapi.rs");
        assert!(
            bras.contains("let volume_de_l_etage = AtomicU32::new(VOLUME_A_L_UNITE);"),
            "l'étage WASAPI doit recevoir l'unité, sinon le volume attend l'anneau"
        );
        assert!(bras.contains("&volume_de_l_etage,"));
        assert!(bras.contains(".avec_le_gain_au_rendu(backend.anneau())"));
        assert!(bras.contains("regler_le_volume_au_rendu(demande.volume.clone())"));
        let rendu = include_str!("../wasapi_exclusive.rs");
        assert_eq!(
            rendu.matches("pop_pcm_bytes_au_gain(").count(),
            2,
            "l'amorce et la boucle de rendu passent par le gain au rendu"
        );
        assert!(
            !rendu.contains(".pop_pcm_bytes(out"),
            "plus de resérialisation sans gain"
        );
    }
}
