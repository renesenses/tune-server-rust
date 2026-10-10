//! La disposition des canaux DÉCLARÉE par le fichier, et le routage qui en
//! découle vers la sortie.
//!
//! # Ce qui manquait
//!
//! Tune déduisait la disposition du seul nombre de canaux, et adaptait vers la
//! sortie par position : un 4.0 (FL FR BL BR) vers un ampli HDMI ouvert en 6
//! voies recopiait la trame en tête, donc **BL et BR partaient sur FC et LFE** ;
//! vers la stéréo, `build_downmix_matrix(4, 2)` n'avait pas de cas et **les
//! voies arrière étaient perdues**. Et rien ne lisait ce que le fichier dit de
//! lui-même :
//!
//! | format | où la disposition est écrite | lue ici par |
//! |--------|------------------------------|-------------|
//! | WAV    | `dwChannelMask` du `fmt ` WAVE_FORMAT_EXTENSIBLE | [`depuis_wav`] |
//! | FLAC   | commentaire `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` | [`depuis_flac`] |
//! | DSF    | `channel_type` du bloc `fmt ` (1 à 7) | [`depuis_dsf`] |
//! | DFF    | identifiants du sous-bloc `CHNL` (SLFT, SRGT, C, LFE…) | [`depuis_dff`] |
//!
//! # Ce que fait le routage
//!
//! Chaque canal de la source a une POSITION (un bit `SPEAKER_*` de
//! `ksmedia.h`). La sortie a la disposition par défaut de son nombre de
//! voies (2 : FL FR ; 6 : FL FR FC LFE BL BR ; 8 : … SL SR). Une position
//! présente des deux côtés est RECOPIÉE (gain 1, au bit près) ; une position
//! absente de la sortie se replie, dans cet ordre : latéral ↔ arrière du même
//! côté (gain 1), arrière-centre sur la paire arrière, centre et voies
//! arrière vers l'avant à −3 dB (ITU-R BS.775), hauteurs sur leur voie au sol.
//! Le LFE n'est jamais replié dans une voie pleine bande (règle ITU, déjà celle
//! de `build_downmix_matrix`). Chaque ligne dont la somme des gains dépasse 1
//! est normalisée : pas d'écrêtage possible.
//!
//! Au-delà de 8 canaux, aucune disposition par défaut ne fait foi : on ne
//! route pas, l'adaptation d'avant reste.
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

// Les positions de haut-parleur de `ksmedia.h` (`SPEAKER_*`).
pub const FL: u32 = 0x1;
pub const FR: u32 = 0x2;
pub const FC: u32 = 0x4;
pub const LFE: u32 = 0x8;
pub const BL: u32 = 0x10;
pub const BR: u32 = 0x20;
pub const FLC: u32 = 0x40;
pub const FRC: u32 = 0x80;
pub const BC: u32 = 0x100;
pub const SL: u32 = 0x200;
pub const SR: u32 = 0x400;
pub const TC: u32 = 0x800;
pub const TFL: u32 = 0x1000;
pub const TFC: u32 = 0x2000;
pub const TFR: u32 = 0x4000;
pub const TBL: u32 = 0x8000;
pub const TBC: u32 = 0x10000;
pub const TBR: u32 = 0x20000;

/// −3 dB (1/√2), le coefficient ITU-R BS.775.
const K: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// La position de chaque canal, dans l'ordre où le flux les entrelace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disposition(Vec<u32>);

impl Disposition {
    pub fn positions(&self) -> &[u32] {
        &self.0
    }
    pub fn canaux(&self) -> u16 {
        self.0.len() as u16
    }
    /// Des positions toutes distinctes, une par canal.
    fn depuis_positions(positions: Vec<u32>) -> Option<Self> {
        let mut vues = 0u32;
        for p in &positions {
            if p.count_ones() != 1 || vues & p != 0 {
                return None;
            }
            vues |= p;
        }
        (!positions.is_empty()).then_some(Self(positions))
    }
    /// Le masque `dwChannelMask` : les canaux suivent l'ordre croissant des
    /// bits. `None` s'il ne compte pas exactement `canaux` bits.
    pub fn depuis_masque(masque: u32, canaux: u16) -> Option<Self> {
        if masque.count_ones() != u32::from(canaux) {
            return None;
        }
        Self::depuis_positions(
            (0..32)
                .map(|b| 1u32 << b)
                .filter(|b| masque & b != 0)
                .collect(),
        )
    }
    /// L'ordre par défaut de FLAC et de WAVE_FORMAT_EXTENSIBLE, de 1 à 8
    /// canaux — l'ordre que Tune supposait jusqu'ici.
    pub fn par_defaut(canaux: u16) -> Option<Self> {
        let p: &[u32] = match canaux {
            1 => &[FC],
            2 => &[FL, FR],
            3 => &[FL, FR, FC],
            4 => &[FL, FR, BL, BR],
            5 => &[FL, FR, FC, BL, BR],
            6 => &[FL, FR, FC, LFE, BL, BR],
            7 => &[FL, FR, FC, LFE, BC, SL, SR],
            8 => &[FL, FR, FC, LFE, BL, BR, SL, SR],
            _ => return None,
        };
        Some(Self(p.to_vec()))
    }
    /// Le badge affiché sous la pochette (« 4.0 », « 5.1 », « 7.1 »…).
    ///
    /// Compté sur les POSITIONS déclarées, pas sur le nombre de canaux :
    /// voies pleine bande au sol, puis LFE, puis hauteurs. Un 4.0 (FL FR BL
    /// BR, masque 0x33) n'est plus un « 5.1 », un 6.0 (FL FR FC BL BR BC)
    /// non plus. `None` pour la mono et la stéréo, comme
    /// [`crate::audio::channels::channel_badge`].
    pub fn badge(&self) -> Option<String> {
        if self.canaux() <= 2 {
            return None;
        }
        const HAUTEURS: u32 = TC | TFL | TFC | TFR | TBL | TBC | TBR;
        let compte = |filtre: &dyn Fn(u32) -> bool| self.0.iter().filter(|p| filtre(**p)).count();
        let lfe = compte(&|p| p == LFE);
        let hauteurs = compte(&|p| p & HAUTEURS != 0);
        let sol = self.0.len() - lfe - hauteurs;
        let nom = if hauteurs > 0 {
            format!("{sol}.{lfe}.{hauteurs}")
        } else {
            format!("{sol}.{lfe}")
        };
        // Mêmes libellés que les dispositions nommées du serveur (#5576).
        Some(match nom.as_str() {
            "7.1.4" | "9.1.6" => format!("{nom} Atmos / Auro-3D"),
            _ => nom,
        })
    }
    /// Cette disposition est-elle exactement celle que Tune supposait ?
    pub fn est_par_defaut(&self) -> bool {
        Self::par_defaut(self.canaux()).as_ref() == Some(self)
    }
}

// ---------------------------------------------------------------------------
// Lecture dans les en-têtes
// ---------------------------------------------------------------------------

/// WAV : le corps du bloc `fmt `. Seul WAVE_FORMAT_EXTENSIBLE (0xFFFE, corps
/// d'au moins 40 octets) déclare un masque ; un masque nul ne dit rien.
pub fn depuis_wav(fmt: &[u8]) -> Option<Disposition> {
    if fmt.len() < 40 || u16::from_le_bytes([fmt[0], fmt[1]]) != 0xFFFE {
        return None;
    }
    let canaux = u16::from_le_bytes([fmt[2], fmt[3]]);
    let masque = u32::from_le_bytes([fmt[20], fmt[21], fmt[22], fmt[23]]);
    (masque != 0)
        .then(|| Disposition::depuis_masque(masque, canaux))
        .flatten()
}

/// FLAC : la valeur du commentaire `WAVEFORMATEXTENSIBLE_CHANNEL_MASK`
/// (« 0x0033 » ou décimal), pour `canaux` canaux.
pub fn depuis_flac(valeur: &str, canaux: u16) -> Option<Disposition> {
    let v = valeur.trim();
    let masque = match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => v.parse().ok()?,
    };
    Disposition::depuis_masque(masque, canaux)
}

/// DSF : `channel_type` (spécification Sony DSF 1.01).
pub fn depuis_dsf(channel_type: u32, canaux: u16) -> Option<Disposition> {
    let p: &[u32] = match channel_type {
        1 => &[FC],
        2 => &[FL, FR],
        3 => &[FL, FR, FC],
        4 => &[FL, FR, BL, BR],
        5 => &[FL, FR, FC, LFE],
        6 => &[FL, FR, FC, BL, BR],
        7 => &[FL, FR, FC, LFE, BL, BR],
        _ => return None,
    };
    (p.len() == usize::from(canaux)).then(|| Disposition(p.to_vec()))
}

/// DFF (DSDIFF 1.5) : les identifiants du sous-bloc `CHNL`, dans l'ordre du flux.
pub fn depuis_dff(identifiants: &[[u8; 4]]) -> Option<Disposition> {
    let positions = identifiants
        .iter()
        .map(|id| match id {
            b"SLFT" | b"MLFT" => Some(FL),
            b"SRGT" | b"MRGT" => Some(FR),
            b"C   " => Some(FC),
            b"LFE " => Some(LFE),
            b"LS  " => Some(BL),
            b"RS  " => Some(BR),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Disposition::depuis_positions(positions)
}

const LECTURE_MAX: u64 = 16 * 1024 * 1024;

fn lire_n(f: &mut impl Read, n: usize) -> Option<Vec<u8>> {
    let mut b = vec![0; n];
    f.read_exact(&mut b).ok()?;
    Some(b)
}

fn wav(f: &mut (impl Read + Seek)) -> Option<Disposition> {
    f.seek(SeekFrom::Start(12)).ok()?;
    for _ in 0..64 {
        let h = lire_n(f, 8)?;
        let taille = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64;
        if &h[..4] == b"fmt " {
            return depuis_wav(&lire_n(f, taille.min(1024) as usize)?);
        }
        f.seek(SeekFrom::Current((taille + (taille & 1)) as i64))
            .ok()?;
    }
    None
}

fn flac(f: &mut (impl Read + Seek)) -> Option<Disposition> {
    let mut canaux = 0u16;
    let mut lu = 4u64;
    loop {
        let h = lire_n(f, 4)?;
        let dernier = h[0] & 0x80 != 0;
        let taille = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        lu += 4 + taille as u64;
        if lu > LECTURE_MAX {
            return None;
        }
        match h[0] & 0x7F {
            // STREAMINFO : canaux sur 3 bits, octet 12 (bits 1..3), plus 1.
            0 => {
                let corps = lire_n(f, taille)?;
                if corps.len() >= 13 {
                    canaux = u16::from((corps[12] >> 1) & 0x07) + 1;
                }
            }
            4 => return commentaire_flac(&lire_n(f, taille)?, canaux),
            // Les autres blocs (PICTURE de plusieurs Mio…) sont SAUTÉS : le
            // badge de la bibliothèque lit cet en-tête à chaque liste.
            _ => {
                f.seek(SeekFrom::Current(taille as i64)).ok()?;
            }
        }
        if dernier {
            return None;
        }
    }
}

fn commentaire_flac(corps: &[u8], canaux: u16) -> Option<Disposition> {
    let u32le = |o: usize| -> Option<usize> {
        Some(u32::from_le_bytes(corps.get(o..o + 4)?.try_into().ok()?) as usize)
    };
    let mut o = 4 + u32le(0)?;
    let n = u32le(o)?;
    o += 4;
    for _ in 0..n {
        let l = u32le(o)?;
        let texte = std::str::from_utf8(corps.get(o + 4..o + 4 + l)?).ok()?;
        o += 4 + l;
        if let Some((cle, valeur)) = texte.split_once('=')
            && cle.eq_ignore_ascii_case("WAVEFORMATEXTENSIBLE_CHANNEL_MASK")
        {
            return depuis_flac(valeur, canaux);
        }
    }
    None
}

fn dsf(f: &mut (impl Read + Seek)) -> Option<Disposition> {
    // « DSD » (28 octets), puis le bloc « fmt » : type à +20, canaux à +24.
    f.seek(SeekFrom::Start(28)).ok()?;
    let fmt = lire_n(f, 52)?;
    if &fmt[..4] != b"fmt " {
        return None;
    }
    let u = |o: usize| u32::from_le_bytes([fmt[o], fmt[o + 1], fmt[o + 2], fmt[o + 3]]);
    depuis_dsf(u(20), u16::try_from(u(24)).ok()?)
}

fn dff(f: &mut (impl Read + Seek)) -> Option<Disposition> {
    // FRM8 <taille u64 BE> « DSD », puis des blocs <id> <taille u64 BE>.
    f.seek(SeekFrom::Start(16)).ok()?;
    for _ in 0..64 {
        let h = lire_n(f, 12)?;
        let taille = u64::from_be_bytes(h[4..12].try_into().ok()?);
        if &h[..4] != b"PROP" {
            f.seek(SeekFrom::Current((taille + (taille & 1)) as i64))
                .ok()?;
            continue;
        }
        let prop = lire_n(f, taille.min(LECTURE_MAX) as usize)?;
        let mut o = 4; // « SND »
        while o + 12 <= prop.len() {
            let t = u64::from_be_bytes(prop[o + 4..o + 12].try_into().ok()?) as usize;
            if &prop[o..o + 4] == b"CHNL" {
                let c = prop.get(o + 12..o + 12 + t)?;
                let n = usize::from(u16::from_be_bytes([*c.first()?, *c.get(1)?]));
                let ids: Vec<[u8; 4]> = (0..n)
                    .map(|i| c.get(2 + 4 * i..6 + 4 * i).and_then(|s| s.try_into().ok()))
                    .collect::<Option<_>>()?;
                return depuis_dff(&ids);
            }
            o += 12 + t + (t & 1);
        }
        return None;
    }
    None
}

/// La disposition que le fichier DÉCLARE, d'après sa signature (WAV, FLAC,
/// DSF, DFF). `None` quand le fichier ne dit rien, ou rien de lisible.
pub fn lire_le_fichier(chemin: &Path) -> Option<Disposition> {
    let mut f = std::fs::File::open(chemin).ok()?;
    let magie = lire_n(&mut f, 12)?;
    match &magie[..4] {
        b"RIFF" if &magie[8..12] == b"WAVE" => wav(&mut f),
        b"fLaC" => {
            f.seek(SeekFrom::Start(4)).ok()?;
            flac(&mut f)
        }
        b"DSD " => dsf(&mut f),
        b"FRM8" => dff(&mut f),
        _ => None,
    }
}

/// La disposition à retenir pour router : celle du fichier si elle diffère de
/// l'ordre par défaut ; `None` sinon (le comportement par défaut suffit).
pub fn declaree_hors_defaut(chemin: &Path) -> Option<Disposition> {
    lire_le_fichier(chemin).filter(|d| !d.est_par_defaut())
}

// ---------------------------------------------------------------------------
// Routage
// ---------------------------------------------------------------------------

fn replis(p: u32, sortie: &[u32], profondeur: u8) -> Vec<(usize, f32)> {
    if let Some(i) = sortie.iter().position(|s| *s == p) {
        return vec![(i, 1.0)];
    }
    if profondeur == 0 {
        return Vec::new();
    }
    let a = |q: u32| sortie.contains(&q);
    let suite = |q: u32, g: f32| -> Vec<(usize, f32)> {
        replis(q, sortie, profondeur - 1)
            .into_iter()
            .map(|(i, h)| (i, g * h))
            .collect()
    };
    let deux = |q1: u32, q2: u32, g: f32| -> Vec<(usize, f32)> {
        let mut v = suite(q1, g);
        v.extend(suite(q2, g));
        v
    };
    match p {
        SL if a(BL) => suite(BL, 1.0),
        SR if a(BR) => suite(BR, 1.0),
        BL if a(SL) => suite(SL, 1.0),
        BR if a(SR) => suite(SR, 1.0),
        SL | BL => suite(FL, K),
        SR | BR => suite(FR, K),
        BC if a(BL) && a(BR) => deux(BL, BR, K),
        BC if a(SL) && a(SR) => deux(SL, SR, K),
        BC => deux(FL, FR, 0.5),
        FC => deux(FL, FR, K),
        FLC => suite(FL, 1.0),
        FRC => suite(FR, 1.0),
        TFL => suite(FL, K),
        TFR => suite(FR, K),
        TFC | TC => suite(FC, K),
        TBL => suite(BL, K),
        TBR => suite(BR, K),
        TBC => suite(BC, K),
        // LFE : jamais dans une voie pleine bande (ITU) ; inconnu : rien.
        _ => Vec::new(),
    }
}

/// La matrice `sortie × source` (ligne par voie de sortie) qui route la
/// disposition `source` vers les `sortie` voies d'un périphérique, normalisée
/// par ligne. `None` quand la sortie n'a pas de disposition par défaut (plus
/// de 8 voies) ; pour une sortie mono, toutes les voies pleine bande sont
/// sommées à parts égales.
pub fn matrice_de_routage(source: &Disposition, sortie: u16) -> Option<Vec<f32>> {
    let n = source.0.len();
    let m = usize::from(sortie);
    let mut matrice = vec![0.0f32; m * n];
    if sortie == 1 {
        for (i, p) in source.0.iter().enumerate() {
            if *p != LFE {
                matrice[i] = 1.0;
            }
        }
    } else {
        let cible = Disposition::par_defaut(sortie)?;
        for (i, p) in source.0.iter().enumerate() {
            for (o, g) in replis(*p, &cible.0, 3) {
                matrice[o * n + i] += g;
            }
        }
    }
    for ligne in matrice.chunks_exact_mut(n.max(1)) {
        let somme: f32 = ligne.iter().map(|c| c.abs()).sum();
        if somme > 1.0 {
            for c in ligne {
                *c /= somme;
            }
        }
    }
    Some(matrice)
}

#[cfg(test)]
#[path = "disposition_canaux_tests.rs"]
mod tests;
