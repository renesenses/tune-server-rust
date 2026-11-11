//! #8228 — décodage Opus AU FIL du téléchargement.
//!
//! balekutu (fil 2724, rc11) : sur une zone DLNA, une piste YouTube Music
//! attendait plus d'une minute avant le Play. Le bras de pré-transcodage
//! (`pretranscoder_en_flac`) téléchargeait le fichier Opus/WebM ENTIER, puis
//! le décodait ENTIER (`decode_to_pcm`), avant le premier octet du canal WAV.
//! Or `googlevideo` ne débite qu'environ deux fois le temps réel : l'attente
//! valait la moitié de la durée du titre (190 s pour 6 min 19 s).
//!
//! Ici, le décodeur lit le fichier temporaire PENDANT qu'il se remplit
//! ([`GrowingFileSource`] bloque à la frontière d'écriture au lieu de rendre
//! une fin prématurée), et pousse chaque paquet décodé dans le canal. Le
//! téléchargement allant plus vite que la lecture, le canal ne se vide pas.
//!
//! Contrat de sortie identique au chemin intégral : 48 kHz (Opus décode
//! toujours à 48 kHz), canaux et profondeur de la cible. L'appelant ne
//! prend ce chemin que si la cible est 48 kHz (ou libre) et au plus stéréo ;
//! sinon il attend la fin du téléchargement et garde le chemin d'avant.

use std::sync::Arc;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{SEND_TIMEOUT_SECS, convert_pcm_bit_depth, frame_aligned_chunk_len};
use crate::audio::dash_growth::{DashGrowth, GrowingFileSource};

/// Cadence de sortie de libopus.
const CADENCE_OPUS: u32 = 48_000;

/// Ce chemin sait-il servir cette cible sans rééchantillonner ?
pub(super) fn cible_servie(
    target_sample_rate: Option<u32>,
    target_channels: Option<u32>,
    seek_s: f64,
) -> bool {
    seek_s <= 0.0
        && matches!(target_sample_rate, None | Some(CADENCE_OPUS))
        && matches!(target_channels, None | Some(1) | Some(2))
}

/// Passe une trame entrelacée de `de` canaux à `vers` canaux (1 ou 2).
fn remapper(trames: &[i16], de: usize, vers: usize) -> Vec<i32> {
    match (de, vers) {
        (1, 2) => trames
            .iter()
            .flat_map(|&s| [i32::from(s), i32::from(s)])
            .collect(),
        (2, 1) => trames
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[g, d]| (i32::from(*g) + i32::from(*d)) / 2)
            .collect(),
        _ => trames.iter().map(|&s| i32::from(s)).collect(),
    }
}

/// Décode l'Opus d'un fichier encore en téléchargement et l'envoie au canal
/// paquet par paquet. Rend la profondeur et la cadence émises.
#[allow(clippy::too_many_arguments)] // le contrat de `decode_to_pcm_streaming_inner`
pub(super) fn decoder_opus_en_croissance(
    file_path: &str,
    growth: Arc<DashGrowth>,
    target_channels: Option<u32>,
    target_bit_depth: Option<u16>,
    tx: &mpsc::Sender<Vec<u8>>,
    chunk_size: usize,
    data_ready: &Option<Arc<tokio::sync::Notify>>,
    levels_tx: &Option<tokio::sync::mpsc::UnboundedSender<super::super::tap::RawWindow>>,
    rt: &tokio::runtime::Handle,
) -> Result<(u16, u32), String> {
    use opus::{Channels, Decoder as OpusDecoder};

    let source = GrowingFileSource::open(file_path, growth.clone())
        .map_err(|e| format!("open (opus en croissance): {e}"))?;
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("opus");
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("opus probe: {e}"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("opus: no audio track")?;
    let mut track_id = track.id;
    let ch_src: usize = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.channels.as_ref().map(|c| c.count()).unwrap_or(2),
        _ => 2,
    }
    .clamp(1, 2);
    let ch_out: usize = target_channels.map_or(ch_src, |c| c as usize);
    let canaux = if ch_src == 1 {
        Channels::Mono
    } else {
        Channels::Stereo
    };
    let mut decoder =
        OpusDecoder::new(CADENCE_OPUS, canaux).map_err(|e| format!("opus decoder init: {e}"))?;
    let output_bd = target_bit_depth.unwrap_or(16);
    info!(
        file = file_path,
        canaux = ch_out,
        profondeur = output_bd,
        "opus_decodage_pendant_le_telechargement"
    );

    let mut premier_envoye = false;
    if target_bit_depth.is_some() {
        let entete = super::super::wav::build_wav_header(ch_out as u16, CADENCE_OPUS, output_bd);
        if rt.block_on(tx.send(entete.to_vec())).is_err() {
            return Ok((output_bd, CADENCE_OPUS));
        }
        if let Some(n) = data_ready {
            n.notify_one();
        }
        premier_envoye = true;
    }

    let lot = frame_aligned_chunk_len(chunk_size, output_bd, ch_out as u16);
    // 120 ms, la plus grande trame Opus à 48 kHz.
    let mut tampon = vec![0i16; 5760 * ch_src];
    let mut en_attente: Vec<u8> = Vec::with_capacity(lot * 2);
    let mut trames_rendues: u64 = 0;
    let mut fin = false;
    loop {
        if !fin {
            match format.next_packet() {
                Ok(Some(paquet)) => {
                    if paquet.track_id != track_id || paquet.data.is_empty() {
                        continue;
                    }
                    let n = match decoder.decode(&paquet.data, &mut tampon, false) {
                        Ok(n) => n.min(tampon.len() / ch_src),
                        Err(e) => {
                            debug!(error = %e, "opus_en_croissance_trame_refusee");
                            continue;
                        }
                    };
                    trames_rendues += n as u64;
                    let echantillons = remapper(&tampon[..n * ch_src], ch_src, ch_out);
                    en_attente.extend(convert_pcm_bit_depth(&echantillons, 16, output_bd));
                }
                Ok(None) => fin = true,
                // Ogg chaîné (#1270) : nouveau flux physique, même règle que
                // le chemin intégral — décodeur reconstruit, mêmes canaux.
                Err(symphonia::core::errors::Error::ResetRequired) => {
                    let Some(piste) = format.default_track(TrackType::Audio) else {
                        fin = true;
                        continue;
                    };
                    let canaux_piste = match &piste.codec_params {
                        Some(CodecParameters::Audio(p)) => {
                            p.channels.as_ref().map(|c| c.count()).unwrap_or(2)
                        }
                        _ => 2,
                    }
                    .clamp(1, 2);
                    if canaux_piste != ch_src {
                        warn!("opus_en_croissance_chaine_canaux_changes_arret");
                        fin = true;
                        continue;
                    }
                    track_id = piste.id;
                    decoder = OpusDecoder::new(CADENCE_OPUS, canaux)
                        .map_err(|e| format!("opus decoder init: {e}"))?;
                }
                Err(e) => {
                    if growth.a_echoue() {
                        return Err(format!("opus: téléchargement interrompu ({e})"));
                    }
                    // Fin de conteneur signalée en erreur par le démuxeur :
                    // ce qui est décodé est servi, comme sur le chemin intégral.
                    debug!(error = %e, "opus_en_croissance_fin_du_conteneur");
                    fin = true;
                }
            }
        }
        // Les octets décodés partent par lots alignés sur la trame ; le reste
        // part à la fin.
        while en_attente.len() >= lot || (fin && !en_attente.is_empty()) {
            let n = en_attente.len().min(lot);
            let morceau: Vec<u8> = en_attente.drain(..n).collect();
            match rt.block_on(tokio::time::timeout(
                std::time::Duration::from_secs(SEND_TIMEOUT_SECS),
                tx.send(morceau.clone()),
            )) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    debug!("streaming_decode_consumer_dropped (opus en croissance)");
                    return Ok((output_bd, CADENCE_OPUS));
                }
                Err(_) => {
                    warn!(
                        timeout_secs = SEND_TIMEOUT_SECS,
                        "streaming_decode_send_timeout (opus en croissance)"
                    );
                    return Ok((output_bd, CADENCE_OPUS));
                }
            }
            if !premier_envoye {
                premier_envoye = true;
                if let Some(n) = data_ready {
                    n.notify_one();
                }
            }
            if let Some(ltx) = levels_tx {
                super::super::tap::send_windowed_pcm(
                    ltx,
                    &morceau,
                    output_bd,
                    ch_out as u16,
                    CADENCE_OPUS,
                );
            }
        }
        if fin {
            break;
        }
    }
    if trames_rendues == 0 {
        return Err("opus: decoded no audio".into());
    }
    debug!(trames = trames_rendues, "opus_en_croissance_fini");
    Ok((output_bd, CADENCE_OPUS))
}
