//! REF-8 (#2219) — **l'étage natif** : des octets source vers un
//! [`BlocPcm`] de mots `i32` alignés à gauche, pour un [`PuitsNatif`].
//!
//! C'est le jumeau entier d'`EtageDeConversion` (R1) pour les bras Windows
//! exclusifs. `EtageDeConversion` rend des `f32` au format ouvert et refuse un
//! porteur DoP que ce chemin détruirait ; cet étage-ci rend des **mots
//! entiers** et **porte** le DoP tel quel — la décision DoP (`is_dop_pcm`,
//! verrouillée par `dop_latched`) reste ICI, dans le producteur, comme avant,
//! et le volume aussi (D3 : jamais dans le rappel de rendu). Ce que
//! `bras_wasapi.rs` faisait en ligne à `49ecf1fe` — `leftover`,
//! `must_classify_24_bit`, `dop_latched`, `feed_windows_native_exclusive_leftover`,
//! le reliquat 24 bits forcé brut, la queue du DSP — tient dans ce type.
//!
//! # Le mot du bloc, et ce que [`ProfondeurPcm`] en déclare
//!
//! Un bloc livré par cet étage porte **toujours** `ProfondeurPcm::Entier32`,
//! quelle que soit la profondeur de la source. Le mot est celui de
//! [`NativePcmRing`] : un `i32` petit-boutien, aligné à gauche — un mot 16 bits
//! occupe les bits 31..16, un mot 24 bits les bits 31..8, un mot 32 bits tout
//! le mot — et les bits bas inoccupés sont nuls. `Entier32` est la seule
//! variante du jeu fermé qui dise « quatre octets, entier signé » ; ce n'est
//! pas un mensonge sur la source (la source est dans [`EtageNatif::spec`]),
//! c'est l'étiquette du **transport** entre l'étage et le puits. Le fil de
//! rendu WASAPI resérialise ce mot en 2, 3 ou 4 octets par `pop_pcm_bytes`
//! (les octets HAUTS), et c'est ainsi que le mot source revient à l'identique.
//! `dop_stereo_24le_64frames.hex` traverse l'étage, le puits, l'anneau et
//! `pop_pcm_bytes` sans qu'un marqueur bouge : `empreinte_wasapi_f70496.rs`.
//!
//! # Compilation
//!
//! `cfg(any(target_os = "windows", test))`, comme les aides qu'il appelle
//! (`prepare_windows_native_pcm`, `pcm_bytes_to_native_i32`,
//! `f32_to_native_i32`) : tout ce qui est ici est jugé sur Shrek par
//! `cargo test`, AVANT l'étape Windows de la CI. `super` désigne
//! `outputs::local`.
//!
//! # Coordination (nuit du 12/09)
//!
//! L'agent A rend `BoucleProducteur::tourner` générique sur un trait `Etage`.
//! Sa branche n'existait pas quand ce fichier a été écrit : les quatre
//! méthodes attendues sont là, sous les noms convenus (`decoder_et_pousser`,
//! `rendre_la_queue`, `vider`, `transformations`), et le bras WASAPI garde
//! sa boucle en attendant. L'agent ASIO lit ce fichier : [`PuitsAnneauNatif`]
//! est le puits de TOUT anneau `NativePcmRing`, pas seulement de WASAPI.

use super::*;
use crate::outputs::traits::{ProfondeurPcm, PuitsNatif, TransformationsReelles};

/// Ce qu'une poussée vers le puits natif a produit.
///
/// Jumeau de `PousseeVersLePuits` (R1), sans `PorteurDopRefuse` : la route
/// native ne refuse pas le DoP, elle le **porte** et le dit (`dop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EcritureNative {
    /// Rien d'aligné à décoder — ou la quarantaine 24 bits n'a pas encore ses
    /// 32 trames : les octets restent en attente, aucun mot n'a été produit.
    RienAPousser,
    /// Bloc poussé : `trames_source` trames consommées à l'entrée, et le
    /// verdict que le producteur remet au rappel — DoP porté, octets source
    /// conservés (`bit_perfect`) ou aller-retour flottant (volume, DSP).
    Poussee {
        trames_source: u64,
        dop: bool,
        bit_perfect: bool,
    },
    /// Le puits a cessé de consommer (anneau jamais drainé : rappel mort).
    /// Les trames et le verdict sont rendus quand même : le bras les comptait
    /// et les publiait déjà sans regarder le verdict du puits, et ce compte
    /// est la position.
    PuitsMort {
        trames_source: u64,
        dop: bool,
        bit_perfect: bool,
    },
}

/// Ce que [`EtageNatif::vider`] a poussé du reliquat 24 bits, brut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReliquatForceBrut {
    /// Octets alignés poussés tels quels, à l'unité (0 si rien n'était aligné).
    pub(super) octets: usize,
    pub(super) trames: u64,
}

/// L'étage natif : source → mots `i32` alignés à gauche → [`PuitsNatif`].
pub(super) struct EtageNatif<'a> {
    /// Le format de la SOURCE : celui dans lequel `en_attente` se lit.
    spec: AudioSpec,
    /// Le format réellement OUVERT par le périphérique : la cadence de la
    /// source, et ses canaux — sauf quand la grille de réaffectation pilote
    /// les voies (#6230 : une grille 6 → 2 ouvre 2 voies). Les blocs livrés
    /// au puits portent ces voies-là ([`EtageNatif::spec_du_puits`]).
    sortie: FormatOuvert,
    /// Octets reçus de l'amont, pas encore alignés sur une trame source — ou
    /// en quarantaine 24 bits tant que la première sonde DoP n'a pas ses
    /// 32 trames.
    en_attente: Vec<u8>,
    /// Vrai tant que la première fenêtre 24 bits n'a pas été classée.
    must_classify_24_bit: bool,
    /// La décision DoP, verrouillée après la première fenêtre classée DoP.
    dop_latched: bool,
    volume: &'a AtomicU32,
    eq: &'a std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &'a std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &'a std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &'a AtomicBool,
    mono_downmix: &'a AtomicBool,
    /// #6247 — la réaffectation des canaux et la disposition déclarée de la
    /// sortie, quand le bras les a branchées
    /// ([`EtageNatif::avec_l_adaptation_des_canaux`]). `None` : aucun
    /// déplacement de voie, comme avant.
    adaptation: Option<AdaptationNative<'a>>,
    publication: Option<&'a std::sync::Mutex<Option<TransformationsReelles>>>,
}

impl<'a> EtageNatif<'a> {
    /// Monte l'étage sur un format source, comme `bras_wasapi.rs` posait ses
    /// variables locales : `must_classify_24_bit = bit_depth == 24`,
    /// `dop_latched = false`, `leftover` vide.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn monter(
        spec: AudioSpec,
        sortie: FormatOuvert,
        volume: &'a AtomicU32,
        eq: &'a std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
        convolver: &'a std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
        crossfeed: &'a std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
        pure_bypass: &'a AtomicBool,
        mono_downmix: &'a AtomicBool,
    ) -> Self {
        Self {
            spec,
            sortie,
            en_attente: Vec::new(),
            must_classify_24_bit: spec.profondeur() == ProfondeurPcm::Entier24,
            dop_latched: false,
            volume,
            eq,
            convolver,
            crossfeed,
            pure_bypass,
            mono_downmix,
            adaptation: None,
            publication: None,
        }
    }

    /// #7093 — le format réellement ouvert, puis le verdict PCM/DoP de
    /// chaque bloc, sont publiés vers la zone sans changer les mots rendus.
    #[cfg(any(target_os = "windows", test))]
    pub(super) fn avec_publication(
        mut self,
        creneau: &'a std::sync::Mutex<Option<TransformationsReelles>>,
    ) -> Self {
        self.publication = Some(creneau);
        self.publier();
        self
    }

    fn publier(&self) {
        if let Some(creneau) = self.publication
            && let Ok(mut slot) = creneau.lock()
        {
            *slot = Some(self.transformations());
        }
    }

    /// #6247 — brancher la réaffectation des canaux (#6044) et la disposition
    /// déclarée par le fichier (#6057) de la sortie sur cet étage. Le bras
    /// WASAPI exclusif ne les recevait pas : la matrice d'une zone ou d'un
    /// album n'y était jamais appliquée (Didier, Marantz AV 7706, 7.1). Elles
    /// sont relues à chaque bloc, comme dans l'étage de conversion : une
    /// règle posée à chaud prend effet au bloc suivant.
    pub(super) fn avec_l_adaptation_des_canaux(
        mut self,
        reaffectation: &'a CreneauReaffectation,
        disposition: &'a CreneauDisposition,
    ) -> Self {
        self.adaptation = Some((reaffectation, disposition));
        self
    }

    /// Le format de la source.
    pub(super) fn spec(&self) -> AudioSpec {
        self.spec
    }

    /// Le format des blocs livrés au puits : `Entier32` à la cadence de la
    /// source, sur les voies OUVERTES — celles de la source, sauf grille qui
    /// pilote les voies (#6230). Voir l'en-tête du module.
    pub(super) fn spec_du_puits(&self) -> AudioSpec {
        spec_du_puits_natif_vers(self.spec, self.sortie.canaux)
    }

    /// #6230 — les voies ouvertes diffèrent-elles des canaux de la source ?
    fn voies_pilotees(&self) -> bool {
        self.sortie.canaux != self.spec.canaux()
    }

    /// La décision DoP est-elle verrouillée ?
    // #6734 — le bras natif S32 de Linux ne s'en sert pas.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(super) fn dop_verrouille(&self) -> bool {
        self.dop_latched
    }

    /// La première fenêtre 24 bits attend-elle encore d'être classée ?
    // #6734 — le bras natif S32 de Linux ne s'en sert pas.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(super) fn quarantaine_24_bits_ouverte(&self) -> bool {
        self.must_classify_24_bit
    }

    /// Octets reçus et pas encore poussés.
    // #6734 — le bras natif S32 de Linux ne s'en sert pas.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(super) fn en_attente(&self) -> &[u8] {
        &self.en_attente
    }

    /// Profondeur de la source, telle qu'un en-tête WAV la déclare — ce que
    /// les aides `prepare_windows_native_pcm`, `pcm_bytes_to_native_i32` et
    /// `f32_to_native_i32` prennent encore en `u16` nu.
    fn bit_depth(&self) -> u16 {
        self.spec.profondeur().bits_declares()
    }

    /// Range `mots` dans le puits, sous l'étiquette `Entier32`. Rend le verdict
    /// du puits (`false` = puits mort).
    fn pousser_les_mots(&self, mots: &[i32], puits: &mut dyn PuitsNatif) -> bool {
        let octets: Vec<u8> = mots.iter().flat_map(|mot| mot.to_le_bytes()).collect();
        puits.ecrire(self.spec_du_puits().bloc(&octets))
    }

    /// Le geste élémentaire : ajouter `octets` à l'attente, décoder ce qui est
    /// aligné (`prepare_windows_native_pcm` : sonde DoP, volume, DSP, mot
    /// natif), pousser.
    ///
    /// C'était `feed_windows_native_exclusive_leftover` de `local.rs` (retiré
    /// par REF-10, #2219), avec les quatre variables d'état du bras à
    /// l'intérieur du type. Même ordre : préparation, puis
    /// `must_classify_24_bit = false` et `dop_latched`, puis poussée, puis
    /// `drain` de l'attente.
    pub(super) fn decoder_et_pousser(
        &mut self,
        octets: &[u8],
        puits: &mut dyn PuitsNatif,
    ) -> EcritureNative {
        self.en_attente.extend_from_slice(octets);
        let frame_bytes = self.spec.octets_par_trame();
        let aligned_len = (self.en_attente.len() / frame_bytes) * frame_bytes;
        if aligned_len == 0 {
            return EcritureNative::RienAPousser;
        }
        let Some(prepared) = prepare_windows_native_pcm(
            &self.en_attente[..aligned_len],
            self.bit_depth(),
            self.spec.canaux(),
            self.sortie.canaux,
            self.must_classify_24_bit,
            self.dop_latched,
            self.volume.load(Ordering::SeqCst),
            self.eq,
            self.convolver,
            self.crossfeed,
            self.pure_bypass,
            self.mono_downmix,
            self.adaptation,
        ) else {
            // La première sonde 24 bits n'a pas ses 32 trames : les octets
            // restent en quarantaine, aucun mot n'a été produit.
            return EcritureNative::RienAPousser;
        };

        self.must_classify_24_bit = false;
        self.dop_latched = prepared.dop;
        self.publier();
        let vivant = self.pousser_les_mots(&prepared.samples, puits);
        self.en_attente.drain(..aligned_len);
        let trames_source = (aligned_len / frame_bytes) as u64;
        if vivant {
            EcritureNative::Poussee {
                trames_source,
                dop: prepared.dop,
                bit_perfect: prepared.bit_perfect,
            }
        } else {
            EcritureNative::PuitsMort {
                trames_source,
                dop: prepared.dop,
                bit_perfect: prepared.bit_perfect,
            }
        }
    }

    /// La queue du DSP (#2209), au format de la piste qui se termine : le
    /// convolveur est vidé, la queue traverse crossfeed et repli mono comme le
    /// corps de la piste, le volume s'applique ICI (D3), et les `f32` sont
    /// quantifiés une fois en mots natifs. Rend le verdict du puits ; `true`
    /// s'il n'y avait rien à rendre.
    ///
    /// `flush_local_dsp` reçoit `dop = false`, comme le bras le faisait : un
    /// DoP verrouillé n'a jamais alimenté le convolveur, sa queue est vide.
    pub(super) fn rendre_la_queue(&mut self, puits: &mut dyn PuitsNatif) -> bool {
        let queue = flush_local_dsp(
            self.convolver,
            self.crossfeed,
            self.pure_bypass,
            self.mono_downmix,
            self.spec.canaux(),
            false,
        );
        if queue.is_empty() {
            return true;
        }
        let volume_factor = self.volume.load(Ordering::SeqCst) as f32 / 1000.0;
        // #6230 — la queue sort aux canaux de la source : elle passe par la
        // même adaptation que le corps de la piste avant d'entrer au puits.
        let mut queue = if self.voies_pilotees() {
            self.adapter_hors_bloc(queue)
        } else {
            queue
        };
        if volume_factor != 1.0 {
            for sample in &mut queue {
                *sample *= volume_factor;
            }
        }
        let native = f32_to_native_i32(&queue, self.bit_depth());
        self.pousser_les_mots(&native, puits)
    }

    /// Le reliquat de fin de flux : moins de 32 trames 24 bits initiales ne se
    /// classent pas, mais l'anneau entier peut les porter sans risque. Elles
    /// partent brutes et à l'unité plutôt que devinées PCM et soumises à
    /// l'arithmétique d'échantillons. `None` si la quarantaine est fermée ou
    /// l'attente vide — rien à vider.
    ///
    /// Le verdict du puits est ignoré, comme le bras l'ignorait.
    pub(super) fn vider(&mut self, puits: &mut dyn PuitsNatif) -> Option<ReliquatForceBrut> {
        if !(self.must_classify_24_bit && !self.en_attente.is_empty()) {
            return None;
        }
        let frame_bytes = self.spec.octets_par_trame();
        let aligned = (self.en_attente.len() / frame_bytes) * frame_bytes;
        // #6230 — sur des voies pilotées, rien ne part brut : le reliquat est
        // porté aux voies ouvertes, à l'unité, comme le corps de la piste.
        let native = if self.voies_pilotees() {
            let float = pcm_bytes_to_f32(&self.en_attente[..aligned], self.bit_depth());
            f32_to_native_i32(&self.adapter_hors_bloc(float), self.bit_depth())
        } else {
            pcm_bytes_to_native_i32(&self.en_attente[..aligned], self.bit_depth())
        };
        let _ = self.pousser_les_mots(&native, puits);
        self.en_attente.drain(..aligned);
        Some(ReliquatForceBrut {
            octets: aligned,
            trames: (aligned / frame_bytes) as u64,
        })
    }

    /// #5204 — **la frontière d'une piste enchaînée** sur un transport natif
    /// exclusif, qui n'a ni rééchantillonneur ni adaptation de canaux de son
    /// propre chef.
    ///
    /// `Err(Rouvrir)` : le format change, on n'enchaîne PAS. Rien n'a été
    /// touché — ni l'étage ni le puits : la piste courante se termine comme si
    /// l'en-tête suivant avait été illisible, et la fin naturelle rouvre le
    /// périphérique au nouveau format.
    ///
    /// `Ok(())` : même format. L'état du DSP est CONSERVÉ (l'audio est
    /// continu, sa queue ne se rend pas ici), mais la décision PCM/DoP est
    /// reprise à zéro pour la nouvelle piste (#2296/#2232) : quarantaine
    /// 24 bits réarmée, DoP déverrouillé. Le reste non aligné de la piste
    /// précédente — moins d'une trame — est jeté, comme le chemin partagé le
    /// jette ; l'appelant a déjà forcé brut un reliquat de quarantaine par
    /// [`EtageNatif::vider`].
    ///
    /// #6230 — `amorce` : le début du PCM de la suivante. Les voies que la
    /// suivante ouvrirait se décident comme à l'ouverture
    /// ([`CreneauReaffectation::voies_a_ouvrir_en_exclusif`]) : une grille
    /// 6 → 2 ouverte sur 2 voies enchaîne la piste 5.1 suivante dans le même
    /// flux ; sans grille, la règle « à format égal » reste entière.
    pub(super) fn enchainer_la_piste(
        &mut self,
        suivante: AudioSpec,
        amorce: &[u8],
    ) -> Result<(), super::enchainement_exclusif::EnchainementNatif> {
        use super::enchainement_exclusif::{EnchainementNatif, decider_l_enchainement_natif_vers};
        // Les voies de la grille ne comptent que si l'ouverture EN COURS est
        // pilotée (étage ouvert sur d'autres voies que la source). Tant que
        // les bras ouvrent au format source (étape 2 de #6230 non faite), une
        // grille armée laisse la règle « à format égal » entière : l'album
        // 5.1 s'enchaîne comme avant, sans rouvrir le périphérique à chaque
        // piste.
        let voies = match self.adaptation {
            Some((reaffectation, _)) if self.sortie.canaux != self.spec.canaux() => reaffectation
                .voies_a_ouvrir_en_exclusif(
                    suivante,
                    amorce,
                    self.pure_bypass.load(Ordering::Relaxed),
                    false,
                ),
            _ => suivante.canaux(),
        };
        match decider_l_enchainement_natif_vers(self.spec, self.sortie, suivante, voies) {
            EnchainementNatif::Rouvrir => Err(EnchainementNatif::Rouvrir),
            EnchainementNatif::Enchainer => {
                self.en_attente.clear();
                self.must_classify_24_bit = suivante.profondeur() == ProfondeurPcm::Entier24;
                self.dop_latched = false;
                self.publier();
                Ok(())
            }
        }
    }

    /// #6230 — porter des flottants hors bloc (queue du DSP, reliquat) des
    /// canaux de la source aux voies ouvertes, par la même décision que les
    /// blocs (`adaptation_retenue_pour`), sans noter le créneau.
    fn adapter_hors_bloc(&self, float: Vec<f32>) -> Vec<f32> {
        let (canaux, voies) = (self.spec.canaux(), self.sortie.canaux);
        let (matrice, declaree) = match self.adaptation {
            Some((reaffectation, disposition)) => adaptation_retenue_pour(
                reaffectation,
                disposition,
                canaux,
                voies,
                false,
                self.pure_bypass.load(Ordering::Relaxed),
            ),
            None => (None, None),
        };
        adapter_les_voies_natives(
            float,
            canaux,
            voies,
            matrice.as_deref(),
            declaree.as_deref(),
        )
    }

    /// Ce que l'étage a réellement fait au signal : aucun rééchantillonnage,
    /// pas d'adaptation de canaux sauf voies pilotées par la grille (#6230,
    /// que `TransformationsReelles` lit dans le format ouvert), et le DSP tel
    /// que `local_dsp_runtime_state` le décrit — appliqué, ou contourné par
    /// DoP, par le bypass pur, ou inactif.
    pub(super) fn transformations(&self) -> TransformationsReelles {
        let dsp_actif = dsp_touche_le_signal(
            self.spec.canaux(),
            &AtomicBool::new(self.dop_latched),
            self.pure_bypass,
            self.eq,
            self.convolver,
            self.crossfeed,
            self.mono_downmix,
        );
        // #6247 — une matrice de réaffectation appliquée touche le signal,
        // même règle que `EtageDeConversion::dsp_actif`.
        let canaux = self.spec.canaux();
        let reaffecte = self.adaptation.is_some_and(|(reaffectation, _)| {
            !self.dop_latched
                && !self.pure_bypass.load(Ordering::Relaxed)
                && reaffectation.pour(canaux, self.sortie.canaux).is_some()
        });
        // #6057 — la disposition déclarée qui réordonne des voies à nombre
        // égal (`prepare_windows_native_pcm` l'applique au même endroit).
        let reordonne = self.adaptation.is_some_and(|(reaffectation, disposition)| {
            super::reorganisation_a_nombre_egal(
                reaffectation,
                disposition,
                canaux,
                canaux,
                self.dop_latched,
                self.pure_bypass.load(Ordering::Relaxed),
            )
        });
        TransformationsReelles::nouvelles(self.spec, self.sortie, dsp_actif || reaffecte)
            .avec_reorganisation_canaux(reordonne)
    }
}

/// Le format des blocs que l'étage natif livre pour une source donnée.
pub(super) fn spec_du_puits_natif(source: AudioSpec) -> AudioSpec {
    spec_du_puits_natif_vers(source, source.canaux())
}

/// #6230 — le format des blocs de l'étage natif quand le transport est
/// ouvert sur `voies` voies (celles de la grille de réaffectation qui pilote
/// l'ouverture, ou celles de la source).
pub(super) fn spec_du_puits_natif_vers(source: AudioSpec, voies: u16) -> AudioSpec {
    AudioSpec::nouvelle(source.cadence(), ProfondeurPcm::Entier32, voies.max(1))
        .expect("au moins une voie : c'est l'invariant d'AudioSpec")
}

/// Les mots natifs d'un bloc `Entier32` : l'inverse exact de
/// `i32::to_le_bytes`, mot par mot.
///
/// TOUS les mots complets sont rendus, trames entières ou non : c'est ce que
/// le bras poussait dans l'anneau (`prepared.samples`, puis la queue du
/// convolveur telle que `flush` la rend), échantillon par échantillon, sans
/// jamais regarder la frontière de trame.
pub(super) fn mots_natifs_du_bloc(bloc: BlocPcm<'_>) -> Vec<i32> {
    bloc.octets()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|mot| i32::from_le_bytes([mot[0], mot[1], mot[2], mot[3]]))
        .collect()
}

/// Le puits d'un [`NativePcmRing`] : l'anneau entier que draine le rappel de
/// rendu (WASAPI `pop_pcm_bytes`, ASIO `pop_mapped`).
///
/// Il porte les trois témoins d'arrêt du fil de lecture parce que l'attente a
/// lieu ICI : quand l'anneau est plein, c'est [`pousser_dans_l_anneau_natif`]
/// qui dort, et c'est donc lui qu'un arrêt doit pouvoir réveiller — même
/// raison que `PuitsAnneauCpal` (`backend.rs`).
///
/// R8 : l'anneau est un `Arc` partagé avec le backend qui le possède ; le
/// puits en est l'écrivain, le fil de rendu en est le lecteur.
pub(super) struct PuitsAnneauNatif<'a> {
    anneau: Arc<NativePcmRing>,
    /// Le format fixé à l'ouverture : un bloc qui en porte un autre tue le
    /// puits (même contrat que `CaptureOutputNatif`).
    spec: AudioSpec,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
    vivant: bool,
}

impl<'a> PuitsAnneauNatif<'a> {
    pub(super) fn sur(
        anneau: Arc<NativePcmRing>,
        spec: AudioSpec,
        stop_rx: &'a std::sync::mpsc::Receiver<()>,
        paused: &'a AtomicBool,
        force_silent: &'a AtomicBool,
    ) -> Self {
        Self {
            anneau,
            spec,
            stop_rx,
            paused,
            force_silent,
            vivant: true,
        }
    }
}

impl PuitsNatif for PuitsAnneauNatif<'_> {
    /// Rend `false` **uniquement** quand l'anneau n'a plus été drainé depuis
    /// cinq secondes (rappel mort) — ou quand un bloc contredit le format
    /// ouvert, ce qui rompt le contrat et tue le puits. Un arrêt demandé rend
    /// `true`, comme le contrat l'exige.
    fn ecrire(&mut self, bloc: BlocPcm<'_>) -> bool {
        if !self.vivant {
            return false;
        }
        if bloc.spec() != self.spec {
            warn!(
                attendue = ?self.spec,
                recue = ?bloc.spec(),
                "windows_native_sink_spec_mismatch"
            );
            self.vivant = false;
            return false;
        }
        let mots = mots_natifs_du_bloc(bloc);
        if !pousser_dans_l_anneau_natif(
            &self.anneau,
            &mots,
            self.stop_rx,
            self.paused,
            Some(self.force_silent),
        ) {
            self.vivant = false;
            return false;
        }
        true
    }
}

/// Pousse `samples` dans l'anneau en attendant qu'il se libère ; rend `true`
/// sur arrêt demandé, `false` après cinq secondes sans qu'un mot ne parte.
///
/// C'était `feed_native_ring_abortable` de `local.rs`, mot pour mot — le même
/// nom d'événement, `windows_native_feed_ring_stall_timeout`. ASIO passe
/// désormais par [`PuitsAnneauNatif`] : l'original a perdu son dernier
/// appelant et REF-10 (#2219) l'a retiré — un seul exemplaire, ici.
fn pousser_dans_l_anneau_natif(
    ring: &NativePcmRing,
    samples: &[i32],
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    abort: Option<&AtomicBool>,
) -> bool {
    let mut offset = 0;
    let mut last_progress_at = std::time::Instant::now();
    while offset < samples.len() {
        if stop_rx.try_recv().is_ok() || abort.is_some_and(|a| a.load(Ordering::Relaxed)) {
            return true;
        }
        while paused.load(Ordering::Relaxed) {
            if stop_rx.try_recv().is_ok() || abort.is_some_and(|a| a.load(Ordering::Relaxed)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            last_progress_at = std::time::Instant::now();
        }
        let written = ring.push(&samples[offset..]);
        offset += written;
        if written == 0 {
            if last_progress_at.elapsed() >= std::time::Duration::from_secs(5) {
                warn!(
                    remaining_samples = samples.len() - offset,
                    "windows_native_feed_ring_stall_timeout"
                );
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        } else {
            last_progress_at = std::time::Instant::now();
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::traits::CaptureOutputNatif;

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

    fn stereo(profondeur: ProfondeurPcm) -> AudioSpec {
        AudioSpec::nouvelle(48_000, profondeur, 2).unwrap()
    }

    fn octets(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + 7) as u8).collect()
    }

    /// La trame coupée par la frontière du tampon est REPORTÉE, jamais jetée
    /// (#3849) : deux lectures qui coupent une trame 24 bits stéréo en deux
    /// rendent exactement les mêmes mots qu'une seule.
    #[test]
    fn le_reste_non_aligne_est_reporte_sur_la_lecture_suivante() {
        let dsp = Dsp::au_repos();
        let spec = stereo(ProfondeurPcm::Entier16);
        let source = octets(4 * 100 + 1);
        let mut d_un_coup = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
        let mut etage = dsp.etage(spec);
        assert!(matches!(
            etage.decoder_et_pousser(&source, &mut d_un_coup),
            EcritureNative::Poussee {
                trames_source: 100,
                dop: false,
                bit_perfect: true
            }
        ));
        assert_eq!(etage.en_attente(), &source[400..]);

        let mut en_deux = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
        let mut etage = dsp.etage(spec);
        let coupe = 4 * 37 + 3;
        assert!(matches!(
            etage.decoder_et_pousser(&source[..coupe], &mut en_deux),
            EcritureNative::Poussee {
                trames_source: 37,
                ..
            }
        ));
        assert!(matches!(
            etage.decoder_et_pousser(&source[coupe..], &mut en_deux),
            EcritureNative::Poussee {
                trames_source: 63,
                ..
            }
        ));
        assert_eq!(d_un_coup.empreinte(), en_deux.empreinte());
        assert_eq!(d_un_coup.trames(), 100);
        assert_eq!(en_deux.trames(), 100);
    }

    /// Moins de 32 trames 24 bits : rien ne sort, tout attend. La 32e trame
    /// libère la fenêtre entière.
    #[test]
    fn la_quarantaine_24_bits_retient_tout_jusqu_a_la_32e_trame() {
        let dsp = Dsp::au_repos();
        let spec = stereo(ProfondeurPcm::Entier24);
        let mut capture = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
        let mut etage = dsp.etage(spec);
        let source = octets(6 * 40);
        assert_eq!(
            etage.decoder_et_pousser(&source[..6 * 31], &mut capture),
            EcritureNative::RienAPousser
        );
        assert!(etage.quarantaine_24_bits_ouverte());
        assert_eq!(capture.blocs(), 0);
        assert!(matches!(
            etage.decoder_et_pousser(&source[6 * 31..], &mut capture),
            EcritureNative::Poussee {
                trames_source: 40,
                dop: false,
                bit_perfect: true
            }
        ));
        assert!(!etage.quarantaine_24_bits_ouverte());
        assert_eq!(capture.trames(), 40);
        assert!(etage.vider(&mut capture).is_none());
    }

    /// À l'EOF, une quarantaine jamais fermée part brute et à l'unité : le
    /// volume n'est PAS appliqué, les octets sont ceux de la source.
    #[test]
    fn le_reliquat_24_bits_part_brut_et_a_l_unite() {
        let dsp = Dsp::au_repos();
        dsp.volume.store(500, Ordering::SeqCst);
        let spec = stereo(ProfondeurPcm::Entier24);
        let mut capture = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
        let mut etage = dsp.etage(spec);
        let source = octets(6 * 10 + 2);
        assert_eq!(
            etage.decoder_et_pousser(&source, &mut capture),
            EcritureNative::RienAPousser
        );
        assert_eq!(
            etage.vider(&mut capture),
            Some(ReliquatForceBrut {
                octets: 60,
                trames: 10
            })
        );
        assert_eq!(etage.en_attente(), &source[60..]);
        let attendu: Vec<u8> = pcm_bytes_to_native_i32(&source[..60], 24)
            .iter()
            .flat_map(|mot| mot.to_le_bytes())
            .collect();
        let mut temoin = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
        assert!(temoin.ecrire(spec_du_puits_natif(spec).bloc(&attendu)));
        assert_eq!(capture.empreinte(), temoin.empreinte());
    }

    /// Le volume vit dans l'étage (D3) : à 50 %, les mots ne sont plus ceux
    /// de la source et le verdict le dit.
    #[test]
    fn le_volume_s_applique_dans_l_etage_et_le_verdict_le_dit() {
        let dsp = Dsp::au_repos();
        dsp.volume.store(500, Ordering::SeqCst);
        let spec = stereo(ProfondeurPcm::Entier16);
        let source = octets(4 * 64);
        let mut capture = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
        let mut etage = dsp.etage(spec);
        assert!(matches!(
            etage.decoder_et_pousser(&source, &mut capture),
            EcritureNative::Poussee {
                bit_perfect: false,
                dop: false,
                ..
            }
        ));
        let bruts: Vec<u8> = pcm_bytes_to_native_i32(&source, 16)
            .iter()
            .flat_map(|mot| mot.to_le_bytes())
            .collect();
        let mut temoin = CaptureOutputNatif::ouvrir(spec_du_puits_natif(spec));
        assert!(temoin.ecrire(spec_du_puits_natif(spec).bloc(&bruts)));
        assert_ne!(capture.empreinte(), temoin.empreinte());
        assert!(!etage.transformations().dsp_actif());
        assert!(!etage.transformations().reechantillonnage());
    }

    /// Un puits mort est dit tel : les trames sont rendues quand même.
    #[test]
    fn un_puits_mort_est_dit_tel_avec_ses_trames() {
        struct Mort;
        impl PuitsNatif for Mort {
            fn ecrire(&mut self, _: BlocPcm<'_>) -> bool {
                false
            }
        }
        let dsp = Dsp::au_repos();
        let spec = stereo(ProfondeurPcm::Entier16);
        let mut etage = dsp.etage(spec);
        assert_eq!(
            etage.decoder_et_pousser(&octets(4 * 8), &mut Mort),
            EcritureNative::PuitsMort {
                trames_source: 8,
                dop: false,
                bit_perfect: true
            }
        );
        assert!(etage.rendre_la_queue(&mut Mort));
    }

    /// Le puits d'anneau : les mots du bloc arrivent dans l'anneau dans
    /// l'ordre, un bloc d'un autre format le tue, un arrêt demandé rend
    /// `true`.
    #[test]
    fn le_puits_d_anneau_pousse_les_mots_et_meurt_sur_un_autre_format() {
        let spec = spec_du_puits_natif(stereo(ProfondeurPcm::Entier24));
        let anneau = Arc::new(NativePcmRing::new(64));
        let (_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let paused = AtomicBool::new(false);
        let force_silent = AtomicBool::new(false);
        let mut puits =
            PuitsAnneauNatif::sur(anneau.clone(), spec, &stop_rx, &paused, &force_silent);
        let mots: Vec<i32> = (0..16).map(|i| (i - 8) << 8).collect();
        let bloc: Vec<u8> = mots.iter().flat_map(|m| m.to_le_bytes()).collect();
        assert!(puits.ecrire(spec.bloc(&bloc)));
        let mut lus = vec![0i32; 16];
        assert_eq!(anneau.pop(&mut lus), 16);
        assert_eq!(lus, mots);

        let autre = stereo(ProfondeurPcm::Entier24);
        assert!(!puits.ecrire(autre.bloc(&bloc)));
        assert!(!puits.ecrire(spec.bloc(&bloc)), "un puits mort le reste");
    }

    /// #6247 — la règle de Didier (Marantz AV 7706, WASAPI exclusif) : une
    /// grille 8 → 8 qui ne garde que FL et FR, sur une piste 7.1. Le bras
    /// WASAPI ne branchait pas la réaffectation : les huit voies sortaient
    /// telles quelles. Branchée, BL…SR sont nuls, FL/FR intacts, le verdict
    /// n'est plus bit-perfect et les transformations le disent. Sans règle,
    /// rien ne bouge : le rendu reste celui de la source, au bit près.
    #[test]
    fn la_reaffectation_s_applique_sur_le_transport_natif_6247() {
        use crate::audio::reaffectation_canaux::{ChannelRemapSettings, Matrice};
        let dsp = Dsp::au_repos();
        let spec = AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier16, 8).unwrap();
        let mut gains = vec![vec![None; 8]; 8];
        gains[0][0] = Some(0.0);
        gains[1][1] = Some(0.0);
        let reglage: ChannelRemapSettings = serde_json::from_value(serde_json::json!({
            "enabled": true, "inputs": 8, "outputs": 8, "gains_db": gains, "normalize": false
        }))
        .unwrap();
        let creneau = CreneauReaffectation::vide();
        creneau.poser(Matrice::du_reglage_arme(&reglage).map(Arc::new));
        assert!(creneau.posee(), "la règle 8 → 8 est armée");

        // 64 trames 16 bits, chaque voie à une valeur non nulle distincte.
        let source: Vec<u8> = (0..64)
            .flat_map(|t| (0..8).map(move |c| (1000 * (c + 1) + t) as i16))
            .flat_map(|v| v.to_le_bytes())
            .collect();
        struct Mots(Vec<i32>);
        impl PuitsNatif for Mots {
            fn ecrire(&mut self, bloc: BlocPcm<'_>) -> bool {
                self.0.extend(mots_natifs_du_bloc(bloc));
                true
            }
        }
        let mut capture = Mots(Vec::new());
        let mut etage = dsp
            .etage(spec)
            .avec_l_adaptation_des_canaux(&creneau, &crate::outputs::local::SANS_DISPOSITION);
        assert!(matches!(
            etage.decoder_et_pousser(&source, &mut capture),
            EcritureNative::Poussee {
                trames_source: 64,
                dop: false,
                bit_perfect: false
            }
        ));
        assert!(creneau.appliquee(), "le bloc est passé par la matrice");
        assert!(
            etage.transformations().dsp_actif(),
            "la matrice est un traitement du signal"
        );
        let bruts = pcm_bytes_to_native_i32(&source, 16);
        let mots = capture.0;
        assert_eq!(mots.len(), bruts.len());
        for (t, (trame, attendue)) in mots.chunks(8).zip(bruts.chunks(8)).enumerate() {
            assert_eq!(&trame[..2], &attendue[..2], "trame {t} : FL/FR intacts");
            assert_eq!(
                &trame[2..],
                &[0; 6],
                "trame {t} : FC, LFE, BL, BR, SL, SR doivent être muets — la \
                 réaffectation n'a pas été appliquée par le transport natif"
            );
        }

        // Sans règle posée, le transport natif reste bit-perfect.
        creneau.poser(None);
        let mut temoin = Mots(Vec::new());
        let mut etage = dsp
            .etage(spec)
            .avec_l_adaptation_des_canaux(&creneau, &crate::outputs::local::SANS_DISPOSITION);
        assert!(matches!(
            etage.decoder_et_pousser(&source, &mut temoin),
            EcritureNative::Poussee {
                bit_perfect: true,
                ..
            }
        ));
        assert!(!creneau.appliquee());
        assert_eq!(temoin.0, bruts, "sans règle, les mots de la source");
    }

    /// #6057 — le transport natif (WASAPI, ASIO) réordonne lui aussi un DFF
    /// 5.1 hors ordre à nombre de voies égal : ses transformations doivent le
    /// déclarer, comme l'étage du chemin partagé. Sans disposition, rien.
    #[test]
    fn une_reorganisation_a_nombre_egal_est_declaree_par_l_etage_natif_6057() {
        let dsp = Dsp::au_repos();
        let spec = AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier16, 6).unwrap();
        let disposition: crate::outputs::local::CreneauDisposition = std::sync::Mutex::new(
            crate::audio::disposition_canaux::depuis_dff(&[
                *b"C   ", *b"LFE ", *b"SLFT", *b"SRGT", *b"LS  ", *b"RS  ",
            ])
            .map(Arc::new),
        );
        let reaffectation = CreneauReaffectation::vide();
        let source: Vec<u8> = (0..32)
            .flat_map(|_| (1..=6).map(|c| (1000 * c) as i16))
            .flat_map(|v| v.to_le_bytes())
            .collect();
        struct Mots(Vec<i32>);
        impl PuitsNatif for Mots {
            fn ecrire(&mut self, bloc: BlocPcm<'_>) -> bool {
                self.0.extend(mots_natifs_du_bloc(bloc));
                true
            }
        }
        let mut capture = Mots(Vec::new());
        let mut etage = dsp
            .etage(spec)
            .avec_l_adaptation_des_canaux(&reaffectation, &disposition);
        etage.decoder_et_pousser(&source, &mut capture);
        let attendue: Vec<i32> = [3000, 4000, 1000, 2000, 5000, 6000]
            .iter()
            .map(|v| v << 16)
            .collect();
        assert_eq!(
            &capture.0[..6],
            attendue.as_slice(),
            "prémisse : le transport natif réordonne les voies du DFF"
        );
        assert!(
            etage.transformations().reorganisation_canaux(),
            "voies réordonnées à nombre égal : l'étage natif doit le déclarer (#6057)"
        );

        let etage = dsp
            .etage(spec)
            .avec_l_adaptation_des_canaux(&reaffectation, &crate::outputs::local::SANS_DISPOSITION);
        assert!(!etage.transformations().reorganisation_canaux());
    }

    /// Un arrêt demandé pendant que l'anneau est plein rend `true` — ce
    /// n'est pas une panne (contrat de `PuitsNatif::ecrire`).
    #[test]
    fn un_arret_demande_n_est_pas_une_panne() {
        let spec = spec_du_puits_natif(stereo(ProfondeurPcm::Entier16));
        let anneau = Arc::new(NativePcmRing::new(4));
        let (tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let paused = AtomicBool::new(false);
        let force_silent = AtomicBool::new(false);
        let mut puits = PuitsAnneauNatif::sur(anneau, spec, &stop_rx, &paused, &force_silent);
        let bloc: Vec<u8> = (0..8i32).flat_map(|m| m.to_le_bytes()).collect();
        tx.send(()).unwrap();
        assert!(puits.ecrire(spec.bloc(&bloc)));
    }
}
