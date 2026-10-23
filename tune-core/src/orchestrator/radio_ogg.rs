//! Les flux radio Ogg : Opus par libopus, et le passage d'un flux logique au
//! suivant (Ogg chaîné).
//!
//! # Opus
//!
//! symphonia démuxe l'Ogg (fonction `ogg`) et reconnaît la piste Opus, mais
//! n'a AUCUN décodeur Opus : `make_audio_decoder` rendait « unsupported audio
//! codec » et la station ne jouait pas (mesuré le 14/10/2026 sur le flux Opus
//! de Deutschlandfunk). Les fichiers passaient déjà par libopus, la crate
//! `opus` (`audio::decode::decode_opus_to_pcm`) ; le lecteur radio, lui, ne
//! connaissait que symphonia. [`DecodeurRadio`] porte l'un ou l'autre.
//!
//! # Ogg chaîné
//!
//! Un serveur Icecast qui diffuse de l'Ogg ouvre un NOUVEAU flux logique à
//! chaque titre : nouvelles pages d'en-tête, nouveaux commentaires Vorbis ou
//! `OpusTags` (c'est là que voyage le titre, pas dans un bloc ICY). Le lecteur
//! Ogg de symphonia le signale par `Error::ResetRequired`. La boucle de
//! décodage le prenait pour une erreur de paquet et refaisait un GET à chaque
//! changement de titre : une coupure, et une reconnexion comptée. On
//! reconstruit ici le décodeur sur la nouvelle piste, sans quitter la
//! connexion, tant que la cadence et le nombre de canaux ne changent pas.
use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::AudioDecoder;
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::packet::Packet;

/// La plus grande trame Opus : 120 ms à 48 kHz, par canal.
const TRAME_OPUS_MAX: usize = 5760;

/// Le décodeur d'une connexion radio.
pub(super) enum DecodeurRadio {
    /// Tout ce que symphonia décode : MP3, AAC, FLAC, ALAC, Vorbis.
    Symphonia(Box<dyn AudioDecoder>),
    /// #6357 — l'AAC d'une station HLS, par `rusty_aac` : HE-AAC (SBR) et
    /// HE-AACv2 (PS) compris, que symphonia 0.6 décode en AAC-LC seul (à la
    /// moitié de la fréquence, sans les aigus, et sans rien dire). symphonia
    /// reste le lecteur ADTS : ses paquets sont des unités d'accès brutes.
    Aac {
        decodeur: Box<rusty_aac::AacDecoder>,
        /// La fréquence et les canaux de la dernière trame décodée : ceux de
        /// la SORTIE (le double du cœur quand le SBR est présent).
        cadence: Option<u32>,
        canaux: usize,
    },
    /// Opus, que symphonia démuxe sans le décoder.
    Opus {
        decodeur: opus::Decoder,
        canaux: usize,
        tampon: Vec<f32>,
        /// Échantillons (par canal) encore à écarter en tête du flux logique :
        /// le pre-skip de l'OpusHead (RFC 7845 §4.2), le pré-roll de
        /// l'encodeur, qui ne fait pas partie du son. Un décodeur neuf par
        /// flux logique : il est rejoué à chaque titre d'un Ogg chaîné.
        a_ecarter: usize,
    },
}

/// Le pre-skip que l'OpusHead annonce (octets 10 et 11, petit-boutiste),
/// 0 sans en-tête lisible. symphonia recopie l'OpusHead entier dans
/// `extra_data`.
fn pre_skip_opus(params: &AudioCodecParameters) -> usize {
    params
        .extra_data
        .as_deref()
        .filter(|tete| tete.len() >= 12 && tete.starts_with(b"OpusHead"))
        .map_or(0, |tete| {
            usize::from(u16::from_le_bytes([tete[10], tete[11]]))
        })
}

impl DecodeurRadio {
    /// Le décodeur qui convient aux paramètres de la piste sondée.
    pub(super) fn pour(params: &AudioCodecParameters) -> Result<Self, String> {
        if params.codec == CODEC_ID_OPUS {
            let canaux = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
            // Le décodeur simple de libopus ne connaît que le mono et la
            // stéréo (famille de projection 0) ; un Opus multicanal demande
            // le décodeur « multistream », qu'aucun chemin de Tune n'emploie.
            let disposition = match canaux {
                1 => opus::Channels::Mono,
                2 => opus::Channels::Stereo,
                n => {
                    return Err(format!(
                        "radio decoder init failed: Opus à {n} canaux non pris en charge"
                    ));
                }
            };
            let decodeur = opus::Decoder::new(48_000, disposition)
                .map_err(|e| format!("radio decoder init failed: opus: {e}"))?;
            return Ok(DecodeurRadio::Opus {
                decodeur,
                canaux,
                tampon: vec![0.0; TRAME_OPUS_MAX * canaux],
                a_ecarter: pre_skip_opus(params),
            });
        }
        symphonia::default::get_codecs()
            .make_audio_decoder(params, &Default::default())
            .map(DecodeurRadio::Symphonia)
            .map_err(|e| format!("radio decoder init failed: {e}"))
    }

    /// #6357 — le décodeur `rusty_aac` d'une piste AAC, configuré par la
    /// sonde ADTS (objet, fréquence et canaux du CŒUR). Le SBR implicite
    /// (signalé dans les éléments de remplissage, cas des TS de la BBC) est
    /// reconnu dès la première trame.
    pub(super) fn aac_pur_rust(params: &AudioCodecParameters) -> Result<Self, String> {
        let cadence = params
            .sample_rate
            .ok_or("radio decoder init failed: aac: fréquence inconnue")?;
        let canaux = params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
        let decodeur = rusty_aac::AacDecoder::with_config(rusty_aac::AudioSpecificConfig {
            object_type: rusty_aac::sbr::AOT_AAC_LC,
            sample_rate: cadence,
            channels: u16::try_from(canaux).unwrap_or(2),
        });
        Ok(DecodeurRadio::Aac {
            decodeur: Box::new(decodeur),
            cadence: None,
            canaux,
        })
    }

    /// La fréquence de la dernière trame décodée, quand le décodeur la
    /// connaît mieux que la sonde (HE-AAC : le double du cœur).
    pub(super) fn cadence_decodee(&self) -> Option<u32> {
        match self {
            DecodeurRadio::Aac { cadence, .. } => *cadence,
            _ => None,
        }
    }

    /// Décode un paquet en échantillons entrelacés, rangés dans `sortie`
    /// (vidée d'abord). Rend le nombre de canaux.
    pub(super) fn decoder(
        &mut self,
        paquet: &Packet,
        sortie: &mut Vec<f32>,
    ) -> Result<usize, String> {
        sortie.clear();
        match self {
            DecodeurRadio::Symphonia(decodeur) => {
                let decode = decodeur.decode(paquet).map_err(|e| e.to_string())?;
                let canaux = decode.spec().channels().count();
                decode.copy_to_vec_interleaved::<f32>(sortie);
                Ok(canaux)
            }
            DecodeurRadio::Aac {
                decodeur,
                cadence,
                canaux,
            } => match decodeur.decode(&paquet.data, None) {
                Ok(trame) => {
                    *cadence = Some(trame.sample_rate);
                    *canaux = usize::from(trame.channels);
                    sortie.extend_from_slice(&trame.samples);
                    Ok(*canaux)
                }
                // Paquet vide : rien à décoder, rien à servir.
                Err(rusty_aac::Error::Again) => Ok(*canaux),
                Err(e) => Err(format!("aac: {e}")),
            },
            DecodeurRadio::Opus {
                decodeur,
                canaux,
                tampon,
                a_ecarter,
            } => {
                if paquet.data.is_empty() {
                    return Ok(*canaux);
                }
                let par_canal = decodeur
                    .decode_float(&paquet.data, tampon, false)
                    .map_err(|e| format!("opus: {e}"))?;
                let n = (par_canal * *canaux).min(tampon.len());
                let ecartes = (*a_ecarter).min(par_canal);
                *a_ecarter -= ecartes;
                sortie.extend_from_slice(&tampon[(ecartes * *canaux).min(n)..n]);
                Ok(*canaux)
            }
        }
    }
}

/// Le titre que portent les commentaires du flux logique en cours, pour le
/// journal (le now-playing, lui, passe par `radio_metadata`).
pub(super) fn titre_du_flux_logique(
    format: &mut dyn symphonia::core::formats::FormatReader,
) -> Option<String> {
    let mut metadonnees = format.metadata();
    let (artiste, titre) =
        crate::radio_metadata::artiste_et_titre_des_commentaires(metadonnees.skip_to_latest()?)?;
    Some(match artiste {
        Some(a) => format!("{a} - {titre}"),
        None => titre,
    })
}

/// Après `ResetRequired` : la piste du nouveau flux logique, et son décodeur,
/// quand elle garde la cadence et les canaux annoncés au lecteur (l'en-tête
/// WAV est déjà parti). `None` : le format a changé, on se reconnecte, et la
/// garde de `preparer_la_sortie` tranche.
pub(super) fn piste_du_flux_suivant(
    format: &dyn symphonia::core::formats::FormatReader,
    canaux_attendus: u16,
    cadence_attendue: u32,
) -> Option<(u32, DecodeurRadio)> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::TrackType;
    let piste = format.default_track(TrackType::Audio)?;
    let params = match &piste.codec_params {
        Some(CodecParameters::Audio(p)) => p,
        _ => return None,
    };
    let canaux = params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(2);
    let cadence = params.sample_rate.unwrap_or(44100);
    if canaux != canaux_attendus || cadence != cadence_attendue {
        tracing::warn!(
            canaux_attendus,
            canaux,
            cadence_attendue,
            cadence,
            "radio_ogg_flux_suivant_format_change"
        );
        return None;
    }
    let decodeur = DecodeurRadio::pour(params).ok()?;
    Some((piste.id, decodeur))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Au passage d'un flux logique au suivant, la piste nouvelle reçoit un
    /// décodeur, et ses commentaires donnent le titre suivant.
    #[test]
    fn le_flux_logique_suivant_donne_son_decodeur_et_son_titre() {
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::io::MediaSourceStream;
        let mut octets =
            crate::ogg_factice::ogg_opus(30, 0.2, &[("ARTIST", "Artiste A"), ("TITLE", "Titre A")]);
        octets.extend(crate::ogg_factice::ogg_opus(
            31,
            0.2,
            &[("ARTIST", "Artiste B"), ("TITLE", "Titre B")],
        ));
        let mss =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(octets)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("ogg");
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, Default::default(), Default::default())
            .expect("sonde");
        assert_eq!(
            titre_du_flux_logique(&mut *format).as_deref(),
            Some("Artiste A - Titre A")
        );
        loop {
            match format.next_packet() {
                Ok(Some(_)) => continue,
                Err(symphonia::core::errors::Error::ResetRequired) => break,
                autre => panic!("le second flux logique doit être signalé : {autre:?}"),
            }
        }
        let (_, mut decodeur) =
            piste_du_flux_suivant(&*format, 2, 48_000).expect("même format : on continue");
        assert_eq!(
            titre_du_flux_logique(&mut *format).as_deref(),
            Some("Artiste B - Titre B")
        );
        let paquet = format
            .next_packet()
            .expect("paquet")
            .expect("le second flux a des paquets");
        let mut pcm = Vec::new();
        assert_eq!(decodeur.decoder(&paquet, &mut pcm), Ok(2));
        assert!(!pcm.is_empty());
        // Un autre format (cadence annoncée différente) : on ne continue pas.
        assert!(piste_du_flux_suivant(&*format, 2, 44_100).is_none());
    }

    /// Le pre-skip de l'OpusHead (312 dans la fixture) est écarté en tête de
    /// CHAQUE flux logique : sinon le pré-roll de l'encodeur (6,5 ms) est
    /// joué au début du flux, puis à chaque titre d'un Ogg chaîné.
    #[test]
    fn le_pre_skip_opus_est_ecarte_a_chaque_flux_logique() {
        use symphonia::core::codecs::CodecParameters;
        use symphonia::core::formats::TrackType;
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::io::MediaSourceStream;
        // 0,2 s par flux : 10 trames de 960 échantillons par canal.
        let mut octets = crate::ogg_factice::ogg_opus(40, 0.2, &[("TITLE", "Un")]);
        octets.extend(crate::ogg_factice::ogg_opus(41, 0.2, &[("TITLE", "Deux")]));
        let mss =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(octets)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("ogg");
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, Default::default(), Default::default())
            .expect("sonde");
        let mut decodeur = match &format
            .default_track(TrackType::Audio)
            .expect("piste")
            .codec_params
        {
            Some(CodecParameters::Audio(p)) => DecodeurRadio::pour(p).expect("décodeur opus"),
            autre => panic!("piste audio attendue : {autre:?}"),
        };
        let mut par_flux = vec![0usize];
        let mut pcm = Vec::new();
        loop {
            match format.next_packet() {
                Ok(Some(paquet)) => {
                    assert_eq!(decodeur.decoder(&paquet, &mut pcm), Ok(2));
                    *par_flux.last_mut().unwrap() += pcm.len() / 2;
                }
                Err(symphonia::core::errors::Error::ResetRequired) => {
                    decodeur = piste_du_flux_suivant(&*format, 2, 48_000)
                        .expect("même format : on continue")
                        .1;
                    par_flux.push(0);
                }
                _ => break,
            }
        }
        assert_eq!(
            par_flux,
            vec![10 * 960 - 312, 10 * 960 - 312],
            "chaque flux logique doit perdre exactement le pre-skip de son OpusHead"
        );
    }
}
