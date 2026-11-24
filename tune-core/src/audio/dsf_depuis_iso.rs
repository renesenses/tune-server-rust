//! #9184 — une piste d'ISO SACD servie à un renderer réseau en DSD NATIF,
//! sous la forme d'un DSF fabriqué à la volée.
//!
//! Une piste d'ISO SACD est une TRANCHE de l'image (`cue_media_path`,
//! `cue_start_ms`/`cue_end_ms`, écrites par `ecrire_les_iso_sacd`). Le
//! passthrough DSD d'une zone réseau ne sait servir qu'un fichier du disque,
//! tel quel : l'image entière, jamais. Une tranche d'ISO partait donc par le
//! décodeur DSD → PCM, même vers un renderer qui lit le DSD natif et à qui un
//! `.dsf` de la même musique part brut (fil 2957).
//!
//! Ce module sert à la place un DSF NON compressé que personne n'écrit sur
//! disque, sur le modèle exact de `dsf_depuis_dst` (#6221) :
//!
//! - **En-tête** (92 octets, `dsf_depuis_dst::entete_dsf`) : la taille est
//!   connue d'avance — la tranche compte `fin - début` trames de
//!   4 704 octets par canal.
//! - **Données** : le DSD lu dans l'image par le MÊME lecteur que la sortie
//!   locale et le DoP (`sacd::LecteurDsdSacd`, forme DSDIFF : MSB-first,
//!   entrelacé octet par octet), remis en blocs DSF de 4 096 octets par
//!   canal, bits inversés (`dsf_depuis_dst::rendre_les_blocs_dsf`).
//! - **Range** : un bloc DSF désigne une position par canal, donc une trame
//!   (`position / 4 704`) et un décalage dans la trame. Le lecteur se cale
//!   sur la trame sans lire ce qui précède.
//!
//! Limites assumées : zone STÉRÉO seulement (l'ordre des voies d'une zone
//! multicanal et celui du DSF ne sont pas vérifiés ici), DSD 64 non
//! compressé — une image DST est refusée plus haut (`refus_de_decodage`).
//! Aucun ffmpeg.
use super::dsf_depuis_dst::{ENTETE_DSF_LEN, entete_dsf, rendre_les_blocs_dsf, taille_des_donnees};
use super::sacd;
use std::path::{Path, PathBuf};

/// Octets DSD d'une trame SACD, par canal.
const OCTETS_PAR_TRAME: u64 = sacd::OCTETS_PAR_TRAME_ET_CANAL as u64;

/// Un DSF virtuel bâti sur une tranche d'image ISO SACD.
#[derive(Debug, Clone)]
pub struct DsfDepuisIso {
    chemin: PathBuf,
    zone: sacd::ZoneSacd,
    /// Trames `[debut, fin)` de la tranche, sur l'horloge de la zone.
    debut: u32,
    fin: u32,
    canaux: u16,
    cadence: u32,
    octets_par_canal: u64,
    entete: [u8; 92],
    total: u64,
}

impl DsfDepuisIso {
    /// Prépare le DSF virtuel de la tranche `[debut_ms, fin_ms)` (horloge de
    /// la zone, comme `cue_start_ms`/`cue_end_ms`) de l'image `chemin`. Ne
    /// lit que la structure du disque.
    ///
    /// `Err` (le chemin PCM reste alors celui d'avant) : image illisible ou
    /// DST, zone qui n'est pas stéréo, tranche sans fin ou vide.
    pub fn preparer(chemin: &Path, debut_ms: u64, fin_ms: Option<u64>) -> Result<Self, String> {
        let fin_ms = fin_ms.ok_or("dsf_depuis_iso: tranche sans fin, longueur inconnue")?;
        let disque = sacd::lire_disque(chemin)?;
        let zone = sacd::zone_lisible(&disque)?.clone();
        if zone.canaux != 2 {
            return Err(format!(
                "dsf_depuis_iso: zone à {} voies, seule la stéréo part en DSF",
                zone.canaux
            ));
        }
        let cadence = zone
            .frequence
            .ok_or("dsf_depuis_iso: fréquence de la zone inconnue")?;
        let debut = sacd::ms_en_trames(debut_ms);
        let fin = sacd::ms_en_trames(fin_ms);
        if fin <= debut {
            return Err(format!("dsf_depuis_iso: tranche vide ({debut}..{fin})"));
        }
        let canaux = u16::from(zone.canaux);
        let octets_par_canal = u64::from(fin - debut) * OCTETS_PAR_TRAME;
        Ok(Self {
            chemin: chemin.to_path_buf(),
            zone,
            debut,
            fin,
            canaux,
            cadence,
            octets_par_canal,
            entete: entete_dsf(canaux, cadence, octets_par_canal),
            total: ENTETE_DSF_LEN + taille_des_donnees(canaux, octets_par_canal),
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

    /// Rend les octets `[debut, debut + longueur)` du fichier virtuel
    /// (bornés à sa taille), morceau par morceau, à `rendre`. Bloquant : lit
    /// l'image. `rendre` rend `Err` pour interrompre.
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
        let c = usize::from(self.canaux);
        rendre_les_blocs_dsf(
            c,
            self.octets_par_canal,
            d0,
            d1,
            |depart| self.ouvrir_a(depart),
            |atteint| {
                tracing::warn!(
                    fichier = %self.chemin.display(),
                    octet_par_canal = atteint,
                    attendus = self.octets_par_canal,
                    "dsf_depuis_iso_trames_manquantes_completees_en_silence"
                );
            },
            rendre,
        )
    }

    /// Le DSD de la tranche (MSB-first, entrelacé), à partir de l'octet
    /// `depart` PAR CANAL.
    fn ouvrir_a(
        &self,
        depart: u64,
    ) -> Result<impl FnMut() -> Result<Option<Vec<u8>>, String> + use<>, String> {
        let c = usize::from(self.canaux);
        let trame = u64::from(self.debut) + depart / OCTETS_PAR_TRAME;
        // Octets (toutes voies) à sauter dans la première trame lue.
        let mut a_sauter = (depart % OCTETS_PAR_TRAME) as usize * c;
        let mut lecteur = if trame >= u64::from(self.fin) {
            None
        } else {
            let trame = trame as u32;
            let fichier = std::fs::File::open(&self.chemin)
                .map_err(|e| format!("dsf_depuis_iso: ouverture: {e}"))?;
            let l =
                sacd::LecteurDsdSacd::depuis_source(fichier, &self.zone, trame, Some(self.fin))?;
            match l.trame_atteinte() {
                Some(t) if t == trame => Some(l),
                // Fin de zone avant la trame : le reste est silence.
                None => None,
                Some(t) => {
                    return Err(format!(
                        "dsf_depuis_iso: lecture calée sur la trame {t} au lieu de {trame}"
                    ));
                }
            }
        };
        Ok(move || {
            let Some(l) = lecteur.as_mut() else {
                return Ok(None);
            };
            loop {
                let Some(mut m) = l.next_chunk()? else {
                    return Ok(None);
                };
                if a_sauter == 0 {
                    return Ok(Some(m));
                }
                let n = a_sauter.min(m.len());
                a_sauter -= n;
                m.drain(..n);
                if !m.is_empty() {
                    return Ok(Some(m));
                }
            }
        })
    }
}

impl crate::http::streamer::SourceVirtuelle for DsfDepuisIso {
    fn total(&self) -> u64 {
        self.total
    }
    fn lire(
        &self,
        debut: u64,
        longueur: u64,
        rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>,
    ) -> Result<(), String> {
        DsfDepuisIso::lire(self, debut, longueur, rendre)
    }
}
