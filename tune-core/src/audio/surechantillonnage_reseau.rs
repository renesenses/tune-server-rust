//! Discussion #8519 (suite de #8149) — le suréchantillonneur (#7614) sur une
//! zone RÉSEAU, DLNA ou OpenHome.
//!
//! La sortie locale fait tourner l'étage dans son producteur. Une zone réseau
//! n'a pas de producteur : Tune lui sert un fichier (FLAC ré-encodé, ou WAV
//! pour un renderer sans FLAC). L'étage entre donc dans le bras FICHIER du
//! transcodage, celui qui cuit déjà l'égaliseur sur une zone réseau :
//!
//! 1. la piste est décodée à la cadence de la SOURCE (pas de rubato) ;
//! 2. l'étage du greffon la porte à la cadence cible, AVANT le ReplayGain,
//!    l'égaliseur, la correction de pièce et le crossfeed, qui sont bâtis à
//!    la cadence cible ;
//! 3. l'encodeur écrit le fichier à la cadence cible, en 24 bits.
//!
//! Ce module tient la partie PURE de la décision : quelles cadences le
//! renderer a déclarées (`GetProtocolInfo` → `Sink`), lesquelles sont donc
//! acceptées, et pourquoi l'étage n'entre pas dans le flux quand il n'y entre
//! pas. Jamais sous PURE (le choix de la zone n'existe pas), jamais en
//! bit-perfect strict.
use super::surechantillonnage::{CADENCES_CANDIDATES, ChoixSurechantillonnage, UpsamplerSettings};

/// Les types de sortie réseau où l'étage peut entrer : ceux dont Tune sert
/// lui-même le fichier au renderer et lit le `Sink`. AirPlay impose sa
/// cadence de transport, Chromecast plafonne à 96 kHz, les autres n'ont pas
/// de `Sink` à lire.
pub fn sortie_prise_en_charge(output_type: &str) -> bool {
    matches!(output_type, "dlna" | "openhome")
}

/// Ce que l'étage fait au flux réseau : de la cadence de la source à la
/// cadence négociée, avec les réglages de la zone (filtre).
#[derive(Debug, Clone, PartialEq)]
pub struct CibleReseau {
    pub de: u32,
    pub vers: u32,
    pub reglages: UpsamplerSettings,
}

impl CibleReseau {
    /// La réserve de niveau de l'étage, en dB (≤ 0), pour le chemin du
    /// signal. Même calcul que l'étage lui-même (`Instance::gain`).
    pub fn reserve_db(&self) -> f64 {
        if self.de == 0 || !self.vers.is_multiple_of(self.de) {
            return 0.0;
        }
        super::surechantillonnage::greffon::headroom_db(&self.reglages.filter, self.vers / self.de)
            .unwrap_or(0.0)
    }
}

/// Pourquoi l'étage n'entre pas dans le flux d'une zone réseau. Les codes
/// sont stables : journal (`dlna_upsampler_not_applied reason=…`) et chemin
/// du signal (`upsampling.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusReseau {
    /// AirPlay, Chromecast, BluOS… : pas de fichier servi par Tune à un Sink.
    SortieNonPriseEnCharge,
    /// DSD (natif, DoP ou décimé) ou format inconnu : l'étage ne prend que
    /// du PCM.
    SourceNonPcm,
    /// Le « bit-perfect strict » de la zone refuse toute conversion.
    BitPerfectStrict,
    /// La piste part en WAV PROGRESSIF (opt-in `dsp_progressif_reseau`,
    /// `.ape`/WavPack) : l'étage n'est branché que sur le bras fichier.
    FluxProgressif,
    /// Le renderer n'a déclaré aucune cadence (Sink illisible ou sans
    /// `rate=`), et la zone n'a pas de plafond réglé.
    CadencesDuRendererInconnues,
    /// Aucune cadence acceptée n'est un multiple entier de la source au-dessus
    /// d'elle (renderer limité à 48 kHz, famille 48 kHz seule pour du 44,1…).
    PlafondDuRenderer,
    /// Le test de la machine n'admet le filtre à aucune des cadences
    /// acceptées.
    MachineTropLente,
    /// Les plafonds des réglages (cadence, rapport) écartent les cadences
    /// admises.
    PlafondsDesReglages,
    /// La piste, à la cadence cible, ne tient pas dans le bras fichier
    /// (mémoire, ou en-tête RIFF d'un WAV au-delà de 4 Gio).
    PisteTropLourde,
}

impl RefusReseau {
    pub fn code(self) -> &'static str {
        match self {
            Self::SortieNonPriseEnCharge => "output_not_supported",
            Self::SourceNonPcm => "source_not_pcm",
            Self::BitPerfectStrict => "strict_bitperfect",
            Self::FluxProgressif => "progressive_stream",
            Self::CadencesDuRendererInconnues => "renderer_rates_unknown",
            Self::PlafondDuRenderer => "renderer_rate_ceiling",
            Self::MachineTropLente => "machine_test_rejects_accepted_rates",
            Self::PlafondsDesReglages => "settings_caps_exclude_admitted_rates",
            Self::PisteTropLourde => "track_too_heavy",
        }
    }

    /// Le refus inverse de [`Self::code`], pour relire ce que le flux publie.
    pub fn depuis_code(code: &str) -> Option<Self> {
        [
            Self::SortieNonPriseEnCharge,
            Self::SourceNonPcm,
            Self::BitPerfectStrict,
            Self::FluxProgressif,
            Self::CadencesDuRendererInconnues,
            Self::PlafondDuRenderer,
            Self::MachineTropLente,
            Self::PlafondsDesReglages,
            Self::PisteTropLourde,
        ]
        .into_iter()
        .find(|r| r.code() == code)
    }
}

/// Le MIME d'une entrée de `Sink`, sans paramètres, en minuscules, `x-`
/// retiré (`audio/x-flac` ≡ `audio/flac`).
fn mime_de_base(champ: &str) -> String {
    let base = champ
        .split(';')
        .next()
        .unwrap_or(champ)
        .trim()
        .to_ascii_lowercase();
    base.replace("audio/x-", "audio/")
}

/// La famille de MIME qui porte le fichier servi : le FLAC, ou le WAV et ses
/// profils LPCM.
fn famille(mime: &str) -> &'static [&'static str] {
    match mime_de_base(mime).as_str() {
        "audio/wav" | "audio/wave" | "audio/l16" | "audio/l24" => {
            &["audio/wav", "audio/wave", "audio/l16", "audio/l24"]
        }
        _ => &["audio/flac"],
    }
}

/// Les `rate=` d'une entrée (`http-get:*:audio/L24;rate=192000;channels=2:*`).
fn cadences_de_l_entree(entree: &str) -> Option<(String, Vec<u32>)> {
    let champs: Vec<&str> = entree.splitn(4, ':').collect();
    if champs.len() < 3 || !champs[0].trim().eq_ignore_ascii_case("http-get") {
        return None;
    }
    let mime = mime_de_base(champs[2]);
    let cadences = champs[2]
        .split(';')
        .skip(1)
        .filter_map(|p| {
            let (nom, valeur) = p.trim().split_once('=')?;
            if !nom.trim().eq_ignore_ascii_case("rate") {
                return None;
            }
            valeur.trim().parse::<u32>().ok().filter(|r| *r > 0)
        })
        .collect();
    Some((mime, cadences))
}

/// Les cadences que le `Sink` DÉCLARE pour le format servi (`mime`), triées,
/// sans doublon. Vide : le renderer ne dit rien de ses cadences pour ce
/// format (le cas courant du FLAC).
pub fn cadences_declarees(sink: &[String], mime: &str) -> Vec<u32> {
    let famille = famille(mime);
    let mut toutes: Vec<u32> = sink
        .iter()
        .filter_map(|e| cadences_de_l_entree(e))
        .filter(|(m, _)| famille.contains(&m.as_str()))
        .flat_map(|(_, c)| c)
        .collect();
    toutes.sort_unstable();
    toutes.dedup();
    toutes
}

/// Les cadences que le `Sink` déclare pour N'IMPORTE QUEL format PCM sans
/// perte (FLAC, WAV, LPCM, AIFF) : ce que le convertisseur du renderer
/// accepte, quand le format servi ne dit rien.
pub fn cadences_declarees_pcm(sink: &[String]) -> Vec<u32> {
    const PCM: [&str; 7] = [
        "audio/flac",
        "audio/wav",
        "audio/wave",
        "audio/l16",
        "audio/l24",
        "audio/aiff",
        "audio/aif",
    ];
    let mut toutes: Vec<u32> = sink
        .iter()
        .filter_map(|e| cadences_de_l_entree(e))
        .filter(|(m, _)| PCM.contains(&m.as_str()))
        .flat_map(|(_, c)| c)
        .collect();
    toutes.sort_unstable();
    toutes.dedup();
    toutes
}

/// Les cadences du suréchantillonneur que la zone ACCEPTE.
///
/// - `declarees` : ce que le renderer a déclaré (voir
///   [`cadences_declarees`]) ;
/// - `plafond_zone` : le plafond réglé sur la zone ou tiré du catalogue
///   (`max_sample_rate`), une déclaration explicite de ce que le renderer
///   accepte.
///
/// Règle (#8149, « Attendu » 2) : ne viser qu'une cadence DÉCLARÉE ; si le
/// renderer en déclare mais aucune parmi les cadences du suréchantillonneur,
/// toute cadence sous la plus haute déclarée (« la plus haute acceptée ») ;
/// sans déclaration, sous le plafond de la zone ; sans rien, refus. Le
/// plafond de la zone borne toujours.
pub fn cadences_acceptees(
    declarees: &[u32],
    plafond_zone: Option<u32>,
) -> Result<Vec<u32>, RefusReseau> {
    let plafond_zone = plafond_zone.filter(|p| *p > 0);
    let plafond = match (declarees.iter().copied().max(), plafond_zone) {
        (Some(d), Some(z)) => d.min(z),
        (Some(d), None) => d,
        (None, Some(z)) => z,
        (None, None) => return Err(RefusReseau::CadencesDuRendererInconnues),
    };
    let sous_le_plafond: Vec<u32> = CADENCES_CANDIDATES
        .iter()
        .copied()
        .filter(|c| *c <= plafond)
        .collect();
    let exactes: Vec<u32> = sous_le_plafond
        .iter()
        .copied()
        .filter(|c| declarees.contains(c))
        .collect();
    Ok(if exactes.is_empty() {
        sous_le_plafond
    } else {
        exactes
    })
}

/// La cadence cible pour une source à `source`, parmi `acceptees`, avec la
/// raison précise quand il n'y en a pas. Même règle que la sortie locale
/// ([`ChoixSurechantillonnage::negocier`]), avec le refus nommé.
pub fn negocier(
    choix: &ChoixSurechantillonnage,
    source: u32,
    acceptees: &[u32],
) -> Result<u32, RefusReseau> {
    if source == 0 {
        return Err(RefusReseau::SourceNonPcm);
    }
    let multiples: Vec<u32> = acceptees
        .iter()
        .copied()
        .filter(|r| *r > source && r.is_multiple_of(source))
        .collect();
    if multiples.is_empty() {
        return Err(RefusReseau::PlafondDuRenderer);
    }
    if !multiples
        .iter()
        .any(|r| choix.verdict.admet(&choix.reglages.filter, *r))
    {
        return Err(RefusReseau::MachineTropLente);
    }
    choix
        .negocier(source, &multiples)
        .ok_or(RefusReseau::PlafondsDesReglages)
}

/// L'étage appliqué à une piste ENTIÈRE de PCM entier (`bit_depth` 16, 24
/// ou 32 bits, petit-boutiste, cadré à droite) : rend du PCM 32 bits à la
/// cadence de sortie de l'étage, queue du noyau comprise. Découpé en blocs :
/// seul un bloc existe en flottant à la fois.
pub fn surechantillonner_pcm(
    etage: &mut super::surechantillonnage::EtageSurechantillonneur,
    pcm: &[u8],
    bit_depth: u16,
) -> Result<Vec<u8>, String> {
    let octets = match bit_depth {
        16 => 2usize,
        24 => 3,
        32 => 4,
        autre => return Err(format!("upsampler: unsupported bit depth {autre}")),
    };
    let canaux = usize::from(etage.canaux()).max(1);
    let rapport = (etage.cadence_sortie() / etage.cadence_entree().max(1)) as usize;
    let echelle = 1.0 / (1u64 << (bit_depth - 1)) as f32;
    let trame = octets * canaux;
    let mut sortie = Vec::with_capacity(pcm.len() / octets * rapport.max(1) * 4 + 4096);
    let ecrire = |sortie: &mut Vec<u8>, mots: &[f32]| {
        for &x in mots {
            let v = (f64::from(x) * 2_147_483_648.0).clamp(-2_147_483_648.0, 2_147_483_647.0);
            sortie.extend_from_slice(&(v as i32).to_le_bytes());
        }
    };
    let mut flottants: Vec<f32> = Vec::with_capacity(8192 * canaux);
    for morceau in pcm.chunks(trame * 8192) {
        let entier = morceau.len() - morceau.len() % trame;
        flottants.clear();
        for o in morceau[..entier].chunks_exact(octets) {
            let v = match octets {
                2 => i32::from(i16::from_le_bytes([o[0], o[1]])),
                3 => {
                    ((i32::from(o[0]) | (i32::from(o[1]) << 8) | (i32::from(o[2]) << 16)) << 8) >> 8
                }
                _ => i32::from_le_bytes([o[0], o[1], o[2], o[3]]),
            };
            flottants.push(v as f32 * echelle);
        }
        let rendu = etage.traiter(&flottants);
        ecrire(&mut sortie, &rendu);
    }
    let queue = etage.vider();
    ecrire(&mut sortie, &queue);
    Ok(sortie)
}

#[cfg(test)]
#[path = "surechantillonnage_reseau_tests.rs"]
mod tests;
