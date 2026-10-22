//! #6221 — un DSDIFF compressé DST servi à un renderer réseau en DSD NATIF,
//! sous la forme d'un DSF fabriqué à la volée.
//!
//! Le passthrough DSD d'une zone réseau sert le fichier sur disque tel quel
//! (`servir_en_passthrough`). Un `.dff` DST ne peut pas partir ainsi : ses
//! trames sont un code entropique, un renderer qui les lirait comme des bits
//! DSD rendrait du bruit (#4378). Jusqu'ici il passait donc par le décodeur
//! DSD → PCM (WAV 176,4 kHz pour un DSD64), même vers un renderer qui lit le
//! DSD natif.
//!
//! Ce module sert à la place un DSF NON compressé, que personne n'écrit sur
//! disque : un fichier VIRTUEL dont chaque octet se calcule à la demande.
//!
//! - **En-tête** (92 octets, [`entete_dsf`]) : sa taille totale est connue
//!   d'avance, car le DST décode chaque trame en une quantité FIXE de DSD
//!   (`DstDecoder::dsd_frame_bytes`) et `FRTE` donne le nombre de trames.
//!   `Content-Length` et `res@size` sont donc exacts dès la première requête.
//! - **Données** : le DSD décodé par le MÊME lecteur que la sortie USB/ASIO
//!   (`dff::DffStreamReader`, vérifié bit à bit contre libdstdec), remis dans
//!   la forme DSF de la spécification Sony (« DSF File Format Specification »
//!   1.01) : blocs de 4 096 octets PAR CANAL, entrelacés bloc par bloc
//!   (`[G 4096][D 4096][G 4096]…`), bits LSB-first (champ « bits per
//!   sample » = 1 : chaque octet du DSDIFF, MSB-first, est inversé). Le
//!   dernier bloc est complété de zéros, comme le veut la spécification ; le
//!   champ « sample count » dit le nombre réel d'échantillons.
//! - **Range** : une requête `bytes=N-M` désigne un bloc entier, dont la
//!   position par canal (`bloc × 4096`) désigne à l'octet près une position
//!   du flux décodé. `DffStreamReader::seek_to_interleaved_byte` y va sans
//!   décoder les trames précédentes (une trame DST se décode seule) : un
//!   déplacement au milieu du morceau ne coûte que la trame d'arrivée.
//!
//! Aucun ffmpeg : seul le décodeur DST en Rust du dépôt (feature `dst`).
use super::dff;

/// Longueur de l'en-tête DSF : bloc `DSD ` (28) + bloc `fmt ` (52) + en-tête
/// du bloc `data` (12).
pub const ENTETE_DSF_LEN: u64 = 92;
/// Taille de bloc par canal imposée par la spécification DSF.
pub const BLOC_DSF: usize = 4096;
/// Le type MIME de repli quand le renderer n'en a annoncé aucun — le même que
/// le passthrough d'un `.dsf` sur disque (`servir_en_passthrough`).
pub const MIME_DSF_PAR_DEFAUT: &str = "application/x-dsd";
/// Erreur rendue par `rendre` quand le consommateur HTTP est parti : arrêt
/// normal, pas une panne.
pub const CONSOMMATEUR_PARTI: &str = "dsf_depuis_dst: consommateur parti";

/// Le champ « channel type » du bloc `fmt ` pour `canaux` voies, ou `None` si
/// le DSF ne sait pas le dire (au-delà de 6 voies).
///
/// Spécification : 1 mono, 2 stéréo, 3 trois voies, 4 quad, 5 quatre voies,
/// 6 cinq voies, 7 5.1. Un SACD multicanal porte 5 ou 6 voies.
#[must_use]
pub fn type_de_canaux_dsf(canaux: u16) -> Option<u32> {
    match canaux {
        1 => Some(1),
        2 => Some(2),
        3 => Some(3),
        4 => Some(4),
        5 => Some(6),
        6 => Some(7),
        _ => None,
    }
}

/// Nombre de blocs de 4 096 octets par canal pour `octets_par_canal`.
#[must_use]
pub fn blocs_par_canal(octets_par_canal: u64) -> u64 {
    octets_par_canal.div_ceil(BLOC_DSF as u64)
}

/// Taille du bloc de données (sans son en-tête de 12 octets).
#[must_use]
pub fn taille_des_donnees(canaux: u16, octets_par_canal: u64) -> u64 {
    blocs_par_canal(octets_par_canal) * BLOC_DSF as u64 * u64::from(canaux)
}

/// L'en-tête DSF (92 octets, petit-boutiste) d'un fichier sans métadonnées
/// ID3 (pointeur de métadonnées à 0).
///
/// `octets_par_canal` : octets DSD par canal (8 échantillons d'un bit par
/// octet) ; « sample count » vaut `octets_par_canal × 8`.
#[must_use]
pub fn entete_dsf(canaux: u16, cadence: u32, octets_par_canal: u64) -> [u8; 92] {
    let donnees = taille_des_donnees(canaux, octets_par_canal);
    let total = ENTETE_DSF_LEN + donnees;
    let mut h = [0u8; 92];
    // Bloc « DSD ».
    h[0..4].copy_from_slice(b"DSD ");
    h[4..12].copy_from_slice(&28u64.to_le_bytes());
    h[12..20].copy_from_slice(&total.to_le_bytes());
    h[20..28].copy_from_slice(&0u64.to_le_bytes());
    // Bloc « fmt ».
    h[28..32].copy_from_slice(b"fmt ");
    h[32..40].copy_from_slice(&52u64.to_le_bytes());
    h[40..44].copy_from_slice(&1u32.to_le_bytes()); // format version
    h[44..48].copy_from_slice(&0u32.to_le_bytes()); // format id : DSD raw
    h[48..52].copy_from_slice(&type_de_canaux_dsf(canaux).unwrap_or(0).to_le_bytes());
    h[52..56].copy_from_slice(&u32::from(canaux).to_le_bytes());
    h[56..60].copy_from_slice(&cadence.to_le_bytes());
    h[60..64].copy_from_slice(&1u32.to_le_bytes()); // bits per sample : LSB-first
    h[64..72].copy_from_slice(&(octets_par_canal * 8).to_le_bytes());
    h[72..76].copy_from_slice(&(BLOC_DSF as u32).to_le_bytes());
    h[76..80].copy_from_slice(&0u32.to_le_bytes());
    // En-tête du bloc « data ».
    h[80..84].copy_from_slice(b"data");
    h[84..92].copy_from_slice(&(12 + donnees).to_le_bytes());
    h
}

/// Un DSF virtuel bâti sur un `.dff` DST.
#[derive(Debug, Clone)]
pub struct DsfDepuisDst {
    chemin: String,
    canaux: u16,
    cadence: u32,
    octets_par_canal: u64,
    entete: [u8; 92],
    total: u64,
}

impl DsfDepuisDst {
    /// Prépare le DSF virtuel de `chemin` : lit l'en-tête DSDIFF, vérifie
    /// que c'est bien du DST que ce binaire sait décoder EN TEMPS RÉEL, et
    /// calcule la taille. Ne décode rien.
    ///
    /// `Err` (le chemin PCM reste alors celui d'avant) : pas un DFF DST,
    /// feature `dst` absente, plus de 6 voies, cadence que ce processeur ne
    /// tient pas, nombre de trames inconnu.
    pub fn preparer(chemin: &str) -> Result<Self, String> {
        let info = dff::parse_dff(chemin)?;
        if !info.is_dst() {
            return Err("dsf_depuis_dst: le DSDIFF n'est pas compressé DST".into());
        }
        let canaux = u16::try_from(info.channels)
            .ok()
            .filter(|c| type_de_canaux_dsf(*c).is_some())
            .ok_or_else(|| {
                format!(
                    "dsf_depuis_dst: {} voies, le DSF n'en porte que 1 à 6",
                    info.channels
                )
            })?;
        let trames = u64::from(
            info.dst_frames
                .filter(|n| *n > 0)
                .ok_or("dsf_depuis_dst: FRTE absent, durée inconnue")?,
        );
        let octets_par_trame = octets_par_trame_decodee(info.sample_rate, info.channels)?;
        let octets_par_canal = trames * octets_par_trame / u64::from(canaux);
        let entete = entete_dsf(canaux, info.sample_rate, octets_par_canal);
        Ok(Self {
            chemin: chemin.to_string(),
            canaux,
            cadence: info.sample_rate,
            octets_par_canal,
            total: ENTETE_DSF_LEN + taille_des_donnees(canaux, octets_par_canal),
            entete,
        })
    }

    /// Taille du fichier virtuel, en-tête compris.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    /// L'en-tête DSF servi en tête du fichier.
    #[must_use]
    pub fn entete(&self) -> &[u8; 92] {
        &self.entete
    }

    #[must_use]
    pub fn canaux(&self) -> u16 {
        self.canaux
    }

    #[must_use]
    pub fn cadence(&self) -> u32 {
        self.cadence
    }

    #[must_use]
    pub fn octets_par_canal(&self) -> u64 {
        self.octets_par_canal
    }

    /// Durée annoncée, en millisecondes.
    #[must_use]
    pub fn duree_ms(&self) -> u64 {
        if self.cadence == 0 {
            return 0;
        }
        self.octets_par_canal * 8_000 / u64::from(self.cadence)
    }

    /// Rend les octets `[debut, debut + longueur)` du fichier virtuel
    /// (bornés à sa taille), morceau par morceau, à `rendre`. Bloquant :
    /// décode du DST. `rendre` rend `Err` pour interrompre.
    pub fn lire(
        &self,
        debut: u64,
        longueur: u64,
        rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>,
    ) -> Result<(), String> {
        let fin = debut.saturating_add(longueur).min(self.total);
        if debut >= fin {
            return Ok(());
        }
        if debut < ENTETE_DSF_LEN {
            let a = debut as usize;
            let b = fin.min(ENTETE_DSF_LEN) as usize;
            rendre(self.entete[a..b].to_vec())?;
        }
        if fin <= ENTETE_DSF_LEN {
            return Ok(());
        }
        let d0 = debut.max(ENTETE_DSF_LEN) - ENTETE_DSF_LEN;
        let d1 = fin - ENTETE_DSF_LEN;
        self.lire_les_donnees(d0, d1, rendre)
    }

    /// Les octets `[d0, d1)` du bloc de données.
    fn lire_les_donnees(
        &self,
        d0: u64,
        d1: u64,
        rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>,
    ) -> Result<(), String> {
        let c = usize::from(self.canaux);
        let super_bloc = (BLOC_DSF * c) as u64;
        let premier_bloc = d0 / super_bloc;
        let mut source = SourceDsdMsb::ouvrir(&self.chemin, c, premier_bloc * BLOC_DSF as u64)?;
        // Position (dans les données) du super-bloc courant, et position PAR
        // CANAL de son premier octet.
        let mut pos = premier_bloc * super_bloc;
        let mut par_canal = premier_bloc * BLOC_DSF as u64;
        let mut en_attente: Vec<u8> = Vec::new();
        let mut silence_rendu = false;
        while pos < d1 {
            // Octets utiles de ce bloc, par canal (le dernier est partiel).
            let utiles = self
                .octets_par_canal
                .saturating_sub(par_canal)
                .min(BLOC_DSF as u64) as usize;
            while en_attente.len() < utiles * c {
                match source.suivant()? {
                    Some(m) => en_attente.extend_from_slice(&m),
                    None => break,
                }
            }
            let disponibles = (en_attente.len() / c).min(utiles);
            if disponibles < utiles && !silence_rendu {
                // Le fichier porte moins de trames que FRTE n'en annonce :
                // la taille promise au renderer est tenue par du silence.
                silence_rendu = true;
                tracing::warn!(
                    fichier = %self.chemin,
                    octet_par_canal = par_canal + disponibles as u64,
                    attendus = self.octets_par_canal,
                    "dsf_depuis_dst_trames_manquantes_completees_en_silence"
                );
            }
            let mut bloc = vec![0u8; BLOC_DSF * c];
            for i in 0..utiles {
                for canal in 0..c {
                    let msb = if i < disponibles {
                        en_attente[i * c + canal]
                    } else {
                        dff::SILENCE_DSD
                    };
                    bloc[canal * BLOC_DSF + i] = msb.reverse_bits();
                }
            }
            en_attente.drain(..disponibles * c);
            let a = d0.saturating_sub(pos) as usize;
            let b = (d1 - pos).min(super_bloc) as usize;
            bloc.truncate(b);
            if a > 0 {
                bloc.drain(..a);
            }
            rendre(bloc)?;
            pos += super_bloc;
            par_canal += BLOC_DSF as u64;
        }
        Ok(())
    }
}

impl crate::http::streamer::SourceVirtuelle for DsfDepuisDst {
    fn total(&self) -> u64 {
        self.total
    }
    fn lire(
        &self,
        debut: u64,
        longueur: u64,
        rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>,
    ) -> Result<(), String> {
        DsfDepuisDst::lire(self, debut, longueur, rendre)
    }
}

/// Octets DSD décodés (toutes voies) d'une trame DST — la valeur même que le
/// lecteur emploie (`DstDecoder::dsd_frame_bytes`), avec le même refus temps
/// réel.
#[cfg(feature = "dst")]
fn octets_par_trame_decodee(cadence: u32, canaux: u32) -> Result<u64, String> {
    if let Some(motif) = dff::refus_dst_hors_temps_reel(cadence, canaux) {
        return Err(motif);
    }
    let decodeur = dst_decoder::decoder::DstDecoder::new(canaux as usize, cadence as usize)
        .map_err(|e| {
            format!("dsf_depuis_dst: DST illisible à {cadence} Hz × {canaux} voies : {e}")
        })?;
    Ok(decodeur.dsd_frame_bytes() as u64)
}

#[cfg(not(feature = "dst"))]
fn octets_par_trame_decodee(_cadence: u32, _canaux: u32) -> Result<u64, String> {
    Err("dsf_depuis_dst: binaire sans la feature `dst`, le DST ne se décode pas".into())
}

/// Le DSD décodé, MSB-first, entrelacé octet par octet (forme DSDIFF), à
/// partir d'une position par canal.
struct SourceDsdMsb {
    lecteur: Option<dff::DffStreamReader>,
}

impl SourceDsdMsb {
    fn ouvrir(chemin: &str, canaux: usize, depart_par_canal: u64) -> Result<Self, String> {
        let info = dff::parse_dff(chemin)?;
        let mut lecteur = dff::DffStreamReader::open(chemin, &info, 32768 / canaux * canaux)?;
        if depart_par_canal > 0 {
            let cible = depart_par_canal as usize * canaux;
            let atteint = lecteur.seek_to_interleaved_byte(cible, canaux)?;
            if atteint != cible {
                // Au-delà de la dernière trame réelle : tout le reste est
                // silence (voir `lire_les_donnees`).
                return Ok(Self { lecteur: None });
            }
        }
        Ok(Self {
            lecteur: Some(lecteur),
        })
    }

    fn suivant(&mut self) -> Result<Option<Vec<u8>>, String> {
        match self.lecteur.as_mut() {
            Some(l) => l.next_chunk(),
            None => Ok(None),
        }
    }
}
