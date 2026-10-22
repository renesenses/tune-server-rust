//! #6353 (étape 1) — reconnaître, SANS LES DÉCODER, les flux Dolby d'une piste
//! audio Matroska : AC-3, E-AC-3 (Dolby Digital Plus) et TrueHD.
//!
//! Demande de Didier (forum, fil 2202) : envoyer ces flux tels quels à un
//! amplificateur qui les décode, comme Kodi ou VLC (« passthrough »). Tune ne
//! décode pas ces codecs et ne le fera pas (#3633, point 3) ; il n'en a pas
//! besoin : il faut les LIRE assez pour savoir ce qu'ils portent, puis les
//! emballer en IEC 61937 vers une sortie numérique (HDMI, S/PDIF).
//!
//! # Ce que cette étape livre
//!
//! - [`analyser_ac3`] et [`analyser_truehd`] : les en-têtes de trame, lus bit
//!   à bit, d'où le codec, la fréquence, les canaux et, pour TrueHD, le
//!   nombre de sous-flux (quatre : la présentation 16 canaux d'Atmos) ;
//! - [`analyser_les_paquets`] : le verdict sur une suite de paquets bruts,
//!   tels que le démuxeur Matroska les rend ;
//! - [`porteuse_iec61937`] : ce que le transport demanderait à la sortie
//!   (canaux et fréquence de la porteuse PCM) ;
//! - [`rafale_iec61937`] : l'emballage d'une trame AC-3 ou E-AC-3 en rafale
//!   IEC 61937, prêt à partir comme du PCM 16 bits. TrueHD exige en plus le
//!   regroupement MAT (24 unités par rafale) : étape suivante ;
//! - [`chemin_du_signal`] : les étapes source → démuxage → transport, qui
//!   DISENT que le transport n'est pas encore pris en charge.
//!
//! Aucune sortie n'émet encore ces rafales : c'est l'étape 2 (sortie locale
//! ALSA/WASAPI en mode non-audio, volume et DSP court-circuités comme pour le
//! DoP).

/// Le codec d'un flux compressé reconnu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecDolby {
    Ac3,
    Eac3,
    TrueHd,
}

impl CodecDolby {
    pub fn nom(self) -> &'static str {
        match self {
            CodecDolby::Ac3 => "Dolby Digital (AC-3)",
            CodecDolby::Eac3 => "Dolby Digital Plus (E-AC-3)",
            CodecDolby::TrueHd => "Dolby TrueHD",
        }
    }

    /// Le `Pc` (type de données) IEC 61937-2 : 1 AC-3, 21 E-AC-3, 22 MAT.
    pub fn type_iec61937(self) -> u16 {
        match self {
            CodecDolby::Ac3 => 0x01,
            CodecDolby::Eac3 => 0x15,
            CodecDolby::TrueHd => 0x16,
        }
    }
}

/// Ce que l'en-tête d'UNE trame dit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnteteDolby {
    pub codec: CodecDolby,
    pub frequence: u32,
    /// Canaux pleine bande, LFE NON compris.
    pub canaux: u8,
    pub lfe: bool,
    /// Taille de la trame en octets, quand l'en-tête la donne.
    pub taille: Option<usize>,
    /// E-AC-3 : 0 indépendant, 1 dépendant. 0 ailleurs.
    pub type_de_sous_flux: u8,
    /// E-AC-3 : les emplacements ajoutés par un sous-flux dépendant
    /// (`chanmap`). TrueHD : le nombre de sous-flux.
    pub complement: u32,
    /// AC-3 : `bsmod`, recopié dans le `Pc` de la rafale.
    pub bsmod: u8,
}

/// Ce qu'on sait d'un flux après en avoir lu quelques paquets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FluxDolby {
    pub codec: CodecDolby,
    pub frequence: u32,
    /// Canaux au total, LFE compris (8 pour du 7.1).
    pub canaux: u8,
    pub lfe: bool,
    /// TrueHD seulement : `Some(true)` quand le flux porte quatre sous-flux,
    /// signe de la présentation 16 canaux d'Atmos. `None` : l'étape 1 ne sait
    /// pas le dire (E-AC-3 : Atmos voyage dans des métadonnées EMDF).
    pub atmos: Option<bool>,
}

impl FluxDolby {
    /// « 7.1 », « 5.1 », « 2.0 »…
    pub fn disposition(&self) -> String {
        let lfe = u8::from(self.lfe);
        format!("{}.{}", self.canaux.saturating_sub(lfe), lfe)
    }

    pub fn decrire(&self) -> String {
        let atmos = match self.atmos {
            Some(true) => ", Atmos (4 sous-flux)",
            _ => "",
        };
        format!(
            "{} {}, {} kHz{atmos}",
            self.codec.nom(),
            self.disposition(),
            khz(self.frequence)
        )
    }
}

fn khz(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        format!("{}", hz / 1000)
    } else {
        format!("{:.1}", hz as f64 / 1000.0)
    }
}

// ── Lecture bit à bit ──────────────────────────────────────────────────────

struct Bits<'a> {
    octets: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(octets: &'a [u8]) -> Self {
        Self { octets, pos: 0 }
    }

    fn lire(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let octet = *self.octets.get(self.pos / 8)?;
            let bit = (octet >> (7 - (self.pos % 8))) & 1;
            v = (v << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(v)
    }

    fn sauter(&mut self, n: u32) -> Option<()> {
        self.lire(n).map(|_| ())
    }
}

// ── AC-3 / E-AC-3 (ATSC A/52, annexe E) ────────────────────────────────────

const SYNC_AC3: u32 = 0x0B77;

/// Canaux pleine bande selon `acmod` (A/52 tableau 5.8).
const CANAUX_ACMOD: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// Les emplacements des canaux selon `acmod`, pour compter ce qu'un
/// sous-flux dépendant AJOUTE.
fn emplacements_acmod(acmod: u32) -> &'static [&'static str] {
    match acmod {
        0 => &["L", "R"], // 1+1 : deux monos
        1 => &["C"],
        2 => &["L", "R"],
        3 => &["L", "C", "R"],
        4 => &["L", "R", "Cs"],
        5 => &["L", "C", "R", "Cs"],
        6 => &["L", "R", "Ls", "Rs"],
        _ => &["L", "C", "R", "Ls", "Rs"],
    }
}

/// Les emplacements de `chanmap` (A/52 annexe E, tableau E.1.4), bit 0 = le
/// plus significatif. Une paire compte pour deux canaux.
const CHANMAP: [&[&str]; 15] = [
    &["L"],
    &["C"],
    &["R"],
    &["Ls"],
    &["Rs"],
    &["Lc", "Rc"],
    &["Lrs", "Rrs"],
    &["Cs"],
    &["Ts"],
    &["Lsd", "Rsd"],
    &["Lw", "Rw"],
    &["Vhl", "Vhr"],
    &["Vhc"],
    &["Lts", "Rts"],
    &["LFE2"],
];

/// Fréquences de `fscod` (AC-3 et E-AC-3), puis de `fscod2` (E-AC-3).
const FSCOD: [u32; 3] = [48_000, 44_100, 32_000];
const FSCOD2: [u32; 3] = [24_000, 22_050, 16_000];

/// Tailles de trame AC-3 en mots de 16 bits, par `frmsizecod / 2` et par
/// `fscod` (A/52 tableau 5.18) ; à 44,1 kHz, un mot de plus si
/// `frmsizecod` est impair.
const TAILLES_AC3: [[u16; 3]; 19] = [
    [64, 69, 96],
    [80, 87, 120],
    [96, 104, 144],
    [112, 121, 168],
    [128, 139, 192],
    [160, 174, 240],
    [192, 208, 288],
    [224, 243, 336],
    [256, 278, 384],
    [320, 348, 480],
    [384, 417, 576],
    [448, 487, 672],
    [512, 557, 768],
    [640, 696, 960],
    [768, 835, 1152],
    [896, 975, 1344],
    [1024, 1114, 1536],
    [1152, 1253, 1728],
    [1280, 1393, 1920],
];

/// L'en-tête d'une trame AC-3 ou E-AC-3, ou `None` si ce n'en est pas une.
pub fn analyser_ac3(trame: &[u8]) -> Option<EnteteDolby> {
    let mut b = Bits::new(trame);
    if b.lire(16)? != SYNC_AC3 {
        return None;
    }
    // `bsid` est au même endroit dans les deux syntaxes : octet 5, 5 bits
    // de poids fort.
    let bsid = u32::from(*trame.get(5)? >> 3);
    if bsid <= 10 {
        b.sauter(16)?; // crc1
        let fscod = b.lire(2)?;
        let frmsizecod = b.lire(6)?;
        let frequence = *FSCOD.get(fscod as usize)?;
        let mots = TAILLES_AC3.get((frmsizecod / 2) as usize)?[fscod as usize];
        let mots = mots + u16::from(fscod == 1 && frmsizecod % 2 == 1);
        b.sauter(5)?; // bsid
        let bsmod = b.lire(3)? as u8;
        let acmod = b.lire(3)?;
        if acmod & 1 == 1 && acmod != 1 {
            b.sauter(2)?; // cmixlev
        }
        if acmod & 4 == 4 {
            b.sauter(2)?; // surmixlev
        }
        if acmod == 2 {
            b.sauter(2)?; // dsurmod
        }
        let lfe = b.lire(1)? == 1;
        return Some(EnteteDolby {
            codec: CodecDolby::Ac3,
            frequence,
            canaux: CANAUX_ACMOD[acmod as usize],
            lfe,
            taille: Some(usize::from(mots) * 2),
            type_de_sous_flux: 0,
            complement: 0,
            bsmod,
        });
    }
    if !(11..=16).contains(&bsid) {
        return None;
    }
    let strmtyp = b.lire(2)?;
    b.sauter(3)?; // substreamid
    let frmsiz = b.lire(11)?;
    let fscod = b.lire(2)?;
    let frequence = if fscod == 3 {
        *FSCOD2.get(b.lire(2)? as usize)?
    } else {
        b.sauter(2)?; // numblkscod
        FSCOD[fscod as usize]
    };
    let acmod = b.lire(3)?;
    let lfe = b.lire(1)? == 1;
    b.sauter(5)?; // bsid
    b.sauter(5)?; // dialnorm
    if b.lire(1)? == 1 {
        b.sauter(8)?; // compr
    }
    if acmod == 0 {
        b.sauter(5)?; // dialnorm2
        if b.lire(1)? == 1 {
            b.sauter(8)?; // compr2
        }
    }
    let mut chanmap = 0;
    if strmtyp == 1 && b.lire(1)? == 1 {
        chanmap = b.lire(16)?;
    }
    Some(EnteteDolby {
        codec: CodecDolby::Eac3,
        frequence,
        canaux: CANAUX_ACMOD[acmod as usize],
        lfe,
        taille: Some((frmsiz as usize + 1) * 2),
        type_de_sous_flux: strmtyp as u8,
        complement: chanmap,
        bsmod: 0,
    })
}

/// Le flux E-AC-3 d'un paquet : le sous-flux indépendant, complété des
/// emplacements qu'ajoutent ses sous-flux dépendants (7.1 = 5.1 + Lrs/Rrs).
fn flux_eac3(paquet: &[u8]) -> Option<FluxDolby> {
    let mut decalage = 0;
    let mut independant: Option<(EnteteDolby, u32)> = None;
    let mut emplacements: Vec<&str> = Vec::new();
    while decalage + 6 <= paquet.len() {
        let Some(e) = analyser_ac3(&paquet[decalage..]) else {
            break;
        };
        if e.codec != CodecDolby::Eac3 || !trame_entiere(&e, paquet.len() - decalage) {
            break;
        }
        let acmod = acmod_eac3(&paquet[decalage..])?;
        if e.type_de_sous_flux == 1 {
            // Un dépendant porte ses canaux par `chanmap` ; sans lui, par
            // `acmod`, et ils REMPLACENT les mêmes emplacements.
            if e.complement != 0 {
                for (i, noms) in CHANMAP.iter().enumerate() {
                    if e.complement & (1 << (15 - i)) != 0 {
                        emplacements.extend_from_slice(noms);
                    }
                }
            } else {
                emplacements.extend_from_slice(emplacements_acmod(acmod));
            }
        } else if independant.is_none() {
            emplacements.extend_from_slice(emplacements_acmod(acmod));
            independant = Some((e.clone(), acmod));
        }
        decalage += e.taille?;
    }
    let (base, _) = independant?;
    emplacements.sort_unstable();
    emplacements.dedup();
    let lfe = base.lfe;
    Some(FluxDolby {
        codec: CodecDolby::Eac3,
        frequence: base.frequence,
        canaux: emplacements.len() as u8 + u8::from(lfe),
        lfe,
        atmos: None,
    })
}

/// `acmod` d'une trame E-AC-3 (relu : [`EnteteDolby`] ne garde que le compte).
fn acmod_eac3(trame: &[u8]) -> Option<u32> {
    let mut b = Bits::new(trame);
    // syncword, strmtyp, substreamid, frmsiz, fscod, puis fscod2 ou
    // numblkscod : toujours 2 bits.
    b.sauter(16 + 2 + 3 + 11 + 2 + 2)?;
    b.lire(3)
}

// ── TrueHD (Dolby TrueHD / MLP) ────────────────────────────────────────────

/// `format_sync` d'une synchronisation majeure TrueHD (`0xF8726FBB` : MLP).
const SYNC_TRUEHD: u32 = 0xF872_6FBA;
/// Signature qui suit `format_info`.
const SIGNATURE_TRUEHD: u32 = 0xB752;

/// Canaux par bit de l'affectation TrueHD (bit 0 = poids faible) :
/// L/R, C, LFE, Ls/Rs, Lvh/Rvh, Lc/Rc, Lrs/Rrs, Cs, Ts, Lsd/Rsd, Lw/Rw, Cvh,
/// LFE2.
const CANAUX_TRUEHD: [u8; 13] = [2, 1, 1, 2, 2, 2, 2, 1, 1, 2, 2, 1, 1];

fn canaux_truehd(affectation: u32) -> (u8, bool) {
    let mut n = 0;
    for (i, c) in CANAUX_TRUEHD.iter().enumerate() {
        if affectation & (1 << i) != 0 {
            n += c;
        }
    }
    (n, affectation & (1 << 2) != 0)
}

fn frequence_mlp(code: u32) -> Option<u32> {
    if code == 0xF {
        return None;
    }
    let base: u32 = if code & 8 != 0 { 44_100 } else { 48_000 };
    Some(base << (code & 7))
}

/// L'en-tête d'une unité d'accès TrueHD qui porte une synchronisation
/// majeure, ou `None` (unité sans synchronisation, ou pas du TrueHD).
pub fn analyser_truehd(unite: &[u8]) -> Option<EnteteDolby> {
    let mut b = Bits::new(unite);
    b.sauter(32)?; // check_nibble, access_unit_length, input_timing
    if b.lire(32)? != SYNC_TRUEHD {
        return None;
    }
    let frequence = frequence_mlp(b.lire(4)?)?;
    b.sauter(4)?; // réservé
    b.sauter(2 + 2)?; // channel_modifier_thd_stream0 / stream1
    let affectation_6 = b.lire(5)?;
    b.sauter(2)?; // channel_modifier_thd_stream2
    let affectation_8 = b.lire(13)?;
    if b.lire(16)? != SIGNATURE_TRUEHD {
        return None;
    }
    b.sauter(16)?; // flags
    b.sauter(16)?; // réservé
    b.sauter(1 + 15)?; // is_vbr, peak_bitrate
    let sous_flux = b.lire(4)?;
    // La présentation la plus large qui soit renseignée : 8 canaux, sinon 6.
    let (canaux, lfe) = if affectation_8 != 0 {
        canaux_truehd(affectation_8)
    } else {
        canaux_truehd(affectation_6)
    };
    Some(EnteteDolby {
        codec: CodecDolby::TrueHd,
        frequence,
        canaux: canaux.saturating_sub(u8::from(lfe)),
        lfe,
        taille: Some(
            usize::from(u16::from_be_bytes([*unite.first()?, *unite.get(1)?]) & 0x0FFF) * 2,
        ),
        type_de_sous_flux: 0,
        complement: sous_flux,
        bsmod: 0,
    })
}

// ── Le verdict sur des paquets ─────────────────────────────────────────────

/// L'en-tête annonce une taille : le paquet doit la contenir. Un en-tête
/// qui promet plus d'octets qu'il n'en suit n'est pas une trame — c'est un
/// hasard d'octets, ou un fichier tronqué.
fn trame_entiere(e: &EnteteDolby, disponibles: usize) -> bool {
    e.taille.is_some_and(|t| t > 0 && t <= disponibles)
}

/// Le flux que portent ces paquets bruts, lus dans l'ordre jusqu'au premier
/// en-tête complet. TrueHD n'a de synchronisation majeure que toutes les
/// 128 unités au plus : le démuxeur en passe autant qu'il faut.
pub fn analyser_les_paquets<'a>(
    codec: CodecDolby,
    paquets: impl IntoIterator<Item = &'a [u8]>,
) -> Option<FluxDolby> {
    for paquet in paquets {
        let flux = match codec {
            CodecDolby::Ac3 => analyser_ac3(paquet)
                .filter(|e| e.codec == CodecDolby::Ac3 && trame_entiere(e, paquet.len()))
                .map(|e| FluxDolby {
                    codec: e.codec,
                    frequence: e.frequence,
                    canaux: e.canaux + u8::from(e.lfe),
                    lfe: e.lfe,
                    atmos: None,
                }),
            CodecDolby::Eac3 => flux_eac3(paquet),
            CodecDolby::TrueHd => analyser_truehd(paquet)
                .filter(|e| trame_entiere(e, paquet.len()))
                .map(|e| FluxDolby {
                    codec: e.codec,
                    frequence: e.frequence,
                    canaux: e.canaux + u8::from(e.lfe),
                    lfe: e.lfe,
                    atmos: Some(e.complement >= 4),
                }),
        };
        if flux.is_some() {
            return flux;
        }
    }
    None
}

// ── IEC 61937 ──────────────────────────────────────────────────────────────

/// La porteuse PCM qu'un flux exige de la liaison numérique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Porteuse {
    pub canaux: u8,
    pub frequence: u32,
}

/// IEC 61937 : AC-3 sur 2 canaux à la fréquence du flux ; E-AC-3 sur 2 canaux
/// à quatre fois (le débit le demande) ; TrueHD sur 8 canaux à quatre fois
/// (« HBR », HDMI seulement : une liaison S/PDIF ne le porte pas).
pub fn porteuse_iec61937(flux: &FluxDolby) -> Porteuse {
    match flux.codec {
        CodecDolby::Ac3 => Porteuse {
            canaux: 2,
            frequence: flux.frequence,
        },
        CodecDolby::Eac3 => Porteuse {
            canaux: 2,
            frequence: flux.frequence * 4,
        },
        CodecDolby::TrueHd => Porteuse {
            canaux: 8,
            frequence: flux.frequence.min(48_000) * 4,
        },
    }
}

/// Taille d'une rafale en octets de PCM 16 bits stéréo : 1 536 échantillons
/// pour l'AC-3, 6 144 (quatre fois) pour l'E-AC-3.
pub fn taille_de_rafale(codec: CodecDolby) -> Option<usize> {
    match codec {
        CodecDolby::Ac3 => Some(1536 * 2 * 2),
        CodecDolby::Eac3 => Some(6144 * 2 * 2),
        // MAT : 61 440 octets, 24 unités d'accès regroupées — étape suivante.
        CodecDolby::TrueHd => None,
    }
}

/// Emballe UNE trame en rafale IEC 61937, prête à partir comme des
/// échantillons PCM 16 bits petit-boutistes : préambules `Pa` `Pb` `Pc` `Pd`,
/// la trame en mots de 16 bits, puis du silence jusqu'à la taille de rafale.
///
/// `None` : TrueHD (regroupement MAT, étape suivante), ou trame trop grande
/// pour la rafale.
pub fn rafale_iec61937(entete: &EnteteDolby, trame: &[u8]) -> Option<Vec<u8>> {
    let taille = taille_de_rafale(entete.codec)?;
    if 8 + trame.len().next_multiple_of(2) > taille {
        return None;
    }
    // `Pc` : type de données, et pour l'AC-3 le `bsmod` dans les bits 8-10.
    let pc = entete.codec.type_iec61937() | (u16::from(entete.bsmod & 7) << 8);
    // `Pd` : longueur en BITS pour l'AC-3, en OCTETS pour l'E-AC-3.
    let pd = match entete.codec {
        CodecDolby::Ac3 => u16::try_from(trame.len() * 8).ok()?,
        _ => u16::try_from(trame.len()).ok()?,
    };
    let mut sortie = Vec::with_capacity(taille);
    for mot in [0xF872u16, 0x4E1F, pc, pd] {
        sortie.extend_from_slice(&mot.to_le_bytes());
    }
    // La trame est en mots gros-boutistes : chaque mot part petit-boutiste.
    for paire in trame.chunks(2) {
        let mot = u16::from_be_bytes([paire[0], *paire.get(1).unwrap_or(&0)]);
        sortie.extend_from_slice(&mot.to_le_bytes());
    }
    sortie.resize(taille, 0);
    Some(sortie)
}

// ── Le chemin du signal ────────────────────────────────────────────────────

/// Une étape, sous la forme des étapes de `signal_path` (`name`,
/// `description`, `bit_perfect`, `code`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Etape {
    pub name: &'static str,
    pub description: String,
    pub bit_perfect: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
}

/// Le code de l'étape de transport tant qu'aucune sortie n'émet d'IEC 61937.
pub const CODE_PASSTHROUGH_NON_PRIS_EN_CHARGE: &str = "passthrough_non_pris_en_charge";

/// Le chemin qu'un flux Dolby prendrait : source et démuxage, intacts ; puis
/// le transport IEC 61937, qui DIT qu'il n'est pas encore pris en charge.
pub fn chemin_du_signal(flux: &FluxDolby) -> Vec<Etape> {
    let p = porteuse_iec61937(flux);
    let liaison = if flux.codec == CodecDolby::TrueHd {
        "HDMI (HBR)"
    } else {
        "HDMI ou S/PDIF"
    };
    vec![
        Etape {
            name: "Source",
            description: format!("MKV — {}", flux.decrire()),
            bit_perfect: true,
            code: None,
        },
        Etape {
            name: "Démuxage",
            description: "piste audio lue telle quelle, sans décodage".into(),
            bit_perfect: true,
            code: None,
        },
        Etape {
            name: "Transport",
            description: format!(
                "passthrough IEC 61937 vers l'amplificateur ({liaison}, porteuse {} canaux à \
                 {} kHz) : pas encore pris en charge, aucune sortie de Tune n'émet ce flux (#6353)",
                p.canaux,
                khz(p.frequence)
            ),
            bit_perfect: false,
            code: Some(CODE_PASSTHROUGH_NON_PRIS_EN_CHARGE),
        },
    ]
}

#[cfg(test)]
#[path = "flux_dolby_tests.rs"]
mod tests;
