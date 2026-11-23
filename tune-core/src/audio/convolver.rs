// Code audio (décodage, analyse, traitement du signal) : les boucles indexées
// et les découpes par `chunks_exact` y sont gardées telles quelles. Les récrire
// (`as_chunks`, itérateurs, `repeat_n`) ne changerait rien au son mais toucherait
// la logique audio pour un gain de forme (clippy 1.98).
#![allow(
    clippy::needless_range_loop,
    clippy::manual_repeat_n,
    clippy::chunks_exact_to_as_chunks
)]

use crate::audio::dither::quantifier_avec;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::collections::VecDeque;
use std::sync::Arc;

/// #8761 — jeu de RI par cadence et rééchantillonnage en repli.
pub mod par_cadence;
pub use par_cadence::{ChoixRi, JeuDeZone};

#[cfg(test)]
#[path = "convolver/banc_f64_tests.rs"]
mod banc_f64_tests;

/// Réponse impulsionnelle en domaine temporel, indépendante d'un flux.
///
/// La configuration conserve volontairement les taps et leur cadence source :
/// une instance [`Convolver`] est un état de traitement lié à un nombre de
/// canaux et doit être reconstruite quand le format du flux change (#2210).
///
/// #8761 — elle porte un JEU de réponses, une par cadence (la principale en
/// tête), et le droit de rééchantillonner en repli (voir [`par_cadence`]).
#[derive(Clone)]
pub struct ConvolverConfig {
    impulse_response: Arc<[Vec<f32>]>,
    sample_rate: u32,
    /// Toutes les réponses du jeu, une par cadence, la principale en tête.
    variantes: Arc<[par_cadence::Variante]>,
    /// Rééchantillonner le fichier le plus proche quand aucune cadence ne
    /// correspond (défaut). Faux : le refus d'avant #8761.
    reechantillonner: bool,
    /// Le chemin du fichier principal : clé de l'état effectif publié.
    cle_jeu: Option<String>,
}

impl ConvolverConfig {
    pub fn new(impulse_response: Vec<Vec<f32>>, sample_rate: u32) -> Result<Self, String> {
        if sample_rate == 0 {
            return Err("la cadence de la réponse impulsionnelle doit être positive".into());
        }
        if impulse_response.is_empty() || impulse_response.iter().any(Vec::is_empty) {
            return Err("la réponse impulsionnelle doit contenir des taps sur chaque canal".into());
        }
        par_cadence::ri_admise(&impulse_response, sample_rate)?;
        let taps: Arc<[Vec<f32>]> = impulse_response.into();
        Ok(Self {
            variantes: vec![par_cadence::Variante::en_memoire(taps.clone(), sample_rate)].into(),
            impulse_response: taps,
            sample_rate,
            reechantillonner: true,
            cle_jeu: None,
        })
    }

    pub fn from_wav(path: &str) -> Result<Self, String> {
        Self::depuis_jeu(&[path.to_string()], true)
    }

    /// #8761 — charger un jeu : `chemins[0]` est le fichier principal (son
    /// échec est une erreur), les suivants sont les fichiers par cadence (un
    /// fichier illisible y est écarté avec un avertissement, sans priver la
    /// zone des autres). À cadence égale, le DERNIER fichier de la liste
    /// l'emporte : un fichier déposé « pour 48 kHz » remplace le principal
    /// s'il est lui aussi à 48 kHz.
    pub fn depuis_jeu(chemins: &[String], reechantillonner: bool) -> Result<Self, String> {
        let (principal, autres) = chemins
            .split_first()
            .ok_or("aucun fichier de réponse impulsionnelle")?;
        let tete = par_cadence::Variante::depuis_fichier(principal)?;
        let mut variantes = vec![tete.clone()];
        for chemin in autres {
            match par_cadence::Variante::depuis_fichier(chemin) {
                Ok(v) => {
                    variantes.retain(|x| x.sample_rate != v.sample_rate);
                    variantes.push(v);
                }
                Err(e) => tracing::warn!(chemin, error = %e, "convolver_ir_du_jeu_ecartee"),
            }
        }
        Ok(Self {
            impulse_response: tete.taps.clone(),
            sample_rate: tete.sample_rate,
            variantes: variantes.into(),
            reechantillonner,
            cle_jeu: Some(principal.clone()),
        })
    }

    /// Ajouter une RI en mémoire au jeu (tests, outils) ; même règle de
    /// remplacement à cadence égale que [`Self::depuis_jeu`].
    pub fn avec_variante(mut self, taps: Vec<Vec<f32>>, sample_rate: u32) -> Result<Self, String> {
        let autre = Self::new(taps, sample_rate)?;
        let mut v: Vec<_> = self
            .variantes
            .iter()
            .filter(|x| x.sample_rate != sample_rate)
            .cloned()
            .collect();
        v.extend(autre.variantes.iter().cloned());
        self.variantes = v.into();
        Ok(self)
    }

    pub fn avec_reechantillonnage(mut self, autorise: bool) -> Self {
        self.reechantillonner = autorise;
        self
    }

    pub fn reechantillonnage_autorise(&self) -> bool {
        self.reechantillonner
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Les cadences du jeu, dans l'ordre du jeu (principale en tête).
    pub fn cadences(&self) -> Vec<u32> {
        self.variantes.iter().map(|v| v.sample_rate).collect()
    }

    pub fn source_channels(&self) -> usize {
        self.impulse_response.len()
    }

    /// Quel fichier servirait un flux à `cible` Hz, SANS rien calculer.
    /// `Err` : aucun fichier à cette cadence et rééchantillonnage refusé.
    pub fn choix_pour(&self, cible: u32) -> Result<ChoixRi, String> {
        let (i, raison) = par_cadence::planifier(&self.variantes, cible)
            .ok_or("aucun fichier de réponse impulsionnelle")?;
        let v = &self.variantes[i];
        if v.sample_rate != cible && !self.reechantillonner {
            let cadences = self
                .cadences()
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "Réponse impulsionnelle à {cadences} Hz incompatible avec le flux à {cible} Hz ; réexportez le filtre à {cible} Hz (ou autorisez le rééchantillonnage)"
            ));
        }
        Ok(ChoixRi {
            cadence_flux: cible,
            cadence_fichier: v.sample_rate,
            chemin: v.chemin.clone(),
            reechantillonne: v.sample_rate != cible,
            raison,
        })
    }

    /// Le nombre de canaux de la RI qui servirait un flux à `cible` Hz.
    pub fn canaux_pour(&self, cible: u32) -> Option<usize> {
        par_cadence::planifier(&self.variantes, cible).map(|(i, _)| self.variantes[i].taps.len())
    }

    /// Le nombre de prises du FICHIER qui servirait un flux à `cible` Hz
    /// (avant tout rééchantillonnage).
    pub fn prises_pour(&self, cible: u32) -> Option<usize> {
        par_cadence::planifier(&self.variantes, cible)
            .and_then(|(i, _)| self.variantes[i].taps.first().map(Vec::len))
    }

    /// Les taps qui serviraient un flux à `cible` Hz (rééchantillonnés et mis
    /// en cache au besoin), avec le choix.
    pub fn ri_pour(&self, cible: u32) -> Result<(ChoixRi, Arc<[Vec<f32>]>), String> {
        let choix = self.choix_pour(cible)?;
        let (i, _) = par_cadence::planifier(&self.variantes, cible)
            .ok_or("aucun fichier de réponse impulsionnelle")?;
        let taps = par_cadence::ri_a_la_cadence(&self.variantes[i], cible)?;
        Ok((choix, taps))
    }

    /// Préparer le cache pour ces cadences (en tâche de fond chez l'appelant),
    /// pour qu'un changement de cadence en lecture ne paie pas le calcul.
    pub fn prechauffer(&self, cadences: &[u32]) {
        if !self.reechantillonner {
            return;
        }
        for &c in cadences {
            if let Some((i, _)) = par_cadence::planifier(&self.variantes, c) {
                if let Err(e) = par_cadence::ri_a_la_cadence(&self.variantes[i], c) {
                    tracing::warn!(cadence = c, error = %e, "convolver_ir_prechauffe_ecartee");
                }
            }
        }
    }

    /// Bâtir le moteur pour le format réellement négocié.
    ///
    /// #8761 — le fichier du jeu à la cadence du flux ; à défaut, le plus
    /// proche rééchantillonné (une fois, mis en cache), sauf si le réglage le
    /// refuse : l'utilisateur reçoit alors la cadence à laquelle réexporter
    /// son filtre. Une IR mono est explicitement dupliquée sur tous les
    /// canaux ; tout autre écart de layout est refusé.
    pub fn build_for(
        &self,
        block_size: usize,
        target_sample_rate: u32,
        target_channels: usize,
    ) -> Result<Convolver, String> {
        if target_channels == 0 {
            return Err("le flux ne contient aucun canal audio".into());
        }
        let (choix, taps) = self.ri_pour(target_sample_rate)?;
        let adapted = if taps.len() == target_channels {
            taps.to_vec()
        } else if taps.len() == 1 {
            vec![taps[0].clone(); target_channels]
        } else {
            return Err(format!(
                "Réponse impulsionnelle à {} canaux incompatible avec le flux à {target_channels} canaux ; fournissez une IR mono ou une IR à {target_channels} canaux",
                taps.len()
            ));
        };
        par_cadence::noter_le_choix(self.cle_jeu.as_deref(), &choix);
        Ok(Convolver::new(&adapted, block_size))
    }
}

/// Le type flottant d'un moteur de convolution.
///
/// TESS T09b — le moteur de la chaîne est [`Convolver`] en `f64` (le défaut) :
/// FFT, partitions de la réponse, ligne à retard fréquentielle, recouvrement
/// et sorties sont en double précision. L'instance `f32` n'existe plus que
/// pour le banc (la contre-épreuve « 64 bits réels » et le rapport de coût
/// processeur F64/F32) : aucun chemin de lecture ne la construit.
pub trait Flottant: realfft::FftNum + Default {
    fn de_f64(x: f64) -> Self;
    fn en_f64(self) -> f64;
}

impl Flottant for f64 {
    #[inline]
    fn de_f64(x: f64) -> Self {
        x
    }
    #[inline]
    fn en_f64(self) -> f64 {
        self
    }
}

impl Flottant for f32 {
    #[inline]
    fn de_f64(x: f64) -> Self {
        x as f32
    }
    #[inline]
    fn en_f64(self) -> f64 {
        f64::from(self)
    }
}

/// Réchauffe maximale d'un moteur qui en remplace un autre
/// ([`Convolver::remplacer_en_fondu`]), en trames : au-delà de 2^17 trames
/// (2,7 s à 48 kHz), la fin d'une réponse très longue est un résidu de
/// réverbération à plus de 60 dB sous le direct.
pub const RECHAUFFE_MAX: usize = 1 << 17;

/// Partitioned overlap-add FFT convolver for real-time FIR filtering.
///
/// Processes audio in fixed-size blocks using FFT convolution.
/// Supports arbitrary-length impulse responses by partitioning them
/// into segments and accumulating the results.
///
/// TESS T09b — générique sur le flottant, `f64` par défaut ; aucune
/// allocation après la construction (FFT avec tampons de travail fournis,
/// files amorcées et bornées), y compris pendant une bascule de filtre.
pub struct Convolver<T: Flottant = f64> {
    block_size: usize,
    fft_size: usize,
    channels: usize,
    /// FFT of each partition of the impulse response, per channel.
    ir_partitions: Vec<Vec<Vec<Complex<T>>>>,
    /// Prises de la plus longue réponse (réchauffe d'une bascule).
    prises: usize,
    /// Input buffer per channel (collects samples until block_size is reached).
    input_buf: Vec<Vec<T>>,
    /// Frequency-domain delay line per channel (one slot per partition).
    fdl: Vec<Vec<Vec<Complex<T>>>>,
    fdl_pos: usize,
    /// Overlap buffer per channel (tail from previous block).
    overlap: Vec<Vec<T>>,
    /// File de sortie par canal, amorcée de `block_size` zéros.
    ///
    /// Une convolution par blocs ne peut pas rendre un échantillon avant
    /// d'avoir vu le bloc qui le contient : elle a une latence, et c'est
    /// `block_size`. L'ancien code la niait — il réécrivait le bloc traité
    /// À REBOURS dans le tampon d'entrée, ce qui suppose que le bloc s'y
    /// trouve en entier. Dès qu'un bloc partiel était reporté d'un appel au
    /// suivant, `frame + 1 - block_size` sous-débordait (#2209).
    ///
    /// Avec une file amorcée, chaque trame entrée rend une trame sortie, et le
    /// résultat ne dépend plus du découpage des appels.
    output_buf: Vec<VecDeque<T>>,
    /// Tampon de travail d'un bloc, réutilisé — il était alloué à chaque bloc.
    scratch: Vec<Vec<T>>,
    /// T09b — tampons de travail de la FFT, réservés une fois : l'ancien
    /// moteur allouait quatre vecteurs par bloc et par canal.
    temps: Vec<T>,
    acc: Vec<Complex<T>>,
    sortie_fft: Vec<T>,
    scratch_fwd: Vec<Complex<T>>,
    scratch_inv: Vec<Complex<T>>,
    fwd: Arc<dyn RealToComplex<T>>,
    inv: Arc<dyn ComplexToReal<T>>,
    /// #7632 — l'entrée SÈCHE, retardée de `block_size` trames : exactement
    /// alignée sur la sortie convoluée. Elle sert au fondu d'extinction
    /// ([`Self::eteindre_en_fondu`]) : passer du convolué au direct sans
    /// sauter de `latency_frames()` trames, donc sans clic.
    direct_retarde: Vec<VecDeque<T>>,
    /// Fondu d'extinction en cours : (trames restantes, trames totales).
    fondu: Option<(usize, usize)>,
    /// Fondu terminé : le moteur ne convolue plus, il ne rend que l'entrée
    /// retardée de sa latence, jusqu'à ce qu'on le retire à la frontière de
    /// piste suivante.
    eteint: bool,
    /// T09b — bascule vers un autre filtre, flux en cours
    /// ([`Self::remplacer_en_fondu`]) : le successeur reçoit la même entrée,
    /// se réchauffe sans être entendu, puis prend la main en fondu.
    successeur: Option<Box<Convolver<T>>>,
    /// Trames de réchauffe du successeur qui restent avant le fondu.
    rechauffe: usize,
    /// Fondu de bascule : (trames restantes, trames totales).
    bascule: Option<(usize, usize)>,
    /// Le moteur remplacé, gardé pour être libéré HORS du fil audio (à la
    /// bascule suivante ou à la destruction) : la bascule n'alloue ni ne
    /// libère rien dans `traiter`.
    retraite: Option<Box<Convolver<T>>>,
    /// Copie de l'entrée pour le successeur, un bloc au plus.
    tampon_bascule: Vec<T>,
    /// Conversion des entrées `f32` et PCM, un bloc au plus.
    tampon_es: Vec<f64>,
}

impl<T: Flottant> Convolver<T> {
    /// Construire le moteur depuis des prises `f64` (une réponse par canal).
    pub fn depuis_f64(impulse_response: &[Vec<f64>], block_size: usize) -> Self {
        let channels = impulse_response.len();
        assert!(channels > 0, "IR must have at least one channel");
        let block_size = block_size.max(1);
        let fft_size = (block_size * 2).next_power_of_two();
        let spectrum_len = fft_size / 2 + 1;
        let zero = T::default();
        let czero = Complex::new(zero, zero);

        let mut planner = RealFftPlanner::<T>::new();
        let fwd = planner.plan_fft_forward(fft_size);
        let inv = planner.plan_fft_inverse(fft_size);
        let mut scratch_fwd = fwd.make_scratch_vec();

        let mut ir_partitions = Vec::with_capacity(channels);
        let mut num_partitions = 1;
        let mut padded = vec![zero; fft_size];
        for ch_ir in impulse_response {
            let n_parts = ch_ir.len().div_ceil(block_size);
            num_partitions = num_partitions.max(n_parts);
            let mut ch_parts = Vec::with_capacity(n_parts);
            for p in 0..n_parts {
                let start = p * block_size;
                let end = (start + block_size).min(ch_ir.len());
                padded.fill(zero);
                for (d, s) in padded.iter_mut().zip(&ch_ir[start..end]) {
                    *d = T::de_f64(*s);
                }
                let mut spectrum = fwd.make_output_vec();
                fwd.process_with_scratch(&mut padded, &mut spectrum, &mut scratch_fwd)
                    .expect("FFT de partition aux tailles préparées");
                ch_parts.push(spectrum);
            }
            ir_partitions.push(ch_parts);
        }

        let fdl: Vec<Vec<Vec<_>>> = (0..channels)
            .map(|_| vec![vec![czero; spectrum_len]; num_partitions])
            .collect();
        let amorce = || VecDeque::from(vec![zero; block_size]);
        let mut output_buf = Vec::with_capacity(channels);
        let mut direct_retarde = Vec::with_capacity(channels);
        for _ in 0..channels {
            // Amorçage : la latence de la convolution, rendue explicite. La
            // capacité couvre amorce + un bloc : jamais de croissance ensuite.
            let mut o = amorce();
            o.reserve(block_size + 1);
            output_buf.push(o);
            let mut d = amorce();
            d.reserve(block_size + 1);
            direct_retarde.push(d);
        }

        Self {
            block_size,
            fft_size,
            channels,
            prises: impulse_response.iter().map(Vec::len).max().unwrap_or(0),
            ir_partitions,
            // Une capacité PAR canal : `vec![v; n]` clone `v`, et le clone
            // d'un vecteur vide n'a aucune capacité (le premier bloc aurait
            // alloué dans le fil audio).
            input_buf: (0..channels)
                .map(|_| Vec::with_capacity(block_size))
                .collect(),
            fdl,
            fdl_pos: 0,
            overlap: vec![vec![zero; fft_size - block_size]; channels],
            output_buf,
            scratch: vec![vec![zero; block_size]; channels],
            temps: vec![zero; fft_size],
            acc: vec![czero; spectrum_len],
            sortie_fft: inv.make_output_vec(),
            scratch_inv: inv.make_scratch_vec(),
            scratch_fwd,
            fwd,
            inv,
            direct_retarde,
            fondu: None,
            eteint: false,
            successeur: None,
            rechauffe: 0,
            bascule: None,
            retraite: None,
            tampon_bascule: vec![zero; block_size * channels],
            tampon_es: vec![0.0; block_size * channels],
        }
    }

    /// Construire le moteur depuis des prises `f32` (lecture d'un WAV, jeu
    /// par cadence) : la conversion en `f64` est exacte.
    pub fn depuis_f32(impulse_response: &[Vec<f32>], block_size: usize) -> Self {
        let ir: Vec<Vec<f64>> = impulse_response
            .iter()
            .map(|c| c.iter().map(|&x| f64::from(x)).collect())
            .collect();
        Self::depuis_f64(&ir, block_size)
    }

    /// #7632 — éteindre la convolution EN FONDU, flux en cours : sur
    /// `trames` trames, la sortie passe du convolué à l'entrée directe
    /// retardée de `latency_frames()` (alignée sur le convolué), puis ne rend
    /// plus que celle-ci. Ni saut d'échantillon ni saut de temps : pas de
    /// clic. Le moteur reste un simple retard jusqu'à la piste suivante, où
    /// la sortie ne le reconstruit pas. Sans effet s'il est déjà éteint.
    ///
    /// T09b — une bascule en cours est abandonnée : c'est le moteur EN
    /// SERVICE qui s'éteint, et le successeur n'aura jamais été entendu.
    pub fn eteindre_en_fondu(&mut self, trames: usize) {
        if self.eteint || self.fondu.is_some() {
            return;
        }
        if let Some(s) = self.successeur.take() {
            self.retraite = Some(s);
            self.bascule = None;
            self.rechauffe = 0;
        }
        if trames == 0 {
            self.eteint = true;
        } else {
            self.fondu = Some((trames, trames));
        }
    }

    /// La convolution est-elle éteinte (fondu terminé) ?
    pub fn est_eteint(&self) -> bool {
        self.eteint
    }

    /// T09b — changer de filtre FLUX EN COURS, sans discontinuité.
    ///
    /// `nouveau` reçoit dès maintenant la même entrée que le moteur en
    /// service, sans être entendu, pendant `prises + latence` trames
    /// (plafonné à [`RECHAUFFE_MAX`]) : sa sortie est alors EXACTEMENT celle
    /// qu'il aurait eue s'il avait tourné depuis le début du flux (une
    /// convolution ne voit que les `prises` dernières trames). Puis la sortie
    /// passe de l'un à l'autre en `trames_fondu` trames, en cosinus surélevé
    /// (gains de somme 1 : les deux sorties sont corrélées). Les deux moteurs
    /// ont la même latence, donc aucun saut de temps.
    ///
    /// Hors temps réel (sous le verrou de l'hôte). Refuse — et rend
    /// `nouveau` — si les canaux ou le bloc diffèrent, ou si ce moteur est
    /// éteint : l'appelant décide alors (remplacement à la frontière de
    /// piste). Une bascule déjà en cours est remplacée par la nouvelle.
    pub fn remplacer_en_fondu(
        &mut self,
        nouveau: Convolver<T>,
        trames_fondu: usize,
    ) -> Result<(), Convolver<T>> {
        if nouveau.channels != self.channels
            || nouveau.block_size != self.block_size
            || self.eteint
            || self.fondu.is_some()
        {
            return Err(nouveau);
        }
        // Libérer ici, hors du fil audio, ce que la bascule précédente a
        // laissé.
        self.retraite = None;
        self.rechauffe = (nouveau.prises + nouveau.block_size).min(RECHAUFFE_MAX);
        self.bascule = Some((trames_fondu.max(1), trames_fondu.max(1)));
        self.successeur = Some(Box::new(nouveau));
        Ok(())
    }

    /// Une bascule de filtre est-elle en cours (réchauffe ou fondu) ?
    pub fn bascule_en_cours(&self) -> bool {
        self.successeur.is_some()
    }

    /// Remettre le moteur à zéro : plus rien de la piste précédente.
    ///
    /// Le convolveur est installé une fois (`set_convolver_ir`) et vit aussi
    /// longtemps que la sortie. Sans cette remise à zéro, la file de sortie, la
    /// ligne à retard et l'overlap portent la queue de la piste d'avant : elle
    /// repart dans la suivante, et un seek ou un arrêt n'établit aucune
    /// frontière (JP Robbe, revue de #2268).
    ///
    /// T09b — une bascule en cours se termine d'un coup : à une frontière, il
    /// n'y a plus de flux à raccorder.
    pub fn reset(&mut self) {
        if let Some(mut s) = self.successeur.take() {
            std::mem::swap(self, &mut *s);
            self.retraite = Some(s);
        }
        self.bascule = None;
        self.rechauffe = 0;
        let zero = T::default();
        for c in 0..self.channels {
            self.input_buf[c].clear();
            self.output_buf[c].clear();
            // Ré-amorcer la latence : sans ça, la première trame de la piste
            // suivante sortirait du néant.
            self.output_buf[c].extend(std::iter::repeat_n(zero, self.block_size));
            self.overlap[c].iter_mut().for_each(|v| *v = zero);
            self.direct_retarde[c].clear();
            self.direct_retarde[c].extend(std::iter::repeat_n(zero, self.block_size));
            for slot in self.fdl[c].iter_mut() {
                slot.iter_mut().for_each(|v| *v = Complex::new(zero, zero));
            }
        }
        self.fdl_pos = 0;
    }

    /// Latence introduite par la convolution, en trames.
    ///
    /// Une convolution par blocs ne peut rien rendre avant d'avoir vu un bloc
    /// entier. La déclarer permet aux appelants qui ont besoin d'un alignement
    /// exact — `process_offline` — de la compenser. T09b : c'est aussi la
    /// latence que l'étage annonce à l'exécuteur TESS ; elle est EXACTE (une
    /// impulsion à l'entrée sort `latency_frames()` trames plus tard, à
    /// l'échantillon près, pour une réponse dont le pic est en tête).
    pub fn latency_frames(&self) -> usize {
        self.block_size
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Traiter des échantillons entrelacés du type du moteur, sur place.
    ///
    /// Le résultat ne dépend PAS du découpage des appels : nourrir 1024 trames
    /// d'un coup, ou 100 puis 924, ou 1 par 1, rend exactement la même suite,
    /// décalée de `latency_frames()`. N'alloue jamais.
    pub fn traiter(&mut self, samples: &mut [T]) {
        let ch = self.channels;
        if ch == 0 {
            return;
        }
        let pas = self.block_size * ch;
        let mut debut = 0;
        while debut < samples.len() {
            let fin = (debut + pas).min(samples.len());
            let morceau = &mut samples[debut..fin];
            debut = fin;
            if self.successeur.is_none() {
                self.coeur(morceau);
                continue;
            }
            let n = morceau.len();
            self.tampon_bascule[..n].copy_from_slice(morceau);
            self.coeur(morceau);
            let Some(succ) = self.successeur.as_mut() else {
                continue;
            };
            succ.traiter(&mut self.tampon_bascule[..n]);
            let mut fini = false;
            for (trame, nouvelle) in morceau
                .chunks_exact_mut(ch)
                .zip(self.tampon_bascule[..n].chunks_exact(ch))
            {
                if fini {
                    trame.copy_from_slice(nouvelle);
                    continue;
                }
                if self.rechauffe > 0 {
                    self.rechauffe -= 1;
                    continue;
                }
                let Some((restant, total)) = self.bascule else {
                    fini = true;
                    trame.copy_from_slice(nouvelle);
                    continue;
                };
                // Cosinus surélevé : gain du nouveau de 0 à 1, sans pente
                // aux deux bouts.
                let t = (total - restant + 1) as f64 / total as f64;
                let g = 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
                let (gn, ga) = (T::de_f64(g), T::de_f64(1.0 - g));
                for (a, b) in trame.iter_mut().zip(nouvelle) {
                    *a = *a * ga + *b * gn;
                }
                if restant <= 1 {
                    self.bascule = None;
                    fini = true;
                } else {
                    self.bascule = Some((restant - 1, total));
                }
            }
            if fini {
                // Le successeur prend la place, sans allocation : l'ancien
                // moteur part dans la boîte du successeur, gardée pour être
                // libérée hors du fil audio.
                if let Some(mut s) = self.successeur.take() {
                    std::mem::swap(self, &mut *s);
                    self.retraite = Some(s);
                }
            }
        }
    }

    /// Le cœur trame par trame (sans bascule).
    fn coeur(&mut self, samples: &mut [T]) {
        let ch = self.channels;
        let frame_count = samples.len() / ch;

        for frame in 0..frame_count {
            // #7632 — l'entrée sèche, retardée de la latence : la sortie d'un
            // moteur éteint, et l'autre moitié du fondu d'extinction.
            for c in 0..ch {
                let x = samples[frame * ch + c];
                self.direct_retarde[c].push_back(x);
            }
            if self.eteint {
                for c in 0..ch {
                    samples[frame * ch + c] =
                        self.direct_retarde[c].pop_front().unwrap_or_default();
                }
                continue;
            }
            for c in 0..ch {
                self.input_buf[c].push(samples[frame * ch + c]);
            }

            if self.input_buf[0].len() >= self.block_size {
                self.process_block();
                for c in 0..ch {
                    self.output_buf[c].extend(self.scratch[c].iter().copied());
                    self.input_buf[c].clear();
                }
            }

            // Une trame entrée, une trame sortie. La file est amorcée, donc
            // elle n'est jamais vide.
            let gain_convolue = self
                .fondu
                .map(|(restant, total)| restant as f64 / total as f64);
            for c in 0..ch {
                let convolue = self.output_buf[c].pop_front().unwrap_or_default();
                let direct = self.direct_retarde[c].pop_front().unwrap_or_default();
                samples[frame * ch + c] = match gain_convolue {
                    None => convolue,
                    Some(g) => convolue * T::de_f64(g) + direct * T::de_f64(1.0 - g),
                };
            }
            if let Some((restant, total)) = self.fondu {
                if restant <= 1 {
                    self.fondu = None;
                    self.eteint = true;
                } else {
                    self.fondu = Some((restant - 1, total));
                }
            }
        }
    }

    fn process_block(&mut self) {
        let Self {
            block_size,
            fft_size,
            channels,
            ir_partitions,
            input_buf,
            fdl,
            fdl_pos,
            overlap,
            scratch,
            temps,
            acc,
            sortie_fft,
            scratch_fwd,
            scratch_inv,
            fwd,
            inv,
            ..
        } = self;
        let (block_size, fft_size) = (*block_size, *fft_size);
        let zero = T::default();
        // La ligne à retard est dimensionnée sur le MAXIMUM de partitions parmi
        // les canaux. Prendre `ir_partitions[0].len()` ignorait la fin d'une IR
        // plus longue sur un autre canal — une IR stéréo aux deux canaux de
        // longueurs différentes perdait sa queue (#2209).
        let num_partitions = fdl[0].len();
        let scale = T::de_f64(1.0 / fft_size as f64);

        for ch in 0..*channels {
            temps.fill(zero);
            let n = input_buf[ch].len().min(block_size);
            temps[..n].copy_from_slice(&input_buf[ch][..n]);
            fwd.process_with_scratch(temps, &mut fdl[ch][*fdl_pos], scratch_fwd)
                .expect("FFT d'entrée aux tailles préparées");

            acc.fill(Complex::new(zero, zero));
            for (p, partition) in ir_partitions[ch].iter().enumerate() {
                let fdl_idx = (*fdl_pos + num_partitions - p) % num_partitions;
                for ((a, x), h) in acc.iter_mut().zip(&fdl[ch][fdl_idx]).zip(partition) {
                    *a = *a + *x * *h;
                }
            }

            // Les parties imaginaires du continu et de Nyquist sont nulles
            // (produits de spectres de signaux réels) ; realfft les remet à
            // zéro de toute façon.
            let _ = inv.process_with_scratch(acc, sortie_fft, scratch_inv);

            let overlap_len = fft_size - block_size;
            for i in 0..block_size {
                scratch[ch][i] = sortie_fft[i] * scale + overlap[ch][i];
            }
            for i in 0..overlap_len {
                overlap[ch][i] = sortie_fft[block_size + i] * scale;
            }
        }

        *fdl_pos = (*fdl_pos + 1) % num_partitions.max(1);
    }
}

impl Convolver<f64> {
    /// Moteur `f64` depuis des prises `f32` (la forme des réponses lues).
    pub fn new(impulse_response: &[Vec<f32>], block_size: usize) -> Self {
        Self::depuis_f32(impulse_response, block_size)
    }

    /// Parse a WAV impulse response into per-channel f32 taps + its sample
    /// rate. Walks the RIFF chunks so extra chunks before `data` don't break it.
    fn read_wav_ir(path: &str) -> Result<(Vec<Vec<f32>>, u32), String> {
        let data = std::fs::read(path).map_err(|e| format!("read IR: {e}"))?;
        if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
            return Err("not a RIFF/WAVE file".into());
        }

        // Walk the RIFF chunks to locate `fmt ` and `data` at their REAL
        // offsets. Impulse WAVs exported by REW / rePhase / Dirac often carry
        // extra chunks (fact, PEAK, LIST/INFO, cue…) before `data`, so a fixed
        // 44-byte header offset misreads them into noise.
        let mut format_tag: u16 = 1; // 1 = PCM int, 3 = IEEE float
        let mut channels: usize = 0;
        let mut sample_rate: u32 = 0;
        let mut bits: usize = 0;
        let mut data_range: Option<(usize, usize)> = None;
        let mut pos = 12usize;
        while pos + 8 <= data.len() {
            let id = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
            let declared =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            let body = pos + 8;
            // Clamp to what's actually present so a truncated final chunk can't
            // index out of bounds.
            let size = declared.min(data.len() - body);
            match &id {
                b"fmt " if size >= 16 => {
                    format_tag = u16::from_le_bytes([data[body], data[body + 1]]);
                    channels = u16::from_le_bytes([data[body + 2], data[body + 3]]) as usize;
                    sample_rate = u32::from_le_bytes([
                        data[body + 4],
                        data[body + 5],
                        data[body + 6],
                        data[body + 7],
                    ]);
                    bits = u16::from_le_bytes([data[body + 14], data[body + 15]]) as usize;
                    // WAVE_FORMAT_EXTENSIBLE: the effective format tag lives in
                    // the first 2 bytes of the sub-format GUID.
                    if format_tag == 0xFFFE && size >= 40 {
                        format_tag = u16::from_le_bytes([data[body + 24], data[body + 25]]);
                    }
                }
                b"data" => data_range = Some((body, body + size)),
                _ => {}
            }
            // Chunks are word-aligned: an odd size carries a trailing pad byte.
            pos = body + size + (size & 1);
        }

        let (dstart, dend) = data_range.ok_or("missing data chunk")?;
        if channels == 0 {
            return Err("missing or invalid fmt chunk".into());
        }
        let bytes_per_sample = bits / 8;
        if bytes_per_sample == 0 {
            return Err(format!("unsupported bit depth: {bits}"));
        }
        let is_float = format_tag == 3;

        let total_samples = (dend - dstart) / bytes_per_sample;
        let samples_per_channel = total_samples / channels;
        if samples_per_channel == 0 {
            return Err("IR has no samples".into());
        }

        tracing::info!(
            path,
            channels,
            sample_rate,
            bits,
            is_float,
            samples_per_channel,
            "convolver_ir_loaded"
        );

        let mut ir = vec![Vec::with_capacity(samples_per_channel); channels];
        for i in 0..samples_per_channel {
            for ch in 0..channels {
                let o = dstart + (i * channels + ch) * bytes_per_sample;
                let sample = match (bits, is_float) {
                    (16, false) => i16::from_le_bytes([data[o], data[o + 1]]) as f32 / 32768.0,
                    (24, false) => {
                        // 24-bit LE placed in the top 3 bytes of an i32 for
                        // correct sign extension, then scaled by 2^31.
                        let v = i32::from_le_bytes([0, data[o], data[o + 1], data[o + 2]]);
                        v as f32 / 2147483648.0
                    }
                    (32, false) => {
                        let v =
                            i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
                        v as f32 / 2147483648.0
                    }
                    (32, true) => {
                        f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
                    }
                    (64, true) => f64::from_le_bytes([
                        data[o],
                        data[o + 1],
                        data[o + 2],
                        data[o + 3],
                        data[o + 4],
                        data[o + 5],
                        data[o + 6],
                        data[o + 7],
                    ]) as f32,
                    _ => {
                        return Err(format!(
                            "unsupported WAV sample format: {bits}-bit {}",
                            if is_float { "float" } else { "int" }
                        ));
                    }
                };
                ir[ch].push(sample);
            }
        }

        Ok((ir, sample_rate))
    }

    /// Load an impulse response from a WAV file (any sample rate / channels).
    pub fn from_wav(path: &str, block_size: usize) -> Result<Self, String> {
        let config = ConvolverConfig::from_wav(path)?;
        Ok(Self::new(&config.impulse_response, block_size))
    }

    /// Read the raw taps of a WAV impulse response (per channel) plus its
    /// sample rate, WITHOUT building a convolver. The running [`Convolver`]
    /// only keeps the IR in FFT-partitioned form, so anything that needs the
    /// time-domain coefficients again (e.g. the `/convolver/response`
    /// visualisation endpoint) re-reads them from the persisted file.
    pub fn read_ir_taps(path: &str) -> Result<(Vec<Vec<f32>>, u32), String> {
        Self::read_wav_ir(path)
    }

    /// Combiner deux reponses impulsionnelles MONO — gauche et droite — en un
    /// seul WAV stereo, et rendre sa cadence.
    ///
    /// Le moteur sait deja convoluer un canal par reponse : `Convolver::new`
    /// prend un `&[Vec<f32>]`, et un WAV stereo donne bien deux corrections
    /// differentes. Ce qui manquait, c'est le CHEMIN D'ENTREE : les outils de
    /// correction de piece — REW, Acourate, Audiolense — exportent DEUX
    /// fichiers mono, `filter_L.wav` et `filter_R.wav`, jamais un stereo.
    /// L'utilisateur devait donc les fusionner lui-meme dans un editeur audio
    /// (Daniel, 24/08/2026).
    ///
    /// On ecrit le resultat au chemin que les consommateurs lisent DEJA, plutot
    /// que d'ajouter un second reglage : la sortie locale, le chemin de
    /// transcodage vers les renderers reseau et la visualisation
    /// `/convolver/response` continuent de ne connaitre qu'un fichier.
    ///
    /// Les deux reponses doivent partager leur cadence — convoluer a des
    /// cadences differentes decalerait un canal par rapport a l'autre. La plus
    /// courte est completee de zeros : c'est neutre pour une convolution.
    pub fn combiner_en_stereo(
        chemin_gauche: &str,
        chemin_droite: &str,
        chemin_sortie: &str,
    ) -> Result<u32, String> {
        let (ir_g, sr_g) = Self::read_wav_ir(chemin_gauche)?;
        let (ir_d, sr_d) = Self::read_wav_ir(chemin_droite)?;
        if sr_g != sr_d {
            return Err(format!(
                "les deux filtres doivent partager leur cadence : {sr_g} Hz a gauche, {sr_d} Hz a droite"
            ));
        }
        // Un fichier stereo passe aussi : on prend son canal correspondant, ce
        // qui evite un refus incomprehensible si l'outil a exporte deux stereo.
        let gauche = ir_g.first().ok_or("le filtre gauche est vide")?;
        let droite = ir_d
            .get(1)
            .or_else(|| ir_d.first())
            .ok_or("le filtre droit est vide")?;
        if gauche.is_empty() || droite.is_empty() {
            return Err("un des deux filtres ne contient aucun echantillon".into());
        }

        let n = gauche.len().max(droite.len());
        let mut entrelace = Vec::with_capacity(n * 2);
        for i in 0..n {
            entrelace.push(gauche.get(i).copied().unwrap_or(0.0));
            entrelace.push(droite.get(i).copied().unwrap_or(0.0));
        }
        ecrire_wav_float32(chemin_sortie, &entrelace, 2, sr_g)?;
        tracing::info!(
            gauche = chemin_gauche,
            droite = chemin_droite,
            sortie = chemin_sortie,
            sample_rate = sr_g,
            taps_gauche = gauche.len(),
            taps_droite = droite.len(),
            "convolver_ir_stereo_combinee"
        );
        Ok(sr_g)
    }

    /// Load an IR for a specific stream rate + channel count. #8761 : an IR at
    /// another rate is resampled (cached); a mono IR is duplicated
    /// to the stream's channel count. Used by the transcode path so the FIR can
    /// apply to network renderers, not just the local output.
    pub fn from_wav_for(
        path: &str,
        block_size: usize,
        target_sr: u32,
        target_channels: usize,
    ) -> Result<Self, String> {
        ConvolverConfig::from_wav(path)?.build_for(block_size, target_sr, target_channels)
    }

    /// TESS T09b — l'appel de la chaîne : des `f64` entrelacés, sur place,
    /// sans aucune conversion. Voir [`Self::traiter`].
    pub fn process_interleaved_f64(&mut self, samples: &mut [f64]) {
        self.traiter(samples);
    }

    /// Process interleaved f32 samples in-place (chemins hérités : le moteur
    /// calcule en `f64`, la conversion se fait bloc par bloc dans un tampon
    /// réservé, sans allocation).
    ///
    /// Le résultat ne dépend PAS du découpage des appels : nourrir 1024 trames
    /// d'un coup, ou 100 puis 924, ou 1 par 1, rend exactement la même suite,
    /// décalée de `latency_frames()`.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        if self.channels == 0 {
            return;
        }
        let pas = self.tampon_es.len().max(self.channels);
        let mut tampon = std::mem::take(&mut self.tampon_es);
        for morceau in samples.chunks_mut(pas) {
            let large = &mut tampon[..morceau.len()];
            for (d, s) in large.iter_mut().zip(morceau.iter()) {
                *d = f64::from(*s);
            }
            self.traiter(large);
            for (d, s) in morceau.iter_mut().zip(large.iter()) {
                *d = *s as f32;
            }
        }
        // La bascule a pu échanger le moteur : le tampon rendu est celui du
        // moteur en service (même taille, mêmes canaux et bloc).
        self.tampon_es = tampon;
    }

    /// Rendre ce que le moteur retient encore, en fin de piste.
    ///
    /// Une convolution par blocs garde `latency_frames()` trames en réserve —
    /// c'est le prix de sa latence. Sans ce drainage, ces trames ne partent
    /// jamais au périphérique : la fin de chaque piste était tronquée
    /// (JP Robbe, revue de #2268).
    ///
    /// Rend des échantillons ENTRELACÉS, prêts à suivre le même chemin que le
    /// reste. Le moteur est remis à zéro après : la piste est finie.
    pub fn flush(&mut self) -> Vec<f32> {
        self.flush_f64().into_iter().map(|x| x as f32).collect()
    }

    /// [`Self::flush`] en `f64` (chaîne TESS : la queue ne repasse pas par le
    /// `f32`).
    pub fn flush_f64(&mut self) -> Vec<f64> {
        let ch = self.channels;
        if ch == 0 {
            return Vec::new();
        }
        // Nourrir du silence pour pousser la queue hors du moteur, puis
        // recueillir exactement ce qui restait.
        let latence = self.latency_frames();
        let mut queue = vec![0.0f64; latence * ch];
        self.traiter(&mut queue);
        self.reset();
        queue
    }

    /// Convolve a whole interleaved buffer offline (one-shot), in place and at
    /// the same length as the input. `process_interleaved` leaves the final
    /// partial (< block_size) frames unprocessed (they'd stay dry → an audible
    /// click at the buffer end); pad up to the next block boundary so that last
    /// block is flushed too, then truncate back. The IR decay *past* the buffer
    /// end is dropped (a few ms of tail) — acceptable for room correction.
    ///
    /// Use a FRESH Convolver per buffer (no carried-over state across tracks).
    pub fn process_offline(&mut self, samples: &mut Vec<f32>) {
        let mut large: Vec<f64> = samples.iter().map(|&x| f64::from(x)).collect();
        self.process_offline_f64(&mut large);
        for (d, s) in samples.iter_mut().zip(large) {
            *d = s as f32;
        }
    }

    /// [`Self::process_offline`] en `f64`.
    pub fn process_offline_f64(&mut self, samples: &mut Vec<f64>) {
        let ch = self.channels;
        if ch == 0 || samples.is_empty() {
            return;
        }
        let orig_len = samples.len();
        let frames = orig_len / ch;
        let rem = frames % self.block_size;
        if rem != 0 {
            let pad = (self.block_size - rem) * ch;
            samples.extend(std::iter::repeat_n(0.0, pad));
        }
        // Compenser la latence : `traiter` rend la trame `i` à la position
        // `i + latency`. On nourrit donc `latency` trames de silence en plus,
        // et on jette autant de trames en tête.
        let latence = self.latency_frames();
        samples.extend(std::iter::repeat_n(0.0, latence * ch));
        self.traiter(samples);
        samples.drain(..latence * ch);
        samples.truncate(orig_len);
    }

    /// Apply the convolver to interleaved integer PCM bytes in place, matching
    /// the layout the transcode pipeline hands to `EqProcessor::process_pcm`
    /// (little-endian, `bit_depth` of 16/24/32). Decodes to f64, convolves
    /// offline, and writes the samples back with soft clamping.
    ///
    /// # #4076 (constat voisin) — une réponse impulsionnelle UNITÉ est
    /// maintenant l'identité
    ///
    /// Le décodage divisait par 2^(n−1) — 32 768 à 16 bits — et le réencodage
    /// multipliait par 2^(n−1) **− 1** — 32 767. Cette asymétrie est un gain de
    /// −0,00027 dB appliqué à chaque passage : mesuré par T9, elle faisait
    /// perdre 1 LSB à **29 214 échantillons sur 44 100 (66 %)** d'un sinus à
    /// −0,1 dBFS, si bien qu'un convolveur chargé d'une impulsion unité — donc
    /// censé ne rien faire — abîmait le signal.
    ///
    /// Les deux sens utilisent désormais la **même** échelle, 2^(n−1), et la
    /// saturation au rail (`2^(n−1) − 1` en haut) vient après l'arrondi. Une IR
    /// unité rend donc exactement ses octets d'entrée, à 16 et 24 bits.
    ///
    /// T09b — le moteur et ce chemin sont en `f64` : l'échantillon 32 bits
    /// (31 bits de mantisse requis) y est exact aussi, et une IR unité rend
    /// ses octets à 32 bits également.
    ///
    /// Pas de dither ici, et c'est délibéré : le convolveur arrondit déjà au
    /// plus proche — son erreur ne dépasse pas un demi-LSB et ne porte pas le
    /// signe du signal, contrairement aux troncatures de #4075 et #4076. Lui
    /// ajouter du bruit détruirait précisément l'identité qu'on vient de lui
    /// rendre. Voir « Où ce dither ne doit PAS aller » dans [`crate::audio::dither`].
    pub fn process_pcm(&mut self, pcm: &mut [u8], bit_depth: u16) {
        let Some(mut buf) = self.pcm_vers_f64(pcm, bit_depth) else {
            return;
        };
        self.process_offline_f64(&mut buf);
        f64_vers_pcm(&buf, pcm, bit_depth);
    }

    /// Convolution d'un FLUX découpé en tampons successifs (#7642).
    ///
    /// [`Self::process_pcm`] traite chaque appel comme une piste entière : il
    /// complète le tampon de zéros jusqu'au bloc, pousse `latency_frames()`
    /// trames de silence pour compenser la latence, puis les jette. Appelé
    /// tampon après tampon sur la MÊME instance, ces zéros entrent dans la
    /// ligne à retard comme s'ils étaient du signal : la queue de l'IR qui
    /// déborde d'un tampon ne rejoint jamais le suivant, et le suivant voit un
    /// passé décalé. Une IR de correction de pièce (des milliers de taps)
    /// était ainsi coupée à chaque frontière de tampon du bras progressif.
    ///
    /// Ici, l'état est conservé d'un appel à l'autre ([`Self::traiter`],
    /// indépendant du découpage), et la latence est compensée sur le flux
    /// entier : les `latency_frames()` premières trames de sortie — l'amorce
    /// de silence — sont retirées du début (`reste_a_jeter`, tenu par
    /// l'appelant et initialisé à `latency_frames()`), et
    /// [`Self::terminer_pcm_en_flux`] rend en fin de flux ce que le moteur
    /// retient encore. Somme des longueurs rendues = somme des longueurs
    /// reçues, et le résultat est celui de [`Self::process_pcm`] sur la piste
    /// entière. Le tampon peut donc RACCOURCIR (jamais en dessous de zéro) au
    /// début du flux ; il reste aligné sur la trame.
    pub fn process_pcm_en_flux(
        &mut self,
        pcm: &mut Vec<u8>,
        bit_depth: u16,
        reste_a_jeter: &mut usize,
    ) {
        let Some(mut buf) = self.pcm_vers_f64(pcm, bit_depth) else {
            return;
        };
        self.traiter(&mut buf);
        let ch = self.channels;
        let jetees = (*reste_a_jeter).min(buf.len() / ch);
        *reste_a_jeter -= jetees;
        let garde = &buf[jetees * ch..];
        let bps = (bit_depth / 8) as usize;
        pcm.truncate(garde.len() * bps);
        f64_vers_pcm(garde, pcm, bit_depth);
    }

    /// Fin du flux de [`Self::process_pcm_en_flux`] : rend, en octets PCM, les
    /// trames que la latence retenait encore, puis remet le moteur à zéro
    /// (`reste_a_jeter` est réarmé pour un flux suivant).
    pub fn terminer_pcm_en_flux(&mut self, bit_depth: u16, reste_a_jeter: &mut usize) -> Vec<u8> {
        let ch = self.channels;
        let bps = (bit_depth / 8) as usize;
        if ch == 0 || bps == 0 || !matches!(bit_depth, 16 | 24 | 32) {
            return Vec::new();
        }
        let latence = self.latency_frames();
        let mut queue = vec![0.0f64; latence * ch];
        self.traiter(&mut queue);
        let jetees = (*reste_a_jeter).min(latence);
        let garde = &queue[jetees * ch..];
        let mut pcm = vec![0u8; garde.len() * bps];
        f64_vers_pcm(garde, &mut pcm, bit_depth);
        self.reset();
        *reste_a_jeter = self.latency_frames();
        pcm
    }

    /// Décodage du PCM entier entrelacé en `f64`, à l'échelle 2^(n−1) (voir
    /// [`Self::process_pcm`]). `None` : rien à faire (aucun canal, tampon vide,
    /// profondeur non gérée).
    fn pcm_vers_f64(&self, pcm: &[u8], bit_depth: u16) -> Option<Vec<f64>> {
        let ch = self.channels;
        if ch == 0 || pcm.is_empty() {
            return None;
        }
        let bps = (bit_depth / 8) as usize;
        if bps == 0 {
            return None;
        }
        let total = pcm.len() / bps;
        let mut buf: Vec<f64> = Vec::with_capacity(total);
        for i in 0..total {
            let o = i * bps;
            let s = match bit_depth {
                16 => f64::from(i16::from_le_bytes([pcm[o], pcm[o + 1]])) / 32_768.0,
                24 => {
                    // Le décalage arithmétique redonne l'échantillon 24 bits
                    // SIGNÉ, dans sa plage naturelle ±2^23.
                    let v = i32::from_le_bytes([0, pcm[o], pcm[o + 1], pcm[o + 2]]) >> 8;
                    f64::from(v) / 8_388_608.0
                }
                32 => {
                    let v = i32::from_le_bytes([pcm[o], pcm[o + 1], pcm[o + 2], pcm[o + 3]]);
                    f64::from(v) / 2_147_483_648.0
                }
                _ => return None,
            };
            buf.push(s);
        }
        Some(buf)
    }
}

/// Réencodage des échantillons `f64` dans `pcm` (autant d'échantillons que
/// `pcm` en contient), à la même échelle qu'au décodage.
fn f64_vers_pcm(buf: &[f64], pcm: &mut [u8], bit_depth: u16) {
    let bps = (bit_depth / 8) as usize;
    if bps == 0 {
        return;
    }
    let total = pcm.len() / bps;
    for i in 0..total {
        let o = i * bps;
        let s = buf.get(i).copied().unwrap_or(0.0).clamp(-1.0, 1.0);
        match bit_depth {
            // Même échelle qu'au décodage (2^(n−1)), arrondi au plus
            // proche, saturation au rail EN DERNIER — par la porte
            // partagée, avec un bruit nul : le convolveur ne dithère pas.
            16 => {
                let v = quantifier_avec(s * 32_768.0, 0.0, -32_768.0, 32_767.0);
                pcm[o..o + 2].copy_from_slice(&(v as i16).to_le_bytes());
            }
            24 => {
                let v = quantifier_avec(s * 8_388_608.0, 0.0, -8_388_608.0, 8_388_607.0);
                let b = (v as i32).to_le_bytes();
                pcm[o] = b[0];
                pcm[o + 1] = b[1];
                pcm[o + 2] = b[2];
            }
            32 => {
                let v = quantifier_avec(
                    s * 2_147_483_648.0,
                    0.0,
                    f64::from(i32::MIN),
                    f64::from(i32::MAX),
                );
                pcm[o..o + 4].copy_from_slice(&(v as i32).to_le_bytes());
            }
            _ => {}
        }
    }
}

/// One point of a FIR frequency response, as served by the
/// `/zones/{id}/convolver/response` endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ResponsePoint {
    pub freq_hz: f64,
    pub magnitude_db: f64,
    pub phase_deg: f64,
}

/// Log-spaced frequency grid (`n` points from `f_lo` to `f_hi` inclusive).
pub fn log_freq_grid(n: usize, f_lo: f64, f_hi: f64) -> Vec<f64> {
    if n == 0 || f_lo <= 0.0 || f_hi <= f_lo {
        return Vec::new();
    }
    if n == 1 {
        return vec![f_lo];
    }
    let ratio = (f_hi / f_lo).ln();
    (0..n)
        .map(|i| f_lo * (ratio * i as f64 / (n - 1) as f64).exp())
        .collect()
}

/// Evaluate the frequency response of a FIR filter at arbitrary frequencies by
/// DIRECT summation: H(f) = Σ h[n]·e^(−j2πfn/fs), accumulated in f64. No FFT —
/// the target grid is log-spaced, which an FFT bin grid can't serve without
/// interpolation, and ~200 freqs × 128k taps stays a few tens of millions of
/// f64 ops. The unit phasor is advanced by complex rotation (one sin/cos pair
/// per frequency); f64 rotation drift over 128k steps is ~1e-11, negligible.
///
/// Magnitude is 20·log10(|H|) floored at −200 dB (a true zero would be −inf,
/// which JSON can't carry); phase is the principal atan2 value in degrees.
/// Ecrire un WAV flottant 32 bits — le format que `read_wav_ir` relit sans
/// perte (`format_tag = 3`).
///
/// Volontairement minimal : un `fmt ` et un `data`, rien d'autre. Une reponse
/// impulsionnelle n'a que faire d'un `LIST/INFO`, et moins il y a de chunks,
/// moins il y a de facons de se tromper en les relisant.
fn ecrire_wav_float32(
    chemin: &str,
    entrelace: &[f32],
    canaux: u16,
    sample_rate: u32,
) -> Result<(), String> {
    let bits: u16 = 32;
    let bloc = canaux * bits / 8;
    let octets_par_seconde = sample_rate * bloc as u32;
    let taille_data = (entrelace.len() * 4) as u32;

    let mut w = Vec::with_capacity(44 + taille_data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + taille_data).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    w.extend_from_slice(&canaux.to_le_bytes());
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&octets_par_seconde.to_le_bytes());
    w.extend_from_slice(&bloc.to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&taille_data.to_le_bytes());
    for e in entrelace {
        w.extend_from_slice(&e.to_le_bytes());
    }
    std::fs::write(chemin, &w).map_err(|e| format!("ecriture du filtre combine : {e}"))
}

pub fn fir_frequency_response(
    taps: &[f32],
    sample_rate: u32,
    freqs_hz: &[f64],
) -> Vec<ResponsePoint> {
    let fs = sample_rate as f64;
    if taps.is_empty() || fs <= 0.0 {
        return Vec::new();
    }
    freqs_hz
        .iter()
        .map(|&f| {
            let theta = -2.0 * std::f64::consts::PI * f / fs;
            let (wi, wr) = theta.sin_cos();
            // Unit phasor e^(−j2πfn/fs), advanced by multiplication with w.
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for &h in taps {
                let h = h as f64;
                re += h * cr;
                im += h * ci;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
            let mag = (re * re + im * im).sqrt();
            let magnitude_db = if mag > 1e-10 {
                20.0 * mag.log10()
            } else {
                -200.0
            };
            ResponsePoint {
                freq_hz: f,
                magnitude_db,
                phase_deg: im.atan2(re).to_degrees(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une convolution par blocs a une latence : elle ne peut rendre un
    /// echantillon avant d'avoir vu le bloc qui le contient. Ce test lisait
    /// autrefois la sortie DANS le meme appel, ce qui n'est vrai que si la
    /// frontiere du bloc coincide avec celle de l'appel — l'hypothese meme qui
    /// faisait sous-deborder l'ancien code (#2209). On nourrit donc la latence
    /// et on lit decale.
    /// Convolution directe de reference — la definition, sans FFT ni blocs.
    fn convolution_directe(x: &[f32], h: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for (n, yn) in y.iter_mut().enumerate() {
            for (k, hk) in h.iter().enumerate() {
                if n >= k {
                    *yn += x[n - k] * hk;
                }
            }
        }
        y
    }

    /// Nourrit un convolveur neuf par lots de tailles donnees et rend la
    /// sortie complete, latence comprise.
    fn passer_par_lots(ir: &[Vec<f32>], block: usize, x: &[f32], lots: &[usize]) -> Vec<f32> {
        let mut conv = Convolver::new(ir, block);
        let mut entree = x.to_vec();
        entree.extend(std::iter::repeat(0.0).take(conv.latency_frames()));
        let mut sortie = Vec::with_capacity(entree.len());
        let mut i = 0;
        let mut t = 0;
        while i < entree.len() {
            let n = lots[t % lots.len()].min(entree.len() - i);
            t += 1;
            let mut morceau = entree[i..i + n].to_vec();
            conv.process_interleaved(&mut morceau);
            sortie.extend_from_slice(&morceau);
            i += n;
        }
        sortie
    }

    /// LE test que ce correctif demandait. Le decoupage des appels ne doit rien
    /// changer au resultat — c'est toute la difference entre un convolveur et
    /// un convolveur qui suppose que ses blocs arrivent alignes.
    ///
    /// Le cas `[100, 924]` est celui du ticket : bloc 1024, un premier appel de
    /// 100 trames, un second de 924. L'ancien code calculait
    /// `frame + 1 - block_size` = `924 - 1024` sur des `usize`.
    #[test]
    fn le_decoupage_des_appels_ne_change_pas_le_resultat() {
        let block = 1024;
        let ir = vec![
            (0..200)
                .map(|k| 1.0 / (1.0 + k as f32))
                .collect::<Vec<f32>>(),
        ];
        let x: Vec<f32> = (0..5000)
            .map(|n| ((n as f32) * 0.037).sin() * 0.8)
            .collect();

        let reference = passer_par_lots(&ir, block, &x, &[block]);
        for lots in [
            vec![1usize],
            vec![100, 924],
            vec![1024],
            vec![1500],
            vec![7, 3, 991, 64, 1500, 1],
        ] {
            let obtenu = passer_par_lots(&ir, block, &x, &lots);
            assert_eq!(obtenu.len(), reference.len(), "lots {lots:?}");
            for (i, (o, r)) in obtenu.iter().zip(reference.iter()).enumerate() {
                assert!(
                    (o - r).abs() < 1e-4,
                    "lots {lots:?}, trame {i} : {o} au lieu de {r}"
                );
            }
        }
    }

    /// Et le resultat doit etre LA convolution, pas seulement une valeur
    /// stable : on compare a la definition directe.
    #[test]
    fn le_resultat_est_la_convolution_directe() {
        let block = 64;
        let h: Vec<f32> = (0..40)
            .map(|k| ((k as f32) * 0.3).cos() / (1.0 + k as f32))
            .collect();
        let x: Vec<f32> = (0..1000).map(|n| ((n as f32) * 0.11).sin()).collect();

        let attendu = convolution_directe(&x, &h);
        let sortie = passer_par_lots(std::slice::from_ref(&h), block, &x, &[37]);
        let latence = block;
        for (n, a) in attendu.iter().enumerate() {
            let obtenu = sortie[latence + n];
            assert!(
                (obtenu - a).abs() < 1e-3,
                "trame {n} : {obtenu} au lieu de {a}"
            );
        }
    }

    /// Une IR stereo dont les canaux n'ont pas la meme longueur : la queue du
    /// canal le plus long etait ignoree, `process_block` bornant la boucle sur
    /// le nombre de partitions du canal 0.
    #[test]
    fn la_queue_dune_ir_plus_longue_sur_lautre_canal_nest_pas_perdue() {
        let block = 16;
        let court = vec![1.0f32];
        // Le canal droit porte une impulsion au-dela de la premiere partition.
        let mut long = vec![0.0f32; 40];
        long[0] = 1.0;
        long[33] = 0.5;
        let ir = vec![court, long];

        let mut conv = Convolver::new(&ir, block);
        let latence = conv.latency_frames();
        let trames = 128;
        let mut x = vec![0.0f32; trames * 2];
        x[0] = 1.0; // impulsion a gauche
        x[1] = 1.0; // impulsion a droite
        x.extend(std::iter::repeat(0.0).take(latence * 2));
        conv.process_interleaved(&mut x);

        let droite = |n: usize| x[(latence + n) * 2 + 1];
        assert!((droite(0) - 1.0).abs() < 1e-3, "premiere partition perdue");
        assert!(
            (droite(33) - 0.5).abs() < 1e-3,
            "queue au-dela de la premiere partition perdue : {}",
            droite(33)
        );
    }

    #[test]
    fn identity_ir() {
        let ir = vec![vec![1.0, 0.0, 0.0, 0.0]];
        let mut conv = Convolver::new(&ir, 4);
        let latence = conv.latency_frames();
        let attendu = [1.0f32, 0.5, 0.25, 0.125];
        let mut samples = attendu.to_vec();
        samples.extend(std::iter::repeat(0.0).take(latence));
        conv.process_interleaved(&mut samples);
        for (i, &a) in attendu.iter().enumerate() {
            let s = samples[latence + i];
            assert!((s - a).abs() < 0.001, "sample {i}: {s}");
        }
    }

    #[test]
    fn stereo_ir() {
        let ir = vec![vec![1.0, 0.0]; 2];
        let mut conv = Convolver::new(&ir, 4);
        let latence = conv.latency_frames();
        let mut samples = vec![1.0, 0.5, 0.25, 0.125, 1.0, 0.5, 0.25, 0.125];
        samples.extend(std::iter::repeat(0.0).take(latence * 2));
        conv.process_interleaved(&mut samples);
        assert!((samples[latence * 2] - 1.0).abs() < 0.01);
        assert!((samples[latence * 2 + 1] - 0.5).abs() < 0.01);
    }

    /// #2210 — la cadence n'est plus une métadonnée jetée après lecture du
    /// WAV. Chacune des cadences réellement livrées doit produire un moteur
    /// seulement pour un flux de même cadence.
    #[test]
    fn la_configuration_est_liee_aux_cadences_441_48_96_et_192_khz() {
        for sample_rate in [44_100, 48_000, 96_000, 192_000] {
            let config = ConvolverConfig::new(vec![vec![1.0, 0.5]], sample_rate).unwrap();
            let convolver = config
                .build_for(4, sample_rate, 2)
                .expect("une IR mono de même cadence doit être dupliquée");
            assert_eq!(convolver.channels(), 2);

            let other_rate = if sample_rate == 48_000 {
                96_000
            } else {
                48_000
            };
            // #8761 — par défaut, l'autre cadence est servie par le filtre
            // rééchantillonné ; le refus d'avant reste celui du réglage.
            assert_eq!(config.build_for(4, other_rate, 2).unwrap().channels(), 2);
            let error = config
                .clone()
                .avec_reechantillonnage(false)
                .build_for(4, other_rate, 2)
                .err()
                .expect("une cadence différente doit être refusée sans rééchantillonnage");
            assert!(error.contains(&sample_rate.to_string()), "{error}");
            assert!(error.contains(&other_rate.to_string()), "{error}");
            assert!(error.contains("réexportez"), "{error}");
        }
    }

    #[test]
    fn seule_une_ir_mono_est_dupliquee_implicitement() {
        let mono = ConvolverConfig::new(vec![vec![1.0]], 48_000).unwrap();
        assert_eq!(mono.build_for(4, 48_000, 8).unwrap().channels(), 8);

        let stereo = ConvolverConfig::new(vec![vec![1.0], vec![0.5]], 48_000).unwrap();
        let error = stereo
            .build_for(4, 48_000, 6)
            .err()
            .expect("un layout non mono doit correspondre exactement");
        assert!(
            error.contains("2 canaux") && error.contains("6 canaux"),
            "{error}"
        );
    }

    #[test]
    fn from_wav_skips_extra_chunks_before_data() {
        // Identity IR (4 taps, 16-bit mono @48k) with a spurious LIST chunk
        // inserted before `data` — exactly what the old fixed-44-byte parser
        // misread into noise. The chunk-walking parser must still find `data`.
        let ir_i16: [i16; 4] = [32767, 0, 0, 0];
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0, 0, 0, 0]); // RIFF size — patched below
        wav.extend_from_slice(b"WAVE");
        // fmt  (PCM)
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // channels
        wav.extend_from_slice(&48_000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&(48_000u32 * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits
        // spurious chunk before data
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(b"INFO");
        // data
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&((ir_i16.len() * 2) as u32).to_le_bytes());
        for s in ir_i16 {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());

        let ir_file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let path = ir_file.path().to_path_buf();
        std::fs::write(&path, &wav).unwrap();
        let mut conv = Convolver::from_wav(path.to_str().unwrap(), 4).unwrap();

        let latence = conv.latency_frames();
        let attendu = [1.0f32, 0.5, 0.25, 0.125];
        let mut samples = attendu.to_vec();
        samples.extend(std::iter::repeat(0.0).take(latence));
        conv.process_interleaved(&mut samples);
        for (i, &a) in attendu.iter().enumerate() {
            let s = samples[latence + i];
            assert!((s - a).abs() < 0.01, "sample {i}: {s}");
        }
    }

    #[test]
    fn process_offline_flushes_last_partial_block() {
        // IR = [1.0, 0.5], block 4. A 6-frame buffer is not a block multiple,
        // so the streaming path alone leaves frames 4-5 dry; process_offline
        // must flush them. Convolving an impulse yields the IR then silence.
        let ir = vec![vec![1.0f32, 0.5]];
        let mut conv = Convolver::new(&ir, 4);
        let mut samples = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        conv.process_offline(&mut samples);
        assert_eq!(samples.len(), 6, "length must be preserved");
        let expected = [1.0, 0.5, 0.0, 0.0, 0.0, 0.0];
        for (i, &e) in expected.iter().enumerate() {
            assert!(
                (samples[i] - e).abs() < 0.001,
                "frame {i}: {} != {e}",
                samples[i]
            );
        }
    }

    #[test]
    fn process_pcm_16bit_round_trip() {
        let ir = vec![vec![1.0f32, 0.5]];
        let mut conv = Convolver::new(&ir, 4);
        let frames: [i16; 6] = [32767, 0, 0, 0, 0, 0];
        let mut pcm: Vec<u8> = Vec::new();
        for f in frames {
            pcm.extend_from_slice(&f.to_le_bytes());
        }
        conv.process_pcm(&mut pcm, 16);
        let out: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert!((out[0] as i32 - 32767).abs() < 4, "out0={}", out[0]);
        assert!((out[1] as i32 - 16383).abs() < 4, "out1={}", out[1]); // 0.5 * full-scale
        for (i, &v) in out.iter().enumerate().skip(2) {
            assert!(v.abs() < 4, "out{i}={v}");
        }
    }

    #[test]
    fn frequency_response_unit_impulse_is_flat_0db() {
        // δ[n] → H(f) = 1 everywhere: 0 dB, 0° phase.
        let taps = vec![1.0f32, 0.0, 0.0, 0.0];
        let freqs = log_freq_grid(50, 20.0, 20_000.0);
        assert_eq!(freqs.len(), 50);
        assert!((freqs[0] - 20.0).abs() < 1e-9);
        assert!((freqs[49] - 20_000.0).abs() < 1e-6);
        for p in fir_frequency_response(&taps, 48_000, &freqs) {
            assert!(
                p.magnitude_db.abs() < 1e-6,
                "{} Hz: {} dB",
                p.freq_hz,
                p.magnitude_db
            );
            assert!(
                p.phase_deg.abs() < 1e-6,
                "{} Hz: {}°",
                p.freq_hz,
                p.phase_deg
            );
        }
    }

    #[test]
    fn frequency_response_pure_delay_flat_linear_phase() {
        // δ[n−1] → |H| = 1 (0 dB), phase = −360·f/fs degrees (no wrap below
        // fs/2 for a 1-sample delay).
        let taps = vec![0.0f32, 1.0];
        let fs = 48_000u32;
        let freqs = log_freq_grid(50, 20.0, 20_000.0);
        for p in fir_frequency_response(&taps, fs, &freqs) {
            assert!(
                p.magnitude_db.abs() < 1e-6,
                "{} Hz: {} dB",
                p.freq_hz,
                p.magnitude_db
            );
            let expected = -360.0 * p.freq_hz / fs as f64;
            assert!(
                (p.phase_deg - expected).abs() < 1e-6,
                "{} Hz: {}° != {expected}°",
                p.freq_hz,
                p.phase_deg
            );
        }
    }

    #[test]
    fn frequency_response_two_tap_average_lowpass() {
        // h = [0.5, 0.5] → |H(f)| = cos(πf/fs): −3.01 dB at fs/4, plunging
        // toward −∞ near fs/2 (floored at −200 dB by the implementation).
        let taps = vec![0.5f32, 0.5];
        let fs = 48_000u32;
        let pts = fir_frequency_response(&taps, fs, &[12_000.0, 23_990.0]);
        assert!(
            (pts[0].magnitude_db - (-3.0103)).abs() < 0.01,
            "fs/4: {} dB",
            pts[0].magnitude_db
        );
        assert!(
            pts[1].magnitude_db < -60.0,
            "near fs/2: {} dB",
            pts[1].magnitude_db
        );
    }

    /// Ecrire un WAV mono flottant pour les tests de combinaison.
    fn ecrire_ir_mono(chemin: &std::path::Path, taps: &[f32], sr: u32) {
        super::ecrire_wav_float32(chemin.to_str().unwrap(), taps, 1, sr).unwrap();
    }

    /// Deux filtres MONO, gauche et droite, deviennent un WAV stereo dont
    /// chaque canal porte SON filtre.
    ///
    /// C'est le chemin d'entree qui manquait : le moteur savait deja convoluer
    /// un canal par reponse, mais REW, Acourate et Audiolense exportent deux
    /// fichiers mono, jamais un stereo (Daniel, 24/08/2026).
    #[test]
    fn deux_filtres_mono_deviennent_un_stereo_par_canal() {
        let dir = crate::test_scratch::scratch_dir("tune-fir");
        let g = dir.join("gauche.wav");
        let d = dir.join("droite.wav");
        let out = dir.join("combine.wav");

        // Deux filtres RECONNAISSABLES et de longueurs differentes.
        ecrire_ir_mono(&g, &[1.0, 0.5, 0.25], 48_000);
        ecrire_ir_mono(&d, &[-1.0, -0.5], 48_000);

        let sr = Convolver::combiner_en_stereo(
            g.to_str().unwrap(),
            d.to_str().unwrap(),
            out.to_str().unwrap(),
        )
        .expect("la combinaison doit reussir");
        assert_eq!(sr, 48_000);

        let (ir, sr_relu) = Convolver::read_ir_taps(out.to_str().unwrap()).unwrap();
        assert_eq!(sr_relu, 48_000);
        assert_eq!(ir.len(), 2, "le fichier combine doit etre STEREO");
        assert_eq!(
            ir[0],
            vec![1.0, 0.5, 0.25],
            "le canal gauche doit porter le filtre gauche, intact"
        );
        assert_eq!(
            ir[1],
            vec![-1.0, -0.5, 0.0],
            "le canal droit doit porter le filtre droit, complete de zeros — un \
             zero est neutre pour une convolution, contrairement a une repetition"
        );
    }

    /// Des cadences differentes sont un refus, pas un rattrapage silencieux :
    /// convoluer a deux cadences decalerait un canal par rapport a l'autre.
    #[test]
    fn deux_cadences_differentes_sont_refusees() {
        let dir = crate::test_scratch::scratch_dir("tune-fir-sr");
        let g = dir.join("g.wav");
        let d = dir.join("d.wav");
        let out = dir.join("o.wav");
        ecrire_ir_mono(&g, &[1.0, 0.0], 48_000);
        ecrire_ir_mono(&d, &[1.0, 0.0], 44_100);

        let e = Convolver::combiner_en_stereo(
            g.to_str().unwrap(),
            d.to_str().unwrap(),
            out.to_str().unwrap(),
        )
        .expect_err("deux cadences differentes doivent etre refusees");
        assert!(
            e.contains("48000") && e.contains("44100"),
            "le refus doit NOMMER les deux cadences, sinon l'utilisateur ne sait \
             pas lequel de ses deux fichiers reexporter — recu : {e}"
        );
    }

    /// Le fichier combine doit vraiment traverser le convolveur : deux
    /// impulsions de signes opposes doivent ressortir de signes opposes.
    #[test]
    fn le_stereo_combine_convolue_chaque_canal_avec_son_filtre() {
        let dir = crate::test_scratch::scratch_dir("tune-fir-conv");
        let g = dir.join("g.wav");
        let d = dir.join("d.wav");
        let out = dir.join("o.wav");
        // Gauche : gain 1. Droite : gain -1 (phase inversee).
        ecrire_ir_mono(&g, &[1.0], 48_000);
        ecrire_ir_mono(&d, &[-1.0], 48_000);
        Convolver::combiner_en_stereo(
            g.to_str().unwrap(),
            d.to_str().unwrap(),
            out.to_str().unwrap(),
        )
        .unwrap();

        let mut conv = Convolver::from_wav(out.to_str().unwrap(), 4).unwrap();
        let latence = conv.latency_frames();
        // Une impulsion identique sur les deux canaux, entrelacee.
        let mut sortie = vec![0.0f32; (latence + 4) * 2];
        sortie[0] = 1.0;
        sortie[1] = 1.0;
        // Traitement EN PLACE : la tranche porte l'entree puis la sortie.
        conv.process_interleaved(&mut sortie);

        let i = latence * 2;
        assert!(
            (sortie[i] - 1.0).abs() < 1e-4,
            "le canal gauche doit sortir en phase : {}",
            sortie[i]
        );
        assert!(
            (sortie[i + 1] + 1.0).abs() < 1e-4,
            "le canal droit doit sortir en phase INVERSEE — s'il sort comme le \
             gauche, les deux canaux partagent le meme filtre : {}",
            sortie[i + 1]
        );
    }
}

/// #7632 — extinction de la convolution flux en cours (droit Premium retiré).
#[cfg(test)]
mod extinction_7632_tests {
    use super::*;

    /// Un sinus stéréo de 1 kHz à 48 kHz.
    fn sinus(trames: usize) -> Vec<f32> {
        (0..trames)
            .flat_map(|n| {
                let v = (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    /// IR d'un gain de 0,25 : la convolution rend `0,25 · x` retardé.
    fn moteur() -> Convolver {
        let mut ir = vec![0.0f32; 300];
        ir[0] = 0.25;
        Convolver::new(&[ir.clone(), ir], 256)
    }

    #[test]
    fn le_fondu_rend_l_entree_intacte_sans_saut_7632() {
        let mut c = moteur();
        let lat = c.latency_frames();
        let x = sinus(48_000);
        let mut y = x.clone();
        // 0,2 s convoluées, puis extinction en 480 trames (10 ms).
        let coupe = 9_600 * 2;
        c.process_interleaved(&mut y[..coupe]);
        c.eteindre_en_fondu(480);
        c.process_interleaved(&mut y[coupe..]);
        assert!(c.est_eteint());

        // Avant : convolué (0,25 · x retardé). Après le fondu : x retardé,
        // À L'ÉCHANTILLON PRÈS — aucune convolution.
        for n in (lat + 10)..(coupe / 2) {
            assert!(
                (y[2 * n] - 0.25 * x[2 * (n - lat)]).abs() < 1e-4,
                "trame {n}"
            );
        }
        for n in (coupe / 2 + 480)..48_000 {
            assert_eq!(y[2 * n], x[2 * (n - lat)], "trame {n} : encore convoluée");
            assert_eq!(y[2 * n + 1], x[2 * (n - lat) + 1]);
        }
        // Pas de clic : aucun pas d'échantillon plus grand que celui du sinus
        // d'entrée (la pente maximale d'un sinus de 0,5 à 1 kHz).
        let pas_max_entree = x
            .chunks(2)
            .zip(x.chunks(2).skip(1))
            .map(|(a, b)| (b[0] - a[0]).abs())
            .fold(0.0f32, f32::max);
        for n in (lat + 1)..48_000 {
            let pas = (y[2 * n] - y[2 * (n - 1)]).abs();
            assert!(
                pas <= pas_max_entree * 1.05,
                "saut de {pas} à la trame {n} (entrée : {pas_max_entree}) : clic"
            );
        }
    }

    #[test]
    fn eteint_le_flush_rend_la_reserve_directe_7632() {
        let mut c = moteur();
        let lat = c.latency_frames();
        c.eteindre_en_fondu(0);
        let x = sinus(1_000);
        let mut y = x.clone();
        c.process_interleaved(&mut y);
        let queue = c.flush();
        assert_eq!(queue.len(), lat * 2);
        // Les `lat` dernières trames d'entrée sortent telles quelles.
        assert_eq!(&queue[..], &x[(1_000 - lat) * 2..]);
    }
}
