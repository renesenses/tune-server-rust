//! Dither TPDF — **une seule implémentation**, partagée par les trois étages
//! qui repassent du flottant à l'entier (#4075, #4076).
//!
//! # Pourquoi un dither, et pourquoi TPDF
//!
//! Repasser à l'entier sans rien ajouter laisse une erreur **corrélée au
//! signal** : ce n'est pas un bruit, c'est une distorsion. Elle s'entend là où
//! l'oreille est le plus sensible — fins de notes, queues de réverbération,
//! fondus — parce que c'est là que le signal n'occupe plus que quelques LSB.
//! La troncature vers zéro fait pire : son erreur porte le **signe du signal**,
//! donc elle est harmonique (défaut mesuré de #4076 : à −1 dB l'erreur suit le
//! signe ; à −0,000001 dB elle déplace 44 098 échantillons sur 44 098 d'un LSB
//! vers zéro). Le décalage arithmétique de #4075 fait la même chose vers −∞.
//!
//! Un bruit triangulaire de ±1 LSB, obtenu en soustrayant deux tirages
//! uniformes, ajouté **avant** l'arrondi :
//!
//! * décorrèle l'erreur du signal — elle redevient un bruit, pas une
//!   distorsion ;
//! * rend en plus la **puissance** de ce bruit indépendante du signal, ce
//!   qu'un bruit rectangulaire (RPDF, un seul tirage) ne fait pas : c'est la
//!   raison de prendre TPDF plutôt que RPDF, et elle ne coûte qu'un tirage de
//!   plus ;
//! * coûte +4,77 dB de plancher de bruit par rapport à un arrondi nu, soit
//!   ≈ −93 dBFS à 16 bits et ≈ −141 dBFS à 24 bits.
//!
//! C'est le choix déjà fait par `crate::audio::eq::EqProcessor` dans ce
//! dépôt. Ce module **est** ce dither : l'égaliseur l'appelle désormais d'ici
//! au lieu d'en garder une copie.
//!
//! # La règle qui va avec : pas de requantification, pas de dither
//!
//! Un dither ajoute du bruit. L'ajouter à un étage qui ne requantifie rien
//! serait une dégradation pure. Donc, sans exception :
//!
//! * gain exactement unitaire ⇒ aucun dither, les octets sortent tels quels ;
//! * élargissement de profondeur (16 → 24, 16 → 32, 24 → 32) ⇒ aucun dither :
//!   c'est un décalage exact, sans perte ;
//! * seules la **réduction** de profondeur et le **gain non unitaire**
//!   dithèrent.
//!
//! Les témoins `q4_*` de `tune-core/tests/marge_et_crete_2218.rs` tiennent
//! cette règle : un étage désarmé reste l'identité octet pour octet.
//!
//! # 🔴 Le dither est DÉTERMINISTE, et ce n'est pas un détail
//!
//! Un dither « vraiment aléatoire » casserait trois contrats de ce serveur, et
//! ces trois-là sont mesurables :
//!
//! 1. **Le cache de transcodage** (`transcode_cache.rs`) porte un nom dérivé de
//!    tout ce qui change les octets encodés, *pour qu'une requête identique
//!    retrouve le fichier fini*. Deux transcodages du même contenu doivent
//!    donner les mêmes octets.
//! 2. **Une rendition de cache peut être remplacée** (`rename` par-dessus)
//!    pendant qu'un renderer la lit. C'est bénin aujourd'hui *uniquement
//!    parce que* les deux productions sont identiques à l'octet ; avec un
//!    bruit tiré au hasard, cela deviendrait un saut audible et un
//!    `Content-Length` incohérent — le FLAC ne comprime pas deux bruits
//!    différents à la même taille.
//! 3. **La reprise par `Range`** de la sortie OAAT (`outputs/oaat/output.rs`,
//!    sur seek et sur erreur de corps mi-flux) exige que le deuxième appel
//!    continue le premier octet pour octet, sur une grille de trames calculée.
//!
//! D'où [`Dither::depuis_contenu`] : la graine est dérivée du **contenu du
//! bloc**, de l'étage et du paramètre de l'étage. Même bloc, même étage, même
//! gain ⇒ même bruit, toujours. Deux blocs différents ⇒ deux suites
//! différentes, donc aucune périodicité liée à la taille de bloc — une graine
//! fixe remise à zéro à chaque bloc ferait, elle, du bruit un motif répété à
//! la période du bloc, c'est-à-dire une raie, pas un bruit.
//!
//! L'étage entre dans la graine pour la même raison : ReplayGain et le
//! mélangeur peuvent s'appliquer aux mêmes échantillons dans le même bloc, et
//! deux étages qui ajouteraient le **même** bruit ne dithèrent plus, ils
//! corrèlent.
//!
//! # ⛔ Où ce dither ne doit PAS aller
//!
//! Surtout pas dans `decode::convert_pcm_bit_depth` ni dans
//! `decode::requantize` : ce sont les helpers du **décodage**, et
//! `audio::analyzer` les emprunte pour l'analyse ReplayGain, le BPM, la forme
//! d'onde et les empreintes. Y poser un dither rendrait l'analyse non
//! reproductible. Le dither de la réduction de profondeur vit dans le corps de
//! `decode::convert_pcm_bytes` — la porte du **transcodage** — et nulle part
//! ailleurs.
//!
//! Le convolveur n'est pas dans cette liste, délibérément : son défaut mesuré
//! (#4076, constat voisin) n'est pas une absence de dither mais une
//! **asymétrie de gain** — il décode en divisant par 2^(n−1) et réencode en
//! multipliant par 2^(n−1) − 1. Cela lui coûtait 1 LSB sur 66 % des
//! échantillons et empêchait une réponse impulsionnelle unité d'être
//! l'identité. C'est l'asymétrie qui est corrigée ; lui ajouter du bruit
//! détruirait justement l'identité qu'on vient de lui rendre.

/// Les étages qui requantifient. Chacun entre dans la graine pour que deux
/// étages successifs n'ajoutent jamais le même bruit aux mêmes échantillons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Etage {
    /// `crate::audio::replaygain::apply_gain_pcm`.
    ReplayGain,
    /// `crate::audio::mixer::PcmMixer::apply_gain`.
    Melangeur,
    /// `crate::audio::decode::convert_pcm_bytes`, réduction de profondeur.
    ReductionDeProfondeur,
}

impl Etage {
    const fn marque(self) -> u64 {
        match self {
            Etage::ReplayGain => 0x5245_5047_4149_4e00,
            Etage::Melangeur => 0x4d45_4c41_4e47_0000,
            Etage::ReductionDeProfondeur => 0x5245_4455_4954_0000,
        }
    }
}

/// Le générateur de bruit TPDF ±1 LSB.
///
/// Xorshift64\* : sans verrou, sans allocation, deux décalages et une
/// multiplication par tirage — tenable dans le chemin audio.
#[derive(Debug, Clone)]
pub struct Dither {
    etat: u64,
}

impl Dither {
    /// Une suite à graine explicite — ce que l'égaliseur tient par canal.
    ///
    /// La graine nulle est écartée : xorshift resterait bloqué sur zéro et ne
    /// produirait plus aucun bruit. Un dither muet est le défaut qu'on corrige.
    pub const fn depuis_graine(graine: u64) -> Self {
        Self {
            etat: if graine == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                graine
            },
        }
    }

    /// Graine dérivée du **contenu** — la porte des fonctions libres.
    ///
    /// `bloc` : les octets PCM qu'on s'apprête à requantifier.
    /// `parametre` : ce qui, en plus du contenu, change la sortie — le facteur
    /// de gain pour ReplayGain et le mélangeur, le couple de profondeurs pour
    /// la réduction.
    ///
    /// Même bloc, même étage, même paramètre ⇒ **même bruit**. Voir la note
    /// « le dither est DÉTERMINISTE » en tête de module : c'est ce qui garde
    /// le cache de transcodage et la reprise par `Range` exacts à l'octet.
    pub fn depuis_contenu(etage: Etage, bloc: &[u8], parametre: u64) -> Self {
        Self::depuis_graine(empreinte(etage, bloc, parametre))
    }

    /// Le dither d'un étage de **gain**, ou `None` quand ce gain ne
    /// requantifie rien.
    ///
    /// Un facteur entier — 1 (étage désarmé), 2 (+6,02 dB), 0 (coupure) —
    /// envoie un échantillon entier sur un entier : il ne perd rien, donc il
    /// n'a rien à dithérer, et lui ajouter du bruit serait une dégradation
    /// pure. C'est la règle « pas de requantification, pas de dither » de ce
    /// module, appliquée au seul cas où elle ne se voit pas au premier coup
    /// d'œil.
    ///
    /// Tous les autres facteurs — et un facteur ReplayGain, valant
    /// 10^(dB/20), n'est jamais entier en pratique — donnent un produit
    /// fractionnaire : ceux-là dithèrent.
    pub fn pour_facteur(etage: Etage, bloc: &[u8], facteur: f64) -> Option<Self> {
        if !facteur.is_finite() || facteur.fract() == 0.0 {
            return None;
        }
        Some(Self::depuis_contenu(etage, bloc, facteur.to_bits()))
    }

    /// Le prochain bruit triangulaire, dans [−1, 1] LSB, de moyenne nulle.
    pub fn tirer(&mut self) -> f64 {
        self.uniforme() - self.uniforme()
    }

    fn uniforme(&mut self) -> f64 {
        self.etat ^= self.etat >> 12;
        self.etat ^= self.etat << 25;
        self.etat ^= self.etat >> 27;
        let valeur = self.etat.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (valeur >> 11) as f64 * (1.0 / ((1_u64 << 53) as f64))
    }

    /// Quantifie une valeur déjà **exprimée en LSB de la cible** : dither,
    /// puis arrondi au plus proche, puis saturation dans `[min, max]`.
    pub fn quantifier(&mut self, ideal_lsb: f64, min: f64, max: f64) -> i64 {
        let bruit = self.tirer();
        quantifier_avec(ideal_lsb, bruit, min, max)
    }
}

/// Dither, arrondi, saturation — avec un bruit **fourni**.
///
/// L'ordre compte. Le bruit s'ajoute **avant** l'arrondi : c'est tout l'objet
/// d'un dither, l'ajouter après ne ferait qu'un bruit de plus. La saturation
/// vient en dernier, pour qu'un échantillon déjà au rail n'en sorte pas parce
/// que le bruit l'a poussé.
///
/// Séparée de [`Dither::quantifier`] pour les appelants qui tiennent leur
/// propre suite — l'égaliseur, une par canal — et pour rendre l'arrondi
/// témoignable avec un bruit nul.
pub fn quantifier_avec(ideal_lsb: f64, bruit_lsb: f64, min: f64, max: f64) -> i64 {
    (ideal_lsb + bruit_lsb).round().clamp(min, max) as i64
}

/// Empreinte bornée du bloc : FNV-1a sur la longueur, l'étage, le paramètre et
/// **au plus 1 024 octets** prélevés à pas constant sur tout le bloc.
///
/// Bornée à dessein : un pré-transcodage passe ici des tampons de plusieurs
/// centaines de mégaoctets, et la graine n'a pas besoin de les lire tous. Deux
/// blocs différents qui prélèveraient les mêmes octets tomberaient sur la même
/// suite — sans conséquence : ce sont deux blocs distincts, le bruit reste
/// décorrélé du signal de chacun. Ce qui compte, et qui est garanti, c'est
/// l'autre sens : le **même** bloc donne toujours la **même** suite.
fn empreinte(etage: Etage, bloc: &[u8], parametre: u64) -> u64 {
    const BASE: u64 = 0xcbf2_9ce4_8422_2325;
    const PREMIER: u64 = 0x0000_0100_0000_01b3;
    const PRELEVES: usize = 1024;

    let mut h = BASE;
    for mot in [etage.marque(), parametre, bloc.len() as u64] {
        for octet in mot.to_le_bytes() {
            h = (h ^ u64::from(octet)).wrapping_mul(PREMIER);
        }
    }
    if !bloc.is_empty() {
        let pas = bloc.len().div_ceil(PRELEVES).max(1);
        for octet in bloc.iter().step_by(pas) {
            h = (h ^ u64::from(*octet)).wrapping_mul(PREMIER);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le bruit reste dans ±1 LSB et sa moyenne est nulle : la définition d'un
    /// TPDF centré.
    #[test]
    fn le_bruit_tient_dans_un_lsb_et_sa_moyenne_est_nulle() {
        let mut d = Dither::depuis_graine(0x1234_5678_9abc_def0);
        let mut somme = 0.0;
        let mut max = 0.0f64;
        const N: usize = 200_000;
        for _ in 0..N {
            let b = d.tirer();
            assert!((-1.0..=1.0).contains(&b), "bruit hors ±1 LSB : {b}");
            somme += b;
            max = max.max(b.abs());
        }
        let moyenne = somme / N as f64;
        assert!(moyenne.abs() < 0.01, "dither non centré : {moyenne}");
        assert!(max > 0.9, "amplitude trop faible : {max}");
    }

    /// TPDF et non RPDF : la densité est triangulaire, donc les valeurs proches
    /// de 0 sont bien plus fréquentes que celles proches de ±1. Un tirage
    /// uniforme donnerait des parts égales.
    #[test]
    fn la_densite_est_triangulaire_pas_rectangulaire() {
        let mut d = Dither::depuis_graine(0xdead_beef_cafe_1234);
        let mut centre = 0usize;
        let mut bords = 0usize;
        const N: usize = 200_000;
        for _ in 0..N {
            let b = d.tirer().abs();
            if b < 0.25 {
                centre += 1;
            } else if b > 0.75 {
                bords += 1;
            }
        }
        assert!(
            centre > bords * 4,
            "densité non triangulaire : centre={centre}, bords={bords}"
        );
    }

    /// 🔴 Le contrat du cache de transcodage : même bloc, même étage, même
    /// paramètre ⇒ MÊME suite de bruit. Sans cela, deux transcodages du même
    /// contenu divergent et une reprise par `Range` saute.
    #[test]
    fn meme_contenu_meme_bruit() {
        let bloc: Vec<u8> = (0..4096u32).map(|i| (i * 37 % 251) as u8).collect();
        let suite = || -> Vec<f64> {
            let mut d =
                Dither::depuis_contenu(Etage::ReductionDeProfondeur, &bloc, (24 << 16) | 16);
            (0..256).map(|_| d.tirer()).collect()
        };
        assert_eq!(suite(), suite(), "deux passes divergent : cache cassé");
    }

    /// Deux blocs différents tirent des suites différentes : pas de motif
    /// répété à la période du bloc, donc pas de raie à la place du bruit.
    #[test]
    fn deux_blocs_differents_tirent_des_suites_differentes() {
        let a: Vec<u8> = (0..4096u32).map(|i| (i * 37 % 251) as u8).collect();
        let mut b = a.clone();
        b[2048] ^= 0x01;
        let tirer = |bloc: &[u8]| -> Vec<f64> {
            let mut d = Dither::depuis_contenu(Etage::ReductionDeProfondeur, bloc, 1);
            (0..256).map(|_| d.tirer()).collect()
        };
        assert_ne!(tirer(&a), tirer(&b), "deux blocs partagent leur bruit");
    }

    /// Deux étages sur le MÊME bloc tirent des suites indépendantes : sinon
    /// ils ne dithèrent plus, ils corrèlent.
    #[test]
    fn deux_etages_sur_le_meme_bloc_sont_independants() {
        let bloc: Vec<u8> = (0..2048u32).map(|i| (i % 253) as u8).collect();
        let tirer = |etage: Etage| -> Vec<f64> {
            let mut d = Dither::depuis_contenu(etage, &bloc, 7);
            (0..256).map(|_| d.tirer()).collect()
        };
        assert_ne!(tirer(Etage::ReplayGain), tirer(Etage::Melangeur));
        assert_ne!(
            tirer(Etage::ReplayGain),
            tirer(Etage::ReductionDeProfondeur)
        );
    }

    /// Le paramètre entre dans la graine : deux gains différents sur le même
    /// bloc ne partagent pas leur bruit.
    #[test]
    fn le_parametre_entre_dans_la_graine() {
        let bloc: Vec<u8> = (0..1024u32).map(|i| (i % 249) as u8).collect();
        let tirer = |p: u64| -> Vec<f64> {
            let mut d = Dither::depuis_contenu(Etage::ReplayGain, &bloc, p);
            (0..128).map(|_| d.tirer()).collect()
        };
        assert_ne!(tirer(0.5f64.to_bits()), tirer(0.25f64.to_bits()));
    }

    /// Une graine nulle ne doit pas éteindre le dither : xorshift resterait
    /// bloqué sur zéro et ne produirait plus rien.
    #[test]
    fn une_graine_nulle_ne_tue_pas_le_bruit() {
        let mut d = Dither::depuis_graine(0);
        assert!(
            (0..64).any(|_| d.tirer() != 0.0),
            "graine nulle : dither muet"
        );
    }

    /// Le dither enlève le biais que la troncature laisse : sur une rampe
    /// lente, l'erreur moyenne du dither est nulle, celle de la troncature est
    /// d'un demi-LSB.
    #[test]
    fn le_dither_enleve_le_biais_que_la_troncature_laisse() {
        let mut d = Dither::depuis_graine(0x5555_aaaa_5555_aaaa);
        let mut err_dither = 0.0;
        let mut err_troncature = 0.0;
        const N: usize = 100_000;
        for i in 0..N {
            let ideal = i as f64 * 0.001 + 0.3;
            err_dither += d.quantifier(ideal, -1e9, 1e9) as f64 - ideal;
            err_troncature += (ideal as i64) as f64 - ideal;
        }
        let biais_dither = err_dither / N as f64;
        let biais_troncature = err_troncature / N as f64;
        assert!(
            biais_dither.abs() < 0.02,
            "dither biaisé : {biais_dither:+.4} LSB"
        );
        assert!(
            biais_troncature < -0.4,
            "la troncature devrait biaiser d'un demi-LSB : {biais_troncature:+.4} LSB"
        );
    }

    /// La saturation vient APRÈS l'arrondi : un échantillon déjà au rail n'en
    /// sort pas parce que le bruit l'a poussé.
    #[test]
    fn la_saturation_tient_le_rail_malgre_le_bruit() {
        let mut d = Dither::depuis_graine(0x0f0f_0f0f_0f0f_0f0f);
        for _ in 0..10_000 {
            let haut = d.quantifier(32_767.0, -32_768.0, 32_767.0);
            assert!((-32_768..=32_767).contains(&haut), "hors rail : {haut}");
            let bas = d.quantifier(-32_768.0, -32_768.0, 32_767.0);
            assert!((-32_768..=32_767).contains(&bas), "hors rail : {bas}");
        }
    }

    /// Bruit nul ⇒ arrondi au plus proche pur. La porte qui rend l'arrondi
    /// témoignable sans le hasard.
    #[test]
    fn un_bruit_nul_rend_l_arrondi_au_plus_proche() {
        assert_eq!(quantifier_avec(1.5, 0.0, -100.0, 100.0), 2);
        assert_eq!(quantifier_avec(1.4, 0.0, -100.0, 100.0), 1);
        assert_eq!(quantifier_avec(-1.5, 0.0, -100.0, 100.0), -2);
        assert_eq!(quantifier_avec(-0.4, 0.0, -100.0, 100.0), 0);
        assert_eq!(quantifier_avec(1e9, 0.0, -100.0, 100.0), 100);
    }

    /// L'empreinte reste bornée : un tampon de pré-transcodage fait des
    /// centaines de mégaoctets, la graine n'a pas à les lire tous.
    #[test]
    fn l_empreinte_est_bornee_sur_un_gros_bloc() {
        let gros = vec![0xa5u8; 8 << 20];
        let a = empreinte(Etage::ReplayGain, &gros, 1);
        let b = empreinte(Etage::ReplayGain, &gros, 1);
        assert_eq!(a, b);
    }
}
