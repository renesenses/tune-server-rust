//! Le flux « DSD brut » de la sortie native (#5643, lot D).
//!
//! Quand une zone réglée « Natif » sort par un pilote ASIO qui déclare le DSD
//! à la cadence du fichier, l'orchestrateur ne sert plus de DoP : il sert les
//! octets DSD eux-mêmes, dans la forme que la sortie ASIO attend
//! (`cpal::SampleFormat::DsdU8`) :
//!
//! - entrelacés **un octet par canal et par trame** (L, R, L, R…) ;
//! - **premier bit DSD dans le bit de poids fort** (MSB-first), quelle que
//!   soit la source. Le DSF range ses bits LSB-first (champ « bits per
//!   sample » = 1) : chaque octet y est inversé. Le DFF et l'ISO SACD sont
//!   déjà MSB-first : rien n'est touché.
//!
//! Le flux part par la session HTTP habituelle, précédé d'un en-tête de
//! 16 octets ([`ENTETE_LEN`]) que `play_url` reconnaît avant tout le reste :
//! la cadence DSD et le nombre de canaux y sont écrits, lus DANS LE FICHIER
//! (même règle que le DoP, #1894). Aucun renderer réseau ne reçoit ce flux :
//! seule la sortie locale ASIO le demande, et seule elle sait le lire.
//!
//! Rien ici ne dépend de la plateforme : tout se teste sous Linux.

use std::path::Path;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use tokio::sync::mpsc;

/// La signature du flux. Huit octets ASCII que ni un WAV (`RIFF`), ni un FLAC
/// (`fLaC`), ni un MP3 (`ID3`/synchro) ne peuvent porter.
pub const MAGIQUE: &[u8; 8] = b"TUNEDSD1";

/// Longueur de l'en-tête : signature, cadence (u32 LE), canaux (u16 LE),
/// réservé (u16, zéro).
pub const ENTETE_LEN: usize = 16;

/// Le type MIME de la session. Aucun lecteur extérieur ne le connaît : c'est
/// voulu, ce flux n'a qu'un destinataire.
pub const MIME: &str = "application/x-tune-dsd";

/// Le motif de repos DSD, MSB-first (même valeur que `cpal` côté ASIO).
pub const SILENCE_DSD_MSB_FIRST: u8 = 0x69;

/// Les cadences que la sortie ASIO native ouvre : DSD64, DSD128, DSD256.
/// Miroir de `vendor/cpal/src/host/asio/dsd.rs::DSD_RATES`.
pub const CADENCES_DSD_NATIVES: [u32; 3] = [2_822_400, 5_644_800, 11_289_600];

/// L'en-tête du flux pour `cadence` (Hz, en bits par seconde et par canal) et
/// `canaux`.
#[must_use]
pub fn entete(cadence: u32, canaux: u16) -> [u8; ENTETE_LEN] {
    let mut h = [0u8; ENTETE_LEN];
    h[..8].copy_from_slice(MAGIQUE);
    h[8..12].copy_from_slice(&cadence.to_le_bytes());
    h[12..14].copy_from_slice(&canaux.to_le_bytes());
    h
}

/// Ce que dit un en-tête de flux DSD brut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnteteDsdBrut {
    pub cadence: u32,
    pub canaux: u16,
}

impl EnteteDsdBrut {
    /// Durée, en millisecondes, de `octets_par_canal` octets DSD : chaque
    /// octet porte 8 échantillons d'un bit.
    #[must_use]
    pub fn ms_pour_octets_par_canal(self, octets_par_canal: u64) -> u64 {
        if self.cadence == 0 {
            return 0;
        }
        octets_par_canal.saturating_mul(8_000) / u64::from(self.cadence)
    }
}

/// Lit l'en-tête au début de `octets`. `None` si la signature n'y est pas, si
/// l'en-tête est tronqué, ou s'il annonce zéro canal ou une cadence nulle :
/// `play_url` passe alors au WAV, comme avant.
#[must_use]
pub fn lire_entete(octets: &[u8]) -> Option<EnteteDsdBrut> {
    if octets.len() < ENTETE_LEN || &octets[..8] != MAGIQUE {
        return None;
    }
    let cadence = u32::from_le_bytes([octets[8], octets[9], octets[10], octets[11]]);
    let canaux = u16::from_le_bytes([octets[12], octets[13]]);
    if cadence == 0 || canaux == 0 {
        return None;
    }
    Some(EnteteDsdBrut { cadence, canaux })
}

/// L'ordre des bits d'une source DSD, d'après son extension et, pour un DSF,
/// le champ « bits per sample » de son bloc `fmt ` : 1 = LSB-first (le cas de
/// tous les DSF rencontrés), 8 = MSB-first (prévu par la spécification Sony).
/// DFF et ISO SACD sont MSB-first.
#[must_use]
pub fn source_lsb_first(ext: &str, dsf_bits_per_sample: Option<u32>) -> bool {
    ext.eq_ignore_ascii_case("dsf") && dsf_bits_per_sample != Some(8)
}

/// Met un bloc d'octets DSD entrelacés dans l'ordre `DsdU8` (MSB-first), en
/// place.
pub fn normaliser_en_dsd_u8(bloc: &mut [u8], lsb_first: bool) {
    if lsb_first {
        for octet in bloc {
            *octet = octet.reverse_bits();
        }
    }
}

/// Où commence et où s'arrête la lecture, sur l'horloge du fichier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fenetre {
    pub debut_ms: u64,
    pub fin_ms: Option<u64>,
}

/// Octets par canal correspondant à `ms` à la cadence `cadence`.
#[must_use]
pub fn octets_par_canal_pour_ms(cadence: u32, ms: u64) -> u64 {
    (u64::from(cadence) / 8).saturating_mul(ms) / 1000
}

/// Le lecteur de blocs entrelacés d'une source, quel que soit son format.
type LecteurDeBlocs = Box<dyn FnMut() -> Result<Option<Vec<u8>>, String>>;

/// Lit `chemin` (DSF, DFF ou ISO SACD) et rend ses octets DSD bruts,
/// normalisés en `DsdU8`, bloc par bloc, à `rendre`. La première valeur de
/// retour est l'en-tête (lu DANS le fichier) ; elle est passée à `rendre`
/// avant tout octet, sous forme d'en-tête de flux.
///
/// `rendre` rend `Err` pour interrompre (consommateur parti, délai dépassé).
pub fn lire_dsd_brut(
    chemin: &str,
    ext: &str,
    fenetre: Fenetre,
    mut rendre: impl FnMut(Vec<u8>) -> Result<(), String>,
) -> Result<EnteteDsdBrut, String> {
    let ext = ext.to_ascii_lowercase();
    // Chaque lecteur rend des blocs entrelacés dont la longueur est un
    // multiple du nombre de canaux (contrat documenté de chacun) ; la position
    // atteinte par le déplacement est en octets PAR CANAL.
    let (entete_lu, lsb_first, atteint_par_canal, mut prochain): (
        EnteteDsdBrut,
        bool,
        u64,
        LecteurDeBlocs,
    ) = if ext == "dsf" {
        let info = super::dsf::parse_dsf(chemin)?;
        let e = EnteteDsdBrut {
            cadence: info.sample_rate,
            canaux: u16::try_from(info.channels).map_err(|_| "dsf: trop de canaux")?,
        };
        let lsb = source_lsb_first("dsf", Some(info.bits_per_sample));
        let mut lecteur = super::dsf::DsfStreamReader::open(chemin, info)?;
        let atteint = if fenetre.debut_ms > 0 {
            let cible = octets_par_canal_pour_ms(e.cadence, fenetre.debut_ms) as usize;
            lecteur.seek_to_bytes_per_channel(cible)? as u64
        } else {
            0
        };
        (e, lsb, atteint, Box::new(move || lecteur.next_chunk()))
    } else if ext == "iso" {
        let (cadence, canaux) = super::sacd::parametres_de_lecture(Path::new(chemin))?;
        let e = EnteteDsdBrut {
            cadence,
            canaux: u16::try_from(canaux).map_err(|_| "sacd: trop de canaux")?,
        };
        // L'image SACD s'ouvre directement sur la trame de début et s'arrête
        // d'elle-même à la trame de fin : la fenêtre est déjà appliquée.
        let mut lecteur =
            super::sacd::ouvrir_lecture(Path::new(chemin), fenetre.debut_ms, fenetre.fin_ms)?;
        let atteint = lecteur.trame_atteinte().map_or(0, |t| {
            octets_par_canal_pour_ms(e.cadence, super::sacd::trames_en_ms(t))
        });
        (e, false, atteint, Box::new(move || lecteur.next_chunk()))
    } else {
        let info = super::dff::parse_dff(chemin)?;
        let canaux = info.channels as usize;
        if canaux == 0 {
            return Err("dff: zéro canal".into());
        }
        let e = EnteteDsdBrut {
            cadence: info.sample_rate,
            canaux: u16::try_from(info.channels).map_err(|_| "dff: trop de canaux")?,
        };
        let morceau = 32768 / canaux * canaux;
        let mut lecteur = super::dff::DffStreamReader::open(chemin, &info, morceau)?;
        let atteint = if fenetre.debut_ms > 0 {
            let cible = octets_par_canal_pour_ms(e.cadence, fenetre.debut_ms) as usize * canaux;
            (lecteur.seek_to_interleaved_byte(cible, canaux)? / canaux) as u64
        } else {
            0
        };
        (e, false, atteint, Box::new(move || lecteur.next_chunk()))
    };

    rendre(entete(entete_lu.cadence, entete_lu.canaux).to_vec())?;

    // La fin de fenêtre pour DSF et DFF (l'ISO l'applique lui-même) : un
    // nombre d'octets par canal à ne pas dépasser, compté depuis la position
    // atteinte.
    let canaux = usize::from(entete_lu.canaux);
    let mut reste_par_canal: Option<u64> = match (ext.as_str(), fenetre.fin_ms) {
        ("iso", _) | (_, None) => None,
        (_, Some(fin)) => {
            Some(octets_par_canal_pour_ms(entete_lu.cadence, fin).saturating_sub(atteint_par_canal))
        }
    };
    while let Some(mut bloc) = prochain()? {
        if let Some(reste) = reste_par_canal.as_mut() {
            let max = (*reste as usize).saturating_mul(canaux);
            if bloc.len() > max {
                bloc.truncate(max);
            }
            *reste -= (bloc.len() / canaux) as u64;
        }
        // Le contrat des lecteurs : des trames entières. Une queue partielle
        // déphaserait les canaux à la sortie ; elle est retirée, et dite.
        let entier = bloc.len() - bloc.len() % canaux;
        if entier != bloc.len() {
            tracing::warn!(
                octets = bloc.len(),
                canaux,
                "dsd_brut_bloc_non_aligne_queue_retiree"
            );
            bloc.truncate(entier);
        }
        if bloc.is_empty() {
            if reste_par_canal == Some(0) {
                break;
            }
            continue;
        }
        normaliser_en_dsd_u8(&mut bloc, lsb_first);
        rendre(bloc)?;
        if reste_par_canal == Some(0) {
            break;
        }
    }
    Ok(entete_lu)
}

/// La même lecture, poussée dans le canal d'une session HTTP (le pendant de
/// `decode_dsd_to_dop_streaming`). `data_ready` est notifié au premier bloc
/// (l'en-tête).
pub fn diffuser_dsd_brut(
    chemin: &str,
    ext: &str,
    fenetre: Fenetre,
    tx: mpsc::Sender<Vec<u8>>,
    data_ready: &std::sync::Arc<tokio::sync::Notify>,
    rt: &tokio::runtime::Handle,
    delai: std::time::Duration,
) -> Result<EnteteDsdBrut, String> {
    let mut premier = true;
    lire_dsd_brut(chemin, ext, fenetre, |bloc| {
        match rt.block_on(tokio::time::timeout(delai, tx.send(bloc))) {
            Ok(Ok(())) => {
                if premier {
                    premier = false;
                    data_ready.notify_one();
                }
                Ok(())
            }
            Ok(Err(_)) => Err("dsd_brut_consumer_closed".into()),
            Err(_) => Err("dsd_brut_send_timeout".into()),
        }
    })
}

/// Anneau SPSC d'octets DSD entre le fil d'alimentation et le rappel ASIO.
///
/// Cases atomiques plutôt que `UnsafeCell` : aucun `unsafe` ici, et un
/// `AtomicU8` en ordre relâché est un simple accès mémoire sur x86. Les deux
/// curseurs (`ecrit`, `lu`) ordonnent la publication (Release/Acquire).
///
/// Le producteur n'écrit que des TRAMES entières et le rappel lit des
/// tampons de trames entières : le contenu reste aligné sur les canaux.
pub struct AnneauDsd {
    cases: Box<[AtomicU8]>,
    ecrit: AtomicU64,
    lu: AtomicU64,
}

impl AnneauDsd {
    /// Un anneau de `contenance` octets (au moins un).
    #[must_use]
    pub fn new(contenance: usize) -> Self {
        Self {
            cases: (0..contenance.max(1)).map(|_| AtomicU8::new(0)).collect(),
            ecrit: AtomicU64::new(0),
            lu: AtomicU64::new(0),
        }
    }

    /// La contenance de deux secondes de DSD à `cadence` sur `canaux`,
    /// arrondie à une trame entière.
    #[must_use]
    pub fn contenance_deux_secondes(cadence: u32, canaux: u16) -> usize {
        let canaux = usize::from(canaux.max(1));
        (cadence as usize / 8) * 2 * canaux
    }

    pub fn contenance(&self) -> usize {
        self.cases.len()
    }

    pub fn disponible(&self) -> usize {
        let w = self.ecrit.load(Ordering::Acquire);
        let r = self.lu.load(Ordering::Acquire);
        w.wrapping_sub(r) as usize
    }

    pub fn libre(&self) -> usize {
        self.contenance() - self.disponible()
    }

    /// Écrit autant que possible de `octets` ; rend le nombre écrit. Le
    /// producteur seul l'appelle.
    pub fn pousser(&self, octets: &[u8]) -> usize {
        let n = octets.len().min(self.libre());
        let w = self.ecrit.load(Ordering::Relaxed);
        let cap = self.contenance() as u64;
        for (i, octet) in octets[..n].iter().enumerate() {
            let pos = ((w + i as u64) % cap) as usize;
            self.cases[pos].store(*octet, Ordering::Relaxed);
        }
        self.ecrit.store(w + n as u64, Ordering::Release);
        n
    }

    /// Comme [`Self::pousser`], mais seulement des trames de `canaux` octets
    /// entières : la place libre est arrondie à la trame inférieure, et
    /// `octets` doit lui-même être fait de trames entières.
    pub fn pousser_trames(&self, octets: &[u8], canaux: usize) -> usize {
        let canaux = canaux.max(1);
        let libre = self.libre() - self.libre() % canaux;
        let n = octets.len().min(libre);
        self.pousser(&octets[..n - n % canaux])
    }

    /// Lit autant que possible dans `dst` ; rend le nombre lu. Le rappel seul
    /// l'appelle.
    pub fn tirer(&self, dst: &mut [u8]) -> usize {
        let n = dst.len().min(self.disponible());
        let r = self.lu.load(Ordering::Relaxed);
        let cap = self.contenance() as u64;
        for (i, case) in dst[..n].iter_mut().enumerate() {
            let pos = ((r + i as u64) % cap) as usize;
            *case = self.cases[pos].load(Ordering::Relaxed);
        }
        self.lu.store(r + n as u64, Ordering::Release);
        n
    }
}

/// Ce que le rappel ASIO fait d'un tampon `DsdU8` : en pause, le motif de
/// repos ; sinon les octets de l'anneau, complétés par le motif de repos en
/// cas de famine. Rend `true` s'il y a eu famine (tampon non rempli).
///
/// Aucun volume, aucun ReplayGain, aucun DSP : le DSD natif sort intouché.
pub fn remplir_tampon_dsd(anneau: &AnneauDsd, en_pause: bool, tampon: &mut [u8]) -> bool {
    if en_pause {
        tampon.fill(SILENCE_DSD_MSB_FIRST);
        return false;
    }
    let lus = anneau.tirer(tampon);
    tampon[lus..].fill(SILENCE_DSD_MSB_FIRST);
    lus < tampon.len()
}
