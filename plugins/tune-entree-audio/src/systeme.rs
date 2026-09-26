//! Les entrées du système, par cpal (feature `capture`).
//!
//! La capture se fait au format que le périphérique a EN CE MOMENT
//! (`default_input_config`) : sa fréquence nominale courante, ses canaux, et
//! le format d'échantillons du pilote. Rien n'est imposé au périphérique —
//! demander une autre fréquence ferait basculer l'horloge d'une interface
//! S/PDIF que la source pilote.
//!
//! Le flux cpal vit dans un fil dédié (il n'est pas `Send` partout) ; l'arrêt
//! passe par un canal.

use std::sync::Arc;
use std::sync::mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig};
use tune_core::source_pcm::FormatPcm;

use crate::anneau::{Anneau, Fin};
use crate::format::{
    Echantillons, Mesure, bits_servis, contient_iec61937, convertir_entiers, convertir_f32,
};
use crate::peripheriques::{Arret, CaptureDemarree, DescriptionEntree, Peripheriques};

pub struct Systeme;

const FREQUENCES_USUELLES: [u32; 10] = [
    32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 768_000,
];

fn pile() -> &'static str {
    if cfg!(target_os = "macos") {
        "coreaudio"
    } else if cfg!(target_os = "linux") {
        "alsa"
    } else if cfg!(target_os = "windows") {
        "wasapi"
    } else {
        "cpal"
    }
}

fn nom(d: &Device) -> String {
    d.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "?".into())
}

fn id(d: &Device) -> String {
    d.id().map(|i| i.to_string()).unwrap_or_else(|_| nom(d))
}

fn echantillons(f: SampleFormat) -> Option<Echantillons> {
    match f {
        SampleFormat::F32 => Some(Echantillons::F32),
        SampleFormat::I16 => Some(Echantillons::I16),
        SampleFormat::I24 => Some(Echantillons::I24),
        SampleFormat::I32 => Some(Echantillons::I32),
        _ => None,
    }
}

fn trouver(entree: &str) -> Result<Device, String> {
    let host = cpal::default_host();
    let mut entrees = host
        .input_devices()
        .map_err(|e| format!("énumération des entrées impossible : {e}"))?;
    entrees
        .find(|d| nom(d) == entree || id(d) == entree)
        .ok_or_else(|| format!("aucune entrée audio « {entree} »"))
}

fn format_natif_de(d: &Device) -> Result<(FormatPcm, SampleFormat), String> {
    let c = d
        .default_input_config()
        .map_err(|e| format!("format courant de « {} » illisible : {e}", nom(d)))?;
    let ech = echantillons(c.sample_format()).ok_or_else(|| {
        format!(
            "format d'échantillons {} non pris en charge",
            c.sample_format()
        )
    })?;
    let bits = bits_servis(ech, physique::bits(&nom(d)));
    Ok((
        FormatPcm {
            frequence: c.sample_rate(),
            canaux: c.channels(),
            bits,
        },
        c.sample_format(),
    ))
}

impl Peripheriques for Systeme {
    fn pile(&self) -> &'static str {
        pile()
    }

    fn lister(&self) -> Result<Vec<DescriptionEntree>, String> {
        let host = cpal::default_host();
        let defaut = host.default_input_device().map(|d| nom(&d));
        let entrees = host
            .input_devices()
            .map_err(|e| format!("énumération des entrées impossible : {e}"))?;
        let mut v = Vec::new();
        for d in entrees {
            let n = nom(&d);
            let courant = d.default_input_config().ok();
            let mut frequences = Vec::new();
            let mut formats: Vec<String> = Vec::new();
            let mut canaux = courant.as_ref().map(|c| c.channels()).unwrap_or(0);
            if let Ok(configs) = d.supported_input_configs() {
                for c in configs {
                    canaux = canaux.max(c.channels());
                    let (a, b) = (c.min_sample_rate(), c.max_sample_rate());
                    for f in FREQUENCES_USUELLES.iter().copied().chain([a, b]) {
                        if f >= a && f <= b && !frequences.contains(&f) {
                            frequences.push(f);
                        }
                    }
                    let f = c.sample_format().to_string();
                    if !formats.contains(&f) {
                        formats.push(f);
                    }
                }
            }
            frequences.sort_unstable();
            let physiques = physique::bits(&n);
            let bits = courant
                .as_ref()
                .and_then(|c| echantillons(c.sample_format()))
                .map(|e| bits_servis(e, physiques))
                .unwrap_or(24);
            v.push(DescriptionEntree {
                par_defaut: defaut.as_deref() == Some(n.as_str()),
                virtuelle: physique::virtuelle(&n),
                id: id(&d),
                nom: n,
                canaux,
                frequences,
                frequence_courante: courant.as_ref().map(|c| c.sample_rate()),
                formats,
                bits_physiques: physiques,
                bits_servis: bits,
            });
        }
        Ok(v)
    }

    fn format_natif(&self, entree: &str) -> Result<(String, FormatPcm), String> {
        let d = trouver(entree)?;
        let (f, _) = format_natif_de(&d)?;
        Ok((nom(&d), f))
    }

    fn demarrer(&self, entree: &str, anneau: Arc<Anneau>) -> Result<CaptureDemarree, String> {
        let entree = entree.to_string();
        let (pret_tx, pret_rx) = mpsc::channel::<Result<(String, FormatPcm), String>>();
        let (arret_tx, arret_rx) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("entree-audio-capture".into())
            .spawn(move || {
                let r = (|| {
                    let d = trouver(&entree)?;
                    let (format, sample_format) = format_natif_de(&d)?;
                    let ech = echantillons(sample_format).expect("vérifié par format_natif_de");
                    let config = StreamConfig {
                        channels: format.canaux,
                        sample_rate: format.frequence,
                        buffer_size: cpal::BufferSize::Default,
                    };
                    let bits = format.bits;
                    let canaux = format.canaux;
                    let a = anneau.clone();
                    let a_err = anneau.clone();
                    let mut origine: Option<cpal::StreamInstant> = None;
                    let mut octets: Vec<u8> = Vec::with_capacity(64 * 1024);
                    let stream = d
                        .build_input_stream_raw(
                            &config,
                            sample_format,
                            move |data: &cpal::Data, info: &cpal::InputCallbackInfo| {
                                octets.clear();
                                let mesure = match ech {
                                    Echantillons::F32 => data
                                        .as_slice::<f32>()
                                        .map(|s| convertir_f32(s, bits, &mut octets)),
                                    Echantillons::I16 => data.as_slice::<i16>().map(|s| {
                                        convertir_entiers(
                                            s.iter().map(|&v| v as i32),
                                            0,
                                            32_768.0,
                                            bits,
                                            &mut octets,
                                        )
                                    }),
                                    Echantillons::I24 => data.as_slice::<cpal::I24>().map(|s| {
                                        convertir_entiers(
                                            s.iter().map(|v| v.inner()),
                                            0,
                                            8_388_608.0,
                                            bits,
                                            &mut octets,
                                        )
                                    }),
                                    Echantillons::I32 => data.as_slice::<i32>().map(|s| {
                                        convertir_entiers(
                                            s.iter().copied(),
                                            8,
                                            2_147_483_648.0,
                                            bits,
                                            &mut octets,
                                        )
                                    }),
                                };
                                let Some(mut mesure): Option<Mesure> = mesure else {
                                    return;
                                };
                                mesure.iec61937 = contient_iec61937(&octets, canaux, bits);
                                let capture = info.timestamp().capture;
                                let o = *origine.get_or_insert(capture);
                                a.pousser(&octets, mesure, capture.duration_since(&o));
                            },
                            move |err| {
                                a_err.fermer(Fin::Erreur(format!(
                                    "le périphérique a failli : {err}"
                                )));
                            },
                            None,
                        )
                        .map_err(|e| format!("ouverture de la capture impossible : {e}"))?;
                    stream
                        .play()
                        .map_err(|e| format!("démarrage de la capture impossible : {e}"))?;
                    Ok((stream, nom(&d), format))
                })();
                match r {
                    Ok((stream, nom, format)) => {
                        let _ = pret_tx.send(Ok((nom, format)));
                        // Vit jusqu'à l'arrêt (ou l'abandon de l'émetteur).
                        let _ = arret_rx.recv();
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = pret_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| format!("fil de capture impossible : {e}"))?;
        let (nom, format) = pret_rx
            .recv()
            .map_err(|_| "le fil de capture s'est arrêté avant de répondre".to_string())??;
        Ok(CaptureDemarree {
            nom,
            format,
            arret: Box::new(ArretCpal(arret_tx)),
        })
    }

    fn frequence_courante(&self, entree: &str) -> Option<u32> {
        if let Some(f) = physique::frequence_nominale(entree) {
            return Some(f);
        }
        trouver(entree)
            .ok()?
            .default_input_config()
            .ok()
            .map(|c| c.sample_rate())
    }
}

struct ArretCpal(mpsc::Sender<()>);

impl Arret for ArretCpal {
    fn arreter(self: Box<Self>) {
        let _ = self.0.send(());
    }
}

/// Ce que cpal ne dit pas : le format PHYSIQUE de l'entrée et sa fréquence
/// nominale courante, lus directement à CoreAudio.
#[cfg(target_os = "macos")]
mod physique {
    use std::mem;
    use std::ptr::{NonNull, null};

    use coreaudio::audio_unit::macos_helpers::get_device_id_from_name;
    use objc2_core_audio::{
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
        kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreams, kAudioHardwareNoError,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeInput, kAudioStreamPropertyPhysicalFormat,
    };
    use objc2_core_audio_types::{AudioStreamBasicDescription, kAudioFormatFlagIsFloat};

    fn identifiant(nom: &str) -> Option<u32> {
        get_device_id_from_name(nom, true)
    }

    /// Profondeur physique (32 pour un flottant), de la PREMIÈRE voie
    /// d'entrée.
    pub fn bits(nom: &str) -> Option<u16> {
        let dev = identifiant(nom)?;
        let adresse = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreams,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut taille = 0u32;
        // SAFETY: appels CoreAudio sur des pointeurs de pile valides ; la
        // taille rendue borne le tampon alloué ensuite.
        let flux: Vec<u32> = unsafe {
            if AudioObjectGetPropertyDataSize(
                dev,
                NonNull::from(&adresse),
                0,
                null(),
                NonNull::from(&mut taille),
            ) != kAudioHardwareNoError
            {
                return None;
            }
            let n = taille as usize / mem::size_of::<u32>();
            let mut v = vec![0u32; n];
            if n == 0
                || AudioObjectGetPropertyData(
                    dev,
                    NonNull::from(&adresse),
                    0,
                    null(),
                    NonNull::from(&mut taille),
                    NonNull::new(v.as_mut_ptr())?.cast(),
                ) != kAudioHardwareNoError
            {
                return None;
            }
            v
        };
        let adresse = AudioObjectPropertyAddress {
            mSelector: kAudioStreamPropertyPhysicalFormat,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut asbd: AudioStreamBasicDescription = unsafe { mem::zeroed() };
        let mut taille = mem::size_of::<AudioStreamBasicDescription>() as u32;
        // SAFETY: idem, sur la première voie d'entrée.
        let statut = unsafe {
            AudioObjectGetPropertyData(
                *flux.first()?,
                NonNull::from(&adresse),
                0,
                null(),
                NonNull::from(&mut taille),
                NonNull::from(&mut asbd).cast(),
            )
        };
        if statut != kAudioHardwareNoError {
            return None;
        }
        if asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0 {
            return Some(32);
        }
        u16::try_from(asbd.mBitsPerChannel).ok().filter(|b| *b > 0)
    }

    /// Transport CoreAudio `Virtual` : Loopback Audio, BlackHole, Teams…
    pub fn virtuelle(nom: &str) -> Option<bool> {
        let t = coreaudio::audio_unit::macos_helpers::get_device_transport_type(identifiant(nom)?)
            .ok()?;
        Some(t == objc2_core_audio::kAudioDeviceTransportTypeVirtual)
    }

    pub fn frequence_nominale(nom: &str) -> Option<u32> {
        let dev = identifiant(nom)?;
        let adresse = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyNominalSampleRate,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut f: f64 = 0.0;
        let mut taille = mem::size_of::<f64>() as u32;
        // SAFETY: lecture d'un f64 dans une variable de pile.
        let statut = unsafe {
            AudioObjectGetPropertyData(
                dev,
                NonNull::from(&adresse),
                0,
                null(),
                NonNull::from(&mut taille),
                NonNull::from(&mut f).cast(),
            )
        };
        (statut == kAudioHardwareNoError && f > 0.0).then_some(f.round() as u32)
    }
}

#[cfg(not(target_os = "macos"))]
mod physique {
    pub fn bits(_: &str) -> Option<u16> {
        None
    }
    /// Le système ne le dit pas ici : on ne devine pas.
    pub fn virtuelle(_: &str) -> Option<bool> {
        None
    }
    pub fn frequence_nominale(_: &str) -> Option<u32> {
        None
    }
}
