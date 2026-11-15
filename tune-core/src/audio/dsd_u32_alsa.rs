//! #8248 — le DSD natif d'une sortie locale Linux : ALSA `DSD_U32_BE` /
//! `DSD_U32_LE`.
//!
//! Le pilote USB Audio de Linux expose le DSD natif d'un DAC (quirk du noyau,
//! selon l'identifiant USB) sous un format PCM « DSD » : `DSD_U32_BE` le plus
//! souvent (Sabaj D5, fil 2730 : `Altset 3 … DSD_U32_BE, DSD raw: DOP=0,
//! bitrev=0`), parfois `DSD_U32_LE`. Une trame ALSA y porte, PAR CANAL, un mot
//! de 32 bits : **32 échantillons DSD consécutifs de ce canal**, le premier
//! dans le bit de poids fort (MSB-first). La cadence ALSA est donc la cadence
//! DSD divisée par 32 : DSD64 (2 822 400 Hz) s'ouvre à 88 200 Hz, DSD512 de la
//! famille 48 kHz (24 576 000 Hz) à 768 000 Hz — ce que MPD ouvre sur le même
//! DAC (mesures d'Erik, fil 2730).
//!
//! Ce module part du flux que sert l'orchestrateur (`audio::dsd_brut`) :
//! des octets `DsdU8` — **un octet par canal et par trame** (L, R, L, R…),
//! premier bit dans le bit de poids fort, DSF déjà remis en MSB-first. Il en
//! fait des mots `DSD_U32` :
//!
//! - quatre octets SUCCESSIFS d'un même canal forment un mot, le plus ancien
//!   dans l'octet de poids fort ;
//! - `DSD_U32_BE` range ce mot en gros-boutiste : en mémoire, les quatre
//!   octets restent dans l'ordre du temps ; `DSD_U32_LE` en petit-boutiste :
//!   l'octet le plus ancien est le DERNIER des quatre en mémoire.
//!
//! Aucun bit n'est retourné ici (`bitrev=0`) : le flux `DsdU8` est déjà
//! MSB-first. Un mauvais ordre produirait du bruit à pleine échelle : c'est
//! pourquoi l'empaquetage est éprouvé au bit près par ses tests, depuis un
//! vrai fichier DSF.
//!
//! Le motif de repos DSD (0x69) est symétrique octet par octet : il vaut pour
//! les deux ordres sans conversion.
//!
//! Rien ici ne dépend de la plateforme : tout se teste partout. Le sondage
//! du pilote, lui, vit dans `outputs/local/bras_alsa_dsd.rs` (Linux).

/// Octets d'un mot `DSD_U32` (32 échantillons d'un canal).
pub const OCTETS_PAR_MOT: usize = 4;

/// Les cadences DSD (Hz, bits par seconde et par canal) que la sortie ALSA
/// native sait ouvrir, de DSD64 à DSD1024, familles 44,1 et 48 kHz. Une
/// cadence n'est retenue pour un périphérique que si son pilote accepte
/// EXACTEMENT `cadence / 32` en `DSD_U32` ([`cadences_offertes`]).
pub const CADENCES_DSD: [u32; 10] = [
    2_822_400,  // DSD64
    3_072_000,  // DSD64 (48 kHz)
    5_644_800,  // DSD128
    6_144_000,  // DSD128 (48 kHz)
    11_289_600, // DSD256
    12_288_000, // DSD256 (48 kHz)
    22_579_200, // DSD512
    24_576_000, // DSD512 (48 kHz)
    45_158_400, // DSD1024
    49_152_000, // DSD1024 (48 kHz)
];

/// La cadence de trame ALSA d'un DSD à `cadence_dsd` en `DSD_U32`.
#[must_use]
pub fn cadence_de_trame(cadence_dsd: u32) -> u32 {
    cadence_dsd / 32
}

/// L'ordre des octets d'un mot `DSD_U32` en mémoire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdreDsdU32 {
    /// `DSD_U32_BE` : l'octet le plus ancien en premier.
    GrosBoutiste,
    /// `DSD_U32_LE` : l'octet le plus ancien en dernier.
    PetitBoutiste,
}

impl OrdreDsdU32 {
    /// Le nom du format ALSA, tel que `/proc/asound/card*/stream0` l'écrit.
    #[must_use]
    pub fn nom_alsa(self) -> &'static str {
        match self {
            Self::GrosBoutiste => "DSD_U32_BE",
            Self::PetitBoutiste => "DSD_U32_LE",
        }
    }
}

/// L'ordre que la sortie ouvrira, d'après les deux formats que le pilote
/// offre. `None` : aucun des deux, pas de DSD natif.
///
/// Miroir EXACT de `vendor/cpal/src/host/alsa/mod.rs`,
/// `sample_format_to_alsa_format` : cpal ouvre `SampleFormat::DsdU32` dans
/// l'ordre NATIF du processeur s'il est offert, l'autre sinon, et ne dit pas
/// lequel il a pris. Les octets ne sont pas convertis par cpal : ils partent
/// tels quels, il faut donc les ranger dans l'ordre qu'il a choisi.
#[must_use]
pub fn ordre_retenu(petit_offert: bool, gros_offert: bool) -> Option<OrdreDsdU32> {
    let (natif, natif_offert, autre, autre_offert) = if cfg!(target_endian = "little") {
        (
            OrdreDsdU32::PetitBoutiste,
            petit_offert,
            OrdreDsdU32::GrosBoutiste,
            gros_offert,
        )
    } else {
        (
            OrdreDsdU32::GrosBoutiste,
            gros_offert,
            OrdreDsdU32::PetitBoutiste,
            petit_offert,
        )
    };
    if natif_offert {
        Some(natif)
    } else if autre_offert {
        Some(autre)
    } else {
        None
    }
}

/// Les cadences DSD de [`CADENCES_DSD`] dont la cadence de trame est
/// acceptée par le pilote (`accepte(cadence_de_trame)`, `test_rate` d'ALSA
/// sur le format `DSD_U32` déjà fixé).
#[must_use]
pub fn cadences_offertes(mut accepte: impl FnMut(u32) -> bool) -> Vec<u32> {
    CADENCES_DSD
        .iter()
        .copied()
        .filter(|c| accepte(cadence_de_trame(*c)))
        .collect()
}

/// Ce qu'un pilote ALSA `hw:` offre en DSD natif.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapaciteDsdU32 {
    pub ordre: OrdreDsdU32,
    /// Les cadences DSD acceptées (vide = format offert mais aucune cadence
    /// de la table : pas de DSD natif).
    pub cadences: Vec<u32>,
    /// Le nombre de voies sondé.
    pub voies: u16,
}

/// Range en mots `DSD_U32` les trames `DsdU8` entières de `entree`, par
/// groupes de quatre trames (un mot par canal), et les ajoute à `sortie`.
///
/// Rend le nombre d'octets de `entree` consommés : un multiple de
/// `4 × canaux`. Le reste (moins de quatre trames) attend la lecture
/// suivante — voir [`Empaqueteur`].
pub fn empaqueter(entree: &[u8], canaux: usize, ordre: OrdreDsdU32, sortie: &mut Vec<u8>) -> usize {
    let canaux = canaux.max(1);
    let groupe = OCTETS_PAR_MOT * canaux;
    let utiles = entree.len() - entree.len() % groupe;
    sortie.reserve(utiles);
    for quatre_trames in entree[..utiles].chunks_exact(groupe) {
        for canal in 0..canaux {
            let mot = [
                quatre_trames[canal],
                quatre_trames[canaux + canal],
                quatre_trames[2 * canaux + canal],
                quatre_trames[3 * canaux + canal],
            ];
            match ordre {
                OrdreDsdU32::GrosBoutiste => sortie.extend_from_slice(&mot),
                OrdreDsdU32::PetitBoutiste => {
                    sortie.extend_from_slice(&[mot[3], mot[2], mot[1], mot[0]]);
                }
            }
        }
    }
    utiles
}

/// L'empaquetage d'un flux lu par morceaux : garde la queue de moins de
/// quatre trames d'une lecture à l'autre, pour que les mots restent alignés
/// sur le temps et sur les canaux quelle que soit la taille des lectures.
#[derive(Debug)]
pub struct Empaqueteur {
    canaux: usize,
    ordre: OrdreDsdU32,
    reste: Vec<u8>,
}

impl Empaqueteur {
    #[must_use]
    pub fn new(canaux: usize, ordre: OrdreDsdU32) -> Self {
        Self {
            canaux: canaux.max(1),
            ordre,
            reste: Vec::new(),
        }
    }

    /// Ajoute `octets` (`DsdU8` entrelacé) et range dans `sortie` tout ce qui
    /// forme des mots complets.
    pub fn pousser(&mut self, octets: &[u8], sortie: &mut Vec<u8>) {
        self.reste.extend_from_slice(octets);
        let pris = empaqueter(&self.reste, self.canaux, self.ordre, sortie);
        self.reste.drain(..pris);
    }

    /// Les octets en attente (moins de quatre trames) : la fin du flux les
    /// abandonne — moins de 32 échantillons, onze microsecondes en DSD64.
    #[must_use]
    pub fn en_attente(&self) -> usize {
        self.reste.len()
    }

    /// Octets d'une trame ALSA `DSD_U32` (un mot par canal).
    #[must_use]
    pub fn octets_par_trame(&self) -> usize {
        OCTETS_PAR_MOT * self.canaux
    }
}
