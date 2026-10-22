//! Les quatre messages du protocole AIR, sans aucune entrée-sortie.
//!
//! Tout ce module est pur : il construit et analyse des octets. Les formats
//! viennent de la rétro-ingénierie publiée par Rémi (fil 2336 du forum),
//! reconstituée par capture passive du trafic entre Roon et un Expert. Seuls
//! des FAITS en sont repris ; le code est écrit ici, et les vecteurs de test
//! (`vecteurs_tune.json`) sont les nôtres.
//!
//! Tout est en big-endian. Chaque message commence par `D` et une lettre :
//!
//! | Type | Sens | Ports | Taille |
//! |------|------|-------|--------|
//! | `Da` | ampli → broadcast | 45454 → 45454 | 345 |
//! | `Dr` | hôte → ampli | éphémère → 45455 | 142 |
//! | `Dm` | hôte → ampli | 45456 → 45455 | variable |
//! | `Df` | ampli → hôte | 45456 → 45456 | 41 + 4n |

/// Port des annonces `Da` (broadcast).
pub const PORT_ANNONCES: u16 = 45454;
/// Port de l'ampli qui reçoit les commandes `Dr` et l'audio `Dm`.
pub const PORT_AMPLI: u16 = 45455;
/// Port local d'où part l'audio et où reviennent les retours `Df`.
pub const PORT_AUDIO_LOCAL: u16 = 45456;

/// Trames (échantillons par canal) dans un paquet audio.
pub const TRAMES_PAR_PAQUET: usize = 200;
/// Taille d'une annonce `Da`.
pub const TAILLE_ANNONCE: usize = 345;
/// Taille d'une commande `Dr`.
pub const TAILLE_COMMANDE: usize = 142;
/// Octets nuls qui terminent `Dr` et `Dm`.
pub const BOURRAGE_FINAL: usize = 128;
/// Taille d'un paquet d'arrêt `Dm` (en-tête, CRC, bourrage).
pub const TAILLE_ARRET: usize = 22 + 2 + BOURRAGE_FINAL;
/// Largeur maximale des résidus au-delà de 96 kHz (codec avec perte).
pub const LARGEUR_MAX_HAUTE_FREQUENCE: u8 = 10;
/// Nombre de sources décrites dans une annonce.
pub const NOMBRE_DE_SOURCES: usize = 15;

/// Les opcodes de commande connus. Graves, aigus, balance et pre-out n'en ont
/// AUCUN : ils se lisent dans l'annonce, ils ne se règlent pas.
pub mod opcode {
    pub const VEILLE: u16 = 0x0001;
    pub const VOLUME: u16 = 0x0004;
    pub const SOURCE: u16 = 0x0005;
    pub const FIN_DE_SOURDINE: u16 = 0x0007;
    pub const ALLUMER: u16 = 0x0101;
    pub const SOURDINE: u16 = 0x0107;
}

// ─── CRC-16/CCITT-FALSE ─────────────────────────────────────────────────────

const fn table_crc() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut b = 0;
        while b < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x1021
            } else {
                c << 1
            };
            b += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE_CRC: [u16; 256] = table_crc();

/// CRC-16/CCITT-FALSE : polynôme 0x1021, init 0xFFFF, sans réflexion, sans
/// XOR final. Valeur de contrôle du catalogue : `crc16(b"123456789") == 0x29B1`.
#[must_use]
pub fn crc16(donnees: &[u8]) -> u16 {
    donnees.iter().fold(0xFFFF_u16, |c, &o| {
        (c << 8) ^ TABLE_CRC[usize::from((c >> 8) as u8 ^ o)]
    })
}

fn crc_valide(message: &[u8], fin_du_corps: usize) -> bool {
    message.len() >= fin_du_corps + 2
        && crc16(&message[..fin_du_corps])
            == u16::from_be_bytes([message[fin_du_corps], message[fin_du_corps + 1]])
}

fn lire_u16(o: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([o[i], o[i + 1]])
}

fn lire_u32(o: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([o[i], o[i + 1], o[i + 2], o[i + 3]])
}

fn lire_u64(o: &[u8], i: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&o[i..i + 8]);
    u64::from_be_bytes(b)
}

// ─── Fréquences ─────────────────────────────────────────────────────────────

/// Les fréquences que transporte AIR, dans l'ordre de leur indice (octet 11
/// de `Dm`, décalé d'un bit).
pub const FREQUENCES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// L'indice AIR d'une fréquence, `None` si AIR ne la transporte pas.
#[must_use]
pub fn indice_de_frequence(frequence: u32) -> Option<u8> {
    FREQUENCES
        .iter()
        .position(|&f| f == frequence)
        .map(|i| i as u8)
}

/// La fréquence reçue sur l'entrée active, telle que l'annonce la code (octet
/// 318). Hypothèse cohérente mais non prouvée sur toutes les entrées.
#[must_use]
pub fn frequence_annoncee(code: u8) -> Option<u32> {
    match code {
        7 => Some(44_100),
        8 => Some(48_000),
        10 => Some(88_200),
        11 => Some(96_000),
        13 => Some(176_400),
        14 => Some(192_000),
        _ => None,
    }
}

// ─── Da : annonce et état ───────────────────────────────────────────────────

/// Une source de l'ampli, telle que l'annonce la décrit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAnnoncee {
    pub indice: u8,
    /// Octet d'état ASCII : `'0'` = désactivée, un autre chiffre = active.
    pub etat: u8,
    pub nom: String,
}

impl SourceAnnoncee {
    #[must_use]
    pub fn active(&self) -> bool {
        self.etat != b'0'
    }
}

/// L'état complet d'un ampli, lu dans une annonce `Da`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annonce {
    pub compteur: u16,
    /// Octet de poids faible de l'identifiant du flux AIR en cours, 0 = aucun.
    pub flux_en_cours: u8,
    pub mac: [u8; 6],
    pub nom: String,
    pub sources: Vec<SourceAnnoncee>,
    pub allume: bool,
    /// Graves ou aigus non nuls (hypothèse cohérente, non prouvée).
    pub tonalite_active: bool,
    /// Bit 7 de l'octet 308 : aucun flux AIR n'est joué.
    pub aucun_flux_air: bool,
    pub source_choisie: u8,
    pub sourdine: bool,
    /// Volume en demi-dB (−62 = −31 dB) : entier, pour rester exact.
    pub volume_demi_db: i16,
    /// Aigus et graves en demi-dB, signés (±36).
    pub aigus_demi_db: i8,
    pub graves_demi_db: i8,
    /// Balance en %, négatif = gauche.
    pub balance_pour_cent: i8,
    /// Octet brut du pre-out : 0x00 inactif ; le sens des bits est inconnu.
    pub pre_out: u8,
    pub frequence_entree: Option<u32>,
    pub horloge_ms: u32,
    pub session: [u8; 4],
}

impl Annonce {
    /// Le volume en dB.
    #[must_use]
    pub fn volume_db(&self) -> f64 {
        f64::from(self.volume_demi_db) / 2.0
    }

    /// L'identité stable de l'appareil : sa MAC, en minuscules, avec deux-points.
    #[must_use]
    pub fn mac_texte(&self) -> String {
        self.mac
            .iter()
            .map(|o| format!("{o:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// Pourquoi un datagramme n'a pas été retenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refus {
    TailleInattendue { attendue: usize, recue: usize },
    MauvaisType,
    CrcFaux,
    Incoherent(&'static str),
}

impl std::fmt::Display for Refus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TailleInattendue { attendue, recue } => {
                write!(f, "taille {recue} au lieu de {attendue}")
            }
            Self::MauvaisType => write!(f, "type de message inattendu"),
            Self::CrcFaux => write!(f, "CRC faux"),
            Self::Incoherent(m) => write!(f, "message incohérent : {m}"),
        }
    }
}

fn texte_ascii(octets: &[u8]) -> String {
    let fin = octets.iter().position(|&o| o == 0).unwrap_or(octets.len());
    octets[..fin]
        .iter()
        .map(|&o| {
            if o.is_ascii_graphic() || o == b' ' {
                char::from(o)
            } else {
                '?'
            }
        })
        .collect()
}

/// Analyse une annonce `Da`. Une annonce au CRC faux est refusée.
pub fn analyser_annonce(o: &[u8]) -> Result<Annonce, Refus> {
    if o.len() != TAILLE_ANNONCE {
        return Err(Refus::TailleInattendue {
            attendue: TAILLE_ANNONCE,
            recue: o.len(),
        });
    }
    if &o[0..2] != b"Da" {
        return Err(Refus::MauvaisType);
    }
    if !crc_valide(o, 343) {
        return Err(Refus::CrcFaux);
    }
    let longueur_nom = usize::from(o[18]).min(32);
    let sources = (0..NOMBRE_DE_SOURCES)
        .map(|i| {
            let debut = 52 + 17 * i;
            SourceAnnoncee {
                indice: i as u8,
                etat: o[debut],
                nom: texte_ascii(&o[debut + 1..debut + 17]),
            }
        })
        .collect();
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&o[12..18]);
    let mut session = [0u8; 4];
    session.copy_from_slice(&o[339..343]);
    Ok(Annonce {
        compteur: lire_u16(o, 2),
        flux_en_cours: o[10],
        mac,
        nom: texte_ascii(&o[19..19 + longueur_nom]),
        sources,
        allume: o[307] & 0x80 != 0,
        tonalite_active: o[307] & 0x10 != 0,
        aucun_flux_air: o[308] & 0x80 != 0,
        source_choisie: (o[308] >> 2) & 0x0F,
        sourdine: o[308] & 0x02 != 0,
        volume_demi_db: i16::from(o[310]) - 195,
        aigus_demi_db: o[311] as i8,
        graves_demi_db: o[312] as i8,
        balance_pour_cent: o[313] as i8,
        pre_out: o[314],
        frequence_entree: frequence_annoncee(o[318]),
        horloge_ms: lire_u32(o, 331),
        session,
    })
}

// ─── Dr : commandes ─────────────────────────────────────────────────────────

/// Construit une commande `Dr` : compteur de paquets, compteur de commandes,
/// opcode, argument en float32, CRC sur les 12 premiers octets, 128 zéros.
#[must_use]
pub fn commande(compteur_paquet: u16, compteur_commande: u16, op: u16, valeur: f32) -> Vec<u8> {
    let mut o = Vec::with_capacity(TAILLE_COMMANDE);
    o.extend_from_slice(b"Dr");
    o.extend_from_slice(&compteur_paquet.to_be_bytes());
    o.extend_from_slice(&compteur_commande.to_be_bytes());
    o.extend_from_slice(&op.to_be_bytes());
    o.extend_from_slice(&valeur.to_be_bytes());
    let crc = crc16(&o);
    o.extend_from_slice(&crc.to_be_bytes());
    o.resize(TAILLE_COMMANDE, 0);
    o
}

/// Une commande `Dr` relue (pour le simulateur et les tests).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CommandeLue {
    pub compteur_paquet: u16,
    pub compteur_commande: u16,
    pub opcode: u16,
    pub valeur: f32,
}

pub fn analyser_commande(o: &[u8]) -> Result<CommandeLue, Refus> {
    if o.len() != TAILLE_COMMANDE {
        return Err(Refus::TailleInattendue {
            attendue: TAILLE_COMMANDE,
            recue: o.len(),
        });
    }
    if &o[0..2] != b"Dr" {
        return Err(Refus::MauvaisType);
    }
    if !crc_valide(o, 12) {
        return Err(Refus::CrcFaux);
    }
    if o[14..].iter().any(|&b| b != 0) {
        return Err(Refus::Incoherent("bourrage non nul"));
    }
    Ok(CommandeLue {
        compteur_paquet: lire_u16(o, 2),
        compteur_commande: lire_u16(o, 4),
        opcode: lire_u16(o, 6),
        valeur: f32::from_be_bytes([o[8], o[9], o[10], o[11]]),
    })
}

// ─── Dm : audio et codec ────────────────────────────────────────────────────

/// Ce qui identifie un paquet audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnteteAudio {
    pub session: [u8; 4],
    pub flux: u32,
    pub frequence: u32,
    /// Bit « lecture » : 0 pendant le pré-remplissage, 1 ensuite.
    pub lecture: bool,
    /// Position du premier échantillon, en trames depuis le début du flux.
    pub position: u64,
}

/// Résidu signé → champ : magnitude décalée d'un bit, signe dans le bit de
/// poids faible.
fn replier(q: i64) -> u64 {
    (q.unsigned_abs() << 1) | u64::from(q < 0)
}

fn deplier(champ: u64) -> i64 {
    let magnitude = (champ >> 1) as i64;
    if champ & 1 != 0 {
        -magnitude
    } else {
        magnitude
    }
}

/// Division par `2^s` tronquée VERS ZÉRO. C'est la seule règle qui reproduit
/// les paquets de Roon au-delà de 96 kHz (le plancher et l'arrondi échouent).
fn tronquer_vers_zero(d: i64, s: u8) -> i64 {
    if d < 0 { -((-d) >> s) } else { d >> s }
}

/// Encode les échantillons d'un canal : premier échantillon en pleine
/// précision, puis les différences d'ordre 1, quantifiées en boucle fermée
/// sur `s` bits quand `largeur_max` est posée (au-delà de 96 kHz).
fn encoder_canal(ech: &[i32], largeur_max: Option<u8>, sortie: &mut Vec<u8>) {
    let mut s: u8 = 0;
    let (champs, largeur) = loop {
        let mut rec = i64::from(ech[0]);
        let mut champs = Vec::with_capacity(ech.len().saturating_sub(1));
        for &x in &ech[1..] {
            let q = tronquer_vers_zero(i64::from(x) - rec, s);
            rec += q << s;
            champs.push(replier(q));
        }
        let max = champs.iter().copied().max().unwrap_or(0);
        let largeur = (64 - max.leading_zeros()) as u8;
        match largeur_max {
            Some(b) if largeur > b => s += 1,
            _ => break (champs, largeur),
        }
    };
    let base = (ech[0] as u32) & 0x00FF_FFFF;
    sortie.extend_from_slice(&base.to_be_bytes()[1..]);
    sortie.extend_from_slice(&[s, s, largeur, largeur]);
    // Champs de `largeur` bits, MSB d'abord, zéros de bourrage à droite.
    let mut accu: u128 = 0;
    let mut bits: u32 = 0;
    let largeur = u32::from(largeur);
    if largeur == 0 {
        return;
    }
    for f in champs {
        accu = (accu << largeur) | u128::from(f);
        bits += largeur;
        while bits >= 8 {
            bits -= 8;
            sortie.push((accu >> bits) as u8);
        }
        accu &= (1u128 << bits) - 1;
    }
    if bits > 0 {
        sortie.push((accu << (8 - bits)) as u8);
    }
}

/// Un paquet audio `Dm` complet. `trames` : 200 paires (gauche, droite)
/// d'entiers signés 24 bits (une source 16 bits arrive décalée de 8 bits).
///
/// # Panics
/// Si `trames` est vide, ou si la fréquence n'est pas une fréquence AIR :
/// l'émetteur ne construit jamais un tel paquet.
#[must_use]
pub fn paquet_audio(entete: &EnteteAudio, trames: &[[i32; 2]]) -> Vec<u8> {
    assert!(
        !trames.is_empty(),
        "un paquet audio porte au moins une trame"
    );
    let indice = indice_de_frequence(entete.frequence).expect("fréquence AIR");
    let mut o = Vec::with_capacity(22 + 2 * (7 + 600) + 2 + BOURRAGE_FINAL);
    o.extend_from_slice(b"Dm\x02");
    o.extend_from_slice(&entete.session);
    o.extend_from_slice(&entete.flux.to_be_bytes());
    o.push((indice << 1) | u8::from(entete.lecture));
    o.extend_from_slice(&entete.position.to_be_bytes());
    o.extend_from_slice(&(trames.len() as u16).to_be_bytes());
    let largeur_max = (entete.frequence > 96_000).then_some(LARGEUR_MAX_HAUTE_FREQUENCE);
    for canal in 0..2 {
        let ech: Vec<i32> = trames.iter().map(|t| t[canal]).collect();
        encoder_canal(&ech, largeur_max, &mut o);
    }
    let crc = crc16(&o);
    o.extend_from_slice(&crc.to_be_bytes());
    o.resize(o.len() + BOURRAGE_FINAL, 0);
    o
}

/// Le paquet d'arrêt : en-tête de type 0, un NOUVEL identifiant de flux,
/// octets 11 à 21 nuls, CRC, 128 zéros.
#[must_use]
pub fn paquet_arret(session: [u8; 4], flux: u32) -> Vec<u8> {
    let mut o = Vec::with_capacity(TAILLE_ARRET);
    o.extend_from_slice(b"Dm\x00");
    o.extend_from_slice(&session);
    o.extend_from_slice(&flux.to_be_bytes());
    o.resize(22, 0);
    let crc = crc16(&o);
    o.extend_from_slice(&crc.to_be_bytes());
    o.resize(TAILLE_ARRET, 0);
    o
}

/// Un paquet `Dm` relu (pour le simulateur et les tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaquetLu {
    Audio {
        entete: EnteteAudio,
        trames: Vec<[i32; 2]>,
        /// `s` de chaque canal : 0 = sans perte.
        decalages: [u8; 2],
    },
    Arret {
        session: [u8; 4],
        flux: u32,
    },
}

fn signe_24(v: u32) -> i32 {
    ((v << 8) as i32) >> 8
}

/// Décode un paquet `Dm` : `x[i] = x[i−1] + (v << s)`.
pub fn analyser_paquet(o: &[u8]) -> Result<PaquetLu, Refus> {
    if o.len() < 22 + 2 + BOURRAGE_FINAL || &o[0..2] != b"Dm" {
        return Err(Refus::MauvaisType);
    }
    let mut session = [0u8; 4];
    session.copy_from_slice(&o[3..7]);
    let flux = lire_u32(o, 7);
    match o[2] {
        0x00 => {
            if o.len() != TAILLE_ARRET {
                return Err(Refus::TailleInattendue {
                    attendue: TAILLE_ARRET,
                    recue: o.len(),
                });
            }
            if !crc_valide(o, 22) {
                return Err(Refus::CrcFaux);
            }
            if o[11..22].iter().any(|&b| b != 0) {
                return Err(Refus::Incoherent("arrêt aux octets 11-21 non nuls"));
            }
            Ok(PaquetLu::Arret { session, flux })
        }
        0x02 => {
            let code = o[11];
            let frequence = *FREQUENCES
                .get(usize::from(code >> 1))
                .ok_or(Refus::Incoherent("indice de fréquence"))?;
            let n = usize::from(lire_u16(o, 20));
            if n == 0 {
                return Err(Refus::Incoherent("zéro trame"));
            }
            let mut i = 22;
            let mut canaux: [Vec<i32>; 2] = [Vec::new(), Vec::new()];
            let mut decalages = [0u8; 2];
            for (c, canal) in canaux.iter_mut().enumerate() {
                if o.len() < i + 7 {
                    return Err(Refus::Incoherent("bloc de canal tronqué"));
                }
                let x0 = signe_24(u32::from_be_bytes([0, o[i], o[i + 1], o[i + 2]]));
                let (s, s2, b, b2) = (o[i + 3], o[i + 4], o[i + 5], o[i + 6]);
                if s != s2 || b != b2 {
                    return Err(Refus::Incoherent("copies de s ou de b divergentes"));
                }
                if b > 57 || s > 24 {
                    return Err(Refus::Incoherent("largeur ou décalage hors bornes"));
                }
                decalages[c] = s;
                let nb = (usize::from(b) * (n - 1)).div_ceil(8);
                if o.len() < i + 7 + nb {
                    return Err(Refus::Incoherent("résidus tronqués"));
                }
                let champs = &o[i + 7..i + 7 + nb];
                let mut x = i64::from(x0);
                canal.push(x0);
                let mut position_bit = 0usize;
                for _ in 1..n {
                    let mut f: u64 = 0;
                    for _ in 0..b {
                        let octet = champs[position_bit / 8];
                        let bit = (octet >> (7 - (position_bit % 8))) & 1;
                        f = (f << 1) | u64::from(bit);
                        position_bit += 1;
                    }
                    x += deplier(f) << s;
                    canal.push(x as i32);
                }
                i += 7 + nb;
            }
            if !crc_valide(o, i) {
                return Err(Refus::CrcFaux);
            }
            if o.len() != i + 2 + BOURRAGE_FINAL || o[i + 2..].iter().any(|&b| b != 0) {
                return Err(Refus::Incoherent("fin de paquet inattendue"));
            }
            let trames = canaux[0]
                .iter()
                .zip(&canaux[1])
                .map(|(&g, &d)| [g, d])
                .collect();
            Ok(PaquetLu::Audio {
                entete: EnteteAudio {
                    session,
                    flux,
                    frequence,
                    lecture: code & 1 != 0,
                    position: lire_u64(o, 12),
                },
                trames,
                decalages,
            })
        }
        _ => Err(Refus::MauvaisType),
    }
}

// ─── Df : retour de l'ampli ─────────────────────────────────────────────────

/// Le retour `Df`, toutes les 50 ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retour {
    pub session: [u8; 4],
    pub flux: u32,
    /// Dernière trame reçue (incluse).
    pub derniere_trame_recue: u64,
    /// Position du dernier paquet reçu.
    pub dernier_paquet_recu: u64,
    /// Position jouée, 0 tant que la lecture n'a pas commencé.
    pub position_jouee: u64,
    /// Plages manquantes `[début, fin]`, en numéros de paquet relatifs à
    /// `floor(position_jouee / 200)`.
    pub manquants: Vec<(u16, u16)>,
}

impl Retour {
    /// Les positions (en trames) des paquets réclamés.
    #[must_use]
    pub fn positions_reclamees(&self) -> Vec<u64> {
        let base = self.position_jouee / TRAMES_PAR_PAQUET as u64;
        let mut v = Vec::new();
        for &(debut, fin) in &self.manquants {
            for k in debut..=fin.max(debut) {
                v.push((base + u64::from(k)) * TRAMES_PAR_PAQUET as u64);
            }
        }
        v
    }
}

pub fn analyser_retour(o: &[u8]) -> Result<Retour, Refus> {
    if o.len() < 41 || &o[0..2] != b"Df" {
        return Err(Refus::MauvaisType);
    }
    let n = usize::from(o[38]);
    let attendue = 41 + 4 * n;
    if o.len() != attendue {
        return Err(Refus::TailleInattendue {
            attendue,
            recue: o.len(),
        });
    }
    if !crc_valide(o, attendue - 2) {
        return Err(Refus::CrcFaux);
    }
    let mut session = [0u8; 4];
    session.copy_from_slice(&o[2..6]);
    let manquants = (0..n)
        .map(|k| (lire_u16(o, 39 + 4 * k), lire_u16(o, 41 + 4 * k)))
        .collect();
    Ok(Retour {
        session,
        flux: lire_u32(o, 6),
        derniere_trame_recue: lire_u64(o, 10),
        dernier_paquet_recu: lire_u64(o, 18),
        position_jouee: lire_u64(o, 26),
        manquants,
    })
}

/// Construit un retour `Df` (pour le simulateur et les tests).
#[must_use]
pub fn retour(r: &Retour) -> Vec<u8> {
    let mut o = Vec::with_capacity(41 + 4 * r.manquants.len());
    o.extend_from_slice(b"Df");
    o.extend_from_slice(&r.session);
    o.extend_from_slice(&r.flux.to_be_bytes());
    o.extend_from_slice(&r.derniere_trame_recue.to_be_bytes());
    o.extend_from_slice(&r.dernier_paquet_recu.to_be_bytes());
    o.extend_from_slice(&r.position_jouee.to_be_bytes());
    o.extend_from_slice(&[0, 0, 0, 0]);
    o.push(r.manquants.len() as u8);
    for &(d, f) in &r.manquants {
        o.extend_from_slice(&d.to_be_bytes());
        o.extend_from_slice(&f.to_be_bytes());
    }
    let crc = crc16(&o);
    o.extend_from_slice(&crc.to_be_bytes());
    o
}

/// Construit une annonce `Da` (pour le simulateur et les tests). Les octets
/// dont le sens est inconnu restent nuls, sauf 4-6 (`02 10 33` observés).
#[must_use]
pub fn annonce(a: &Annonce) -> Vec<u8> {
    let mut o = vec![0u8; TAILLE_ANNONCE];
    o[0..2].copy_from_slice(b"Da");
    o[2..4].copy_from_slice(&a.compteur.to_be_bytes());
    o[4..7].copy_from_slice(&[0x02, 0x10, 0x33]);
    o[10] = a.flux_en_cours;
    o[12..18].copy_from_slice(&a.mac);
    let nom = a.nom.as_bytes();
    let n = nom.len().min(32);
    o[18] = n as u8;
    o[19..19 + n].copy_from_slice(&nom[..n]);
    for s in a.sources.iter().take(NOMBRE_DE_SOURCES) {
        let debut = 52 + 17 * usize::from(s.indice);
        o[debut] = s.etat;
        let nom = s.nom.as_bytes();
        let n = nom.len().min(15);
        o[debut + 1..debut + 1 + n].copy_from_slice(&nom[..n]);
    }
    o[307] = (u8::from(a.allume) << 7) | (u8::from(a.tonalite_active) << 4);
    o[308] = (u8::from(a.aucun_flux_air) << 7)
        | ((a.source_choisie & 0x0F) << 2)
        | (u8::from(a.sourdine) << 1);
    o[310] = (a.volume_demi_db + 195).clamp(0, 255) as u8;
    o[311] = a.aigus_demi_db as u8;
    o[312] = a.graves_demi_db as u8;
    o[313] = a.balance_pour_cent as u8;
    o[314] = a.pre_out;
    o[318] = match a.frequence_entree {
        Some(44_100) => 7,
        Some(48_000) => 8,
        Some(88_200) => 10,
        Some(96_000) => 11,
        Some(176_400) => 13,
        Some(192_000) => 14,
        _ => 0,
    };
    o[331..335].copy_from_slice(&a.horloge_ms.to_be_bytes());
    o[339..343].copy_from_slice(&a.session);
    let crc = crc16(&o[..343]);
    o[343..345].copy_from_slice(&crc.to_be_bytes());
    o
}
