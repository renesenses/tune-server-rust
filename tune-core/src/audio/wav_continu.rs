//! #8082 — un WAV CONTINU fait de plusieurs pistes de même format.
//!
//! L'Eversolo DMP-A8 (lecteur FFmpeg 4.3, une connexion et un démultiplexeur
//! par URL) raccorde mal deux pistes enchaînées par `SetNext` : la jointure
//! d'un album sans silence (The Wall 1→2) reste audible alors que Tune livre
//! des échantillons exacts (#8076). Servir la suite de pistes en UN seul flux
//! retire la jointure au renderer : il n'en a plus aucune à faire.
//!
//! Le fichier virtuel est un en-tête RIFF de 44 octets puis, bout à bout, le
//! PCM de chaque piste, décodé sans aucun traitement (ni gain, ni
//! rééchantillonnage, ni changement de profondeur : PURE respecté). Le nombre
//! total de trames est EXACTEMENT la somme des trames annoncées par chaque
//! conteneur ; une piste qui en rend moins est complétée de silence, une qui
//! en rend plus est coupée, pour que `Content-Length` ne mente jamais.
//!
//! Chaque tranche se calcule à la demande ([`SourceVirtuelle`]) : une requête
//! `Range` au milieu de la troisième piste ouvre la troisième piste, cherche la
//! trame voulue et décode de là. C'est ce qui rend le déplacement possible.

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S16LE, CODEC_ID_PCM_S24BE,
    CODEC_ID_PCM_S24LE, CODEC_ID_PCM_S32BE, CODEC_ID_PCM_S32LE,
};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Timestamp;

use crate::http::streamer::SourceVirtuelle;

/// Taille de l'en-tête WAV servi.
pub const EN_TETE: u64 = 44;

/// Taille des morceaux rendus au consommateur.
const MORCEAU: usize = 64 * 1024;

/// Le format PCM d'une piste, tel que son conteneur le déclare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatPcm {
    pub cadence: u32,
    /// Profondeur du conteneur PCM servi (16, 24 ou 32).
    pub profondeur: u16,
    pub canaux: u16,
}

impl FormatPcm {
    pub fn octets_par_trame(&self) -> u64 {
        u64::from(self.canaux) * u64::from(self.profondeur / 8)
    }
}

/// Ce qu'une sonde rend d'une piste candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PisteSondee {
    pub format: FormatPcm,
    /// Nombre de trames annoncé par le conteneur (STREAMINFO pour un FLAC).
    pub trames: u64,
}

/// Une piste du flux continu : le fichier, la première trame servie (non nulle
/// seulement pour la première piste d'un flux relancé à une position) et le
/// nombre de trames servies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub fichier: PathBuf,
    pub depart: u64,
    pub trames: u64,
}

fn ouvrir(chemin: &Path) -> Result<Box<dyn FormatReader>, String> {
    let fichier = File::open(chemin).map_err(|e| format!("ouverture: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(fichier), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = chemin.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("sonde: {e}"))
}

/// Sonde une piste : `Err` quand elle ne peut PAS entrer dans un flux continu
/// (codec avec perte ou flottant, longueur inconnue, format illisible). Le
/// motif est rendu pour le journal.
pub fn sonder(chemin: &Path) -> Result<PisteSondee, String> {
    let format = ouvrir(chemin)?;
    let piste = format
        .default_track(TrackType::Audio)
        .ok_or("aucune piste audio")?;
    let params = match &piste.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => return Err("pas de paramètres audio".into()),
    };
    let sans_perte_entier = [
        CODEC_ID_FLAC,
        CODEC_ID_ALAC,
        CODEC_ID_PCM_S16LE,
        CODEC_ID_PCM_S16BE,
        CODEC_ID_PCM_S24LE,
        CODEC_ID_PCM_S24BE,
        CODEC_ID_PCM_S32LE,
        CODEC_ID_PCM_S32BE,
    ];
    if !sans_perte_entier.contains(&params.codec) {
        return Err(format!("codec non admis: {:?}", params.codec));
    }
    let trames = piste
        .num_frames
        .filter(|n| *n > 0)
        .ok_or("nombre de trames inconnu")?;
    let cadence = params
        .sample_rate
        .filter(|c| *c > 0)
        .ok_or("cadence inconnue")?;
    let canaux = params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .filter(|c| *c > 0)
        .ok_or("canaux inconnus")?;
    let profondeur = super::decode::container_bit_depth(super::decode::resolve_bit_depth(&params));
    Ok(PisteSondee {
        format: FormatPcm {
            cadence,
            profondeur,
            canaux,
        },
        trames,
    })
}

/// Le fichier virtuel.
#[derive(Debug)]
pub struct WavContinu {
    format: FormatPcm,
    segments: Vec<Segment>,
    /// Première trame (dans le flux) de chaque segment.
    debuts: Vec<u64>,
    entete: [u8; 44],
    total: u64,
}

impl WavContinu {
    /// Assemble le fichier virtuel. `Err` si la suite est vide ou si son PCM
    /// ne tient pas sous un en-tête RIFF (4 Gio).
    pub fn nouveau(format: FormatPcm, segments: Vec<Segment>) -> Result<Self, String> {
        if segments.is_empty() {
            return Err("aucun segment".into());
        }
        let bpf = format.octets_par_trame();
        if bpf == 0 || format.cadence == 0 {
            return Err("format PCM invalide".into());
        }
        let mut debuts = Vec::with_capacity(segments.len());
        let mut cumul = 0u64;
        for s in &segments {
            debuts.push(cumul);
            cumul = cumul.saturating_add(s.trames);
        }
        let data = cumul.saturating_mul(bpf);
        if data > super::wav::PLAFOND_DATA_RIFF {
            return Err(format!("PCM de {data} octets : au-delà du plafond RIFF"));
        }
        let entete = super::wav::build_wav_header_with_data_size(
            format.canaux,
            format.cadence,
            format.profondeur,
            data as u32,
        );
        Ok(Self {
            format,
            segments,
            debuts,
            entete,
            total: EN_TETE + data,
        })
    }

    /// Le plus grand nombre de trames qu'une suite de ce format peut porter.
    pub fn trames_max(format: &FormatPcm) -> u64 {
        super::wav::PLAFOND_DATA_RIFF / format.octets_par_trame().max(1)
    }

    pub fn format(&self) -> FormatPcm {
        self.format
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Durée servie de chaque segment, en millisecondes (arrondi au plus
    /// proche) — et la frontière de chacun dans le flux, en millisecondes.
    pub fn bornes_ms(&self) -> Vec<(u64, u64)> {
        let ms = |trames: u64| {
            trames
                .saturating_mul(1000)
                .saturating_add(u64::from(self.format.cadence) / 2)
                / u64::from(self.format.cadence)
        };
        self.segments
            .iter()
            .zip(&self.debuts)
            .map(|(s, d)| (ms(*d), ms(s.trames)))
            .collect()
    }

    /// Décode `trames` trames du segment `i` à partir de sa trame locale
    /// `depuis`, en sautant les `sauter` premiers octets, et rend au plus
    /// `budget` octets. Complète de silence une piste plus courte que
    /// annoncé. Rend le nombre d'octets rendus.
    fn lire_segment(
        &self,
        i: usize,
        depuis: u64,
        mut sauter: u64,
        budget: u64,
        rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>,
    ) -> Result<u64, String> {
        let seg = &self.segments[i];
        let bpf = self.format.octets_par_trame();
        let largeur = usize::from(self.format.profondeur / 8);
        let decalage = 32 - u32::from(self.format.profondeur);
        // Octets que ce segment doit encore produire, saut compris.
        let a_produire = (seg.trames - depuis)
            .saturating_mul(bpf)
            .min(budget.saturating_add(sauter));
        let mut produits = 0u64;
        let mut rendus = 0u64;
        let mut tampon: Vec<u8> = Vec::with_capacity(MORCEAU + 8192);
        // Pousse des octets PCM : retire le saut, borne au budget, rend par
        // morceaux.
        let pousser = |octets: &[u8],
                       produits: &mut u64,
                       sauter: &mut u64,
                       tampon: &mut Vec<u8>,
                       rendus: &mut u64,
                       rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>|
         -> Result<(), String> {
            let reste = a_produire - *produits;
            let octets = &octets[..(octets.len() as u64).min(reste) as usize];
            *produits += octets.len() as u64;
            let saut = (*sauter).min(octets.len() as u64) as usize;
            *sauter -= saut as u64;
            tampon.extend_from_slice(&octets[saut..]);
            if tampon.len() >= MORCEAU {
                *rendus += tampon.len() as u64;
                rendre(std::mem::take(tampon))?;
            }
            Ok(())
        };

        let mut format = ouvrir(&seg.fichier)?;
        let piste = format
            .default_track(TrackType::Audio)
            .ok_or("aucune piste audio")?;
        let id = piste.id;
        let params = match &piste.codec_params {
            Some(CodecParameters::Audio(p)) => p.clone(),
            _ => return Err("pas de paramètres audio".into()),
        };
        let mut decodeur = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| format!("décodeur: {e}"))?;
        let canaux = usize::from(self.format.canaux);
        let premiere = seg.depart + depuis;
        let mut a_jeter: u64 = 0;
        if premiere > 0 {
            let cherche = format
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Timestamp {
                        ts: Timestamp::new(premiere as i64),
                        track_id: id,
                    },
                )
                .map_err(|e| format!("recherche: {e}"))?;
            a_jeter = (cherche.required_ts.get() - cherche.actual_ts.get()).max(0) as u64;
            decodeur.reset();
        }
        let mut echantillons: Vec<i32> = Vec::new();
        let mut octets: Vec<u8> = Vec::new();
        while produits < a_produire {
            let paquet = match format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => break,
                Err(symphonia::core::errors::Error::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(e) => {
                    tracing::warn!(fichier = %seg.fichier.display(), error = %e, "flux_continu_paquet_illisible");
                    break;
                }
            };
            if paquet.track_id != id {
                continue;
            }
            let decode = match decodeur.decode(&paquet) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(fichier = %seg.fichier.display(), error = %e, "flux_continu_trame_illisible");
                    continue;
                }
            };
            echantillons.clear();
            decode.copy_to_vec_interleaved::<i32>(&mut echantillons);
            let mut debut = 0usize;
            if a_jeter > 0 {
                let n = (a_jeter as usize)
                    .saturating_mul(canaux)
                    .min(echantillons.len());
                debut = n;
                a_jeter -= (n / canaux.max(1)) as u64;
            }
            octets.clear();
            octets.reserve((echantillons.len() - debut) * largeur);
            for &v in &echantillons[debut..] {
                let v = (v >> decalage).to_le_bytes();
                octets.extend_from_slice(&v[..largeur]);
            }
            pousser(
                &octets,
                &mut produits,
                &mut sauter,
                &mut tampon,
                &mut rendus,
                rendre,
            )?;
        }
        // Piste plus courte que son conteneur ne l'annonce : du silence, pour
        // que le flux garde la longueur promise.
        if produits < a_produire {
            let manque = a_produire - produits;
            tracing::warn!(
                fichier = %seg.fichier.display(),
                octets_manquants = manque,
                "flux_continu_piste_plus_courte_que_annoncee_completee_de_silence"
            );
            let zero = vec![0u8; MORCEAU];
            while produits < a_produire {
                let n = (a_produire - produits).min(MORCEAU as u64) as usize;
                pousser(
                    &zero[..n],
                    &mut produits,
                    &mut sauter,
                    &mut tampon,
                    &mut rendus,
                    rendre,
                )?;
            }
        }
        if !tampon.is_empty() {
            rendus += tampon.len() as u64;
            rendre(tampon)?;
        }
        Ok(rendus)
    }
}

impl SourceVirtuelle for WavContinu {
    fn total(&self) -> u64 {
        self.total
    }

    fn lire(
        &self,
        debut: u64,
        longueur: u64,
        rendre: &mut dyn FnMut(Vec<u8>) -> Result<(), String>,
    ) -> Result<(), String> {
        let fin = debut.saturating_add(longueur).min(self.total);
        let mut pos = debut;
        if pos < EN_TETE && pos < fin {
            let bout = fin.min(EN_TETE);
            rendre(self.entete[pos as usize..bout as usize].to_vec())?;
            pos = bout;
        }
        let bpf = self.format.octets_par_trame();
        while pos < fin {
            let decalage = pos - EN_TETE;
            let trame = decalage / bpf;
            let dans_la_trame = decalage % bpf;
            // Le segment qui porte cette trame.
            let i = match self.debuts.binary_search(&trame) {
                Ok(i) => i,
                Err(i) => i - 1,
            };
            // Segments vides : on avance jusqu'au premier qui porte la trame.
            let mut i = i;
            while i + 1 < self.segments.len() && self.debuts[i + 1] <= trame {
                i += 1;
            }
            let local = trame - self.debuts[i];
            let rendus = self.lire_segment(i, local, dans_la_trame, fin - pos, rendre)?;
            if rendus == 0 {
                break;
            }
            pos += rendus;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "wav_continu_tests_8082.rs"]
mod tests;
