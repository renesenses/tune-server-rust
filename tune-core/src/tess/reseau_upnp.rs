//! TESS T12b — la chaîne sur la **famille UPnP** : DLNA, OpenHome, BluOS,
//! Squeezebox et Chromecast.
//!
//! Sur ces zones, Tune ne pousse pas les échantillons : le lecteur va
//! CHERCHER une adresse. La chaîne TESS n'y agit donc que par l'une de deux
//! voies, et la voie dépend du lecteur :
//!
//! | voie | quand | ce qui est porté |
//! |---|---|---|
//! | **relais progressif LPCM** (WAV servi au fil de l'eau, `StreamingDsp` de T04) | le lecteur a ANNONCÉ le LPCM à la profondeur servie, et l'opt-in `dsp_progressif_reseau` est armé (ou un étage casque seul le vaut : #2742, cas 2) | tous les greffons |
//! | **repli par transcodage** (FLAC ré-encodé, piste entière, même chaîne T04) | un égaliseur, une convolution ou un ReplayGain impose le ré-encodage, le FORMAT l'impose (lecteur qui ne lit pas le format, plafond de cadence, DSD converti en PCM), ou le flux vient d'un service | tous les greffons, étage casque compris (#2742, cas 1) |
//! | **aucune** : la piste part telle quelle | étage casque seul (crossfeed, greffons natifs tiers) vers un lecteur SANS LPCM annoncé, piste de la bibliothèque que rien ne force à convertir (#2742, cas 3) | rien : l'étage casque est éteint, motif `renderer_without_lpcm` |
//!
//! Ce module ne décide RIEN de ce qui est servi : la décision reste celle de
//! la lecture (`orchestrator::resolve_local`, `resolve_stream`). Il en est le
//! **miroir** pour l'état effectif (`GET /zones/{id}/tess`), et le témoin
//! `orchestrator::acheminement_upnp_t12b_tests` le compare, case par case, aux
//! prédicats de la lecture : une seule règle, pas deux qui dérivent.
//!
//! Réserves par lecteur (capacité LPCM par type, `capacite_lpcm_par_sortie`) :
//! - DLNA et OpenHome : la sonde (`GetProtocolInfo`, `Playlist:1
//!   ProtocolInfo`) dit ce que le lecteur accepte, à 16 et à 24 bits ;
//! - BluOS et Chromecast : LPCM 16 bits documenté, inconnu au-delà ;
//! - Squeezebox par SlimProto : refusé (le WAV progressif n'a pas été essayé
//!   sur Squeezelite) ; par LMS : inconnu.
//!
//! Une capacité INCONNUE vaut refus pour changer le format servi : on ne
//! promet jamais un traitement qui ne serait pas appliqué.
//!
//! **Bit-perfect** : sans étage effectif (chaîne vide, mode Pure, « Bypass
//! all », DSD natif ou DoP), la voie est `untouched` et le lecteur reçoit les
//! octets de la source. Rien ici ne fait changer de voie une zone qui ne
//! traite rien.

use serde::{Deserialize, Serialize};

use super::resolution::{CodeEtape, EtapeResolue, MotifEtape, Resolution};
use super::zones::TransportZone;

/// La zone joue-t-elle sur la famille UPnP (au sens de T12b) ?
pub fn famille_upnp(t: TransportZone) -> bool {
    matches!(
        t,
        TransportZone::Dlna
            | TransportZone::OpenHome
            | TransportZone::BluOs
            | TransportZone::Squeezebox
            | TransportZone::Chromecast
    )
}

/// Ce que le lecteur dit du LPCM, à la profondeur servie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LpcmDuLecteur {
    /// Annoncé par la sonde, ou établi par la documentation du protocole.
    Accepted,
    /// Établi par le code : servir du LPCM casserait la lecture.
    Refused,
    /// Rien ne l'établit (sonde inconcluante, type sans sonde). Vaut refus.
    #[serde(other)]
    Unknown,
}

/// D'où vient le flux de la zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrigineDuFlux {
    /// Une piste de la bibliothèque (fichier local, partage, UPnP indexé).
    Library,
    /// Un service (Qobuz, TIDAL…) relayé par Tune.
    Service,
    /// Rien ne joue : la règle de la bibliothèque, la plus prudente, s'applique.
    #[serde(other)]
    Unknown,
}

/// Les faits établis par la lecture, sans lesquels la voie ne se dit pas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaitsUpnp {
    /// La capacité LPCM du lecteur à [`Self::lpcm_bits`].
    pub lpcm: LpcmDuLecteur,
    /// La profondeur interrogée (16 ou 24).
    pub lpcm_bits: u8,
    /// L'opt-in `dsp_progressif_reseau` (Réglages → Lecture).
    pub progressif_arme: bool,
    pub origine: OrigineDuFlux,
    /// Un ReplayGain modifie la piste en cours (il impose le ré-encodage
    /// comme l'égaliseur ; hors de la chaîne TESS).
    pub replaygain_actif: bool,
    /// Service seulement : le flux se décode au fil de l'eau (source Range).
    /// Sans cela, le service part au pré-transcodage.
    pub service_decodable_au_fil: bool,
    /// La piste en cours part CONVERTIE quoi qu'il arrive : format que le
    /// lecteur ne lit pas, plafond de cadence, DSD converti… (constaté sur la
    /// session servie). La lecture cuit alors l'étage casque dans la
    /// conversion (`crossfeed_du_fichier`, #2742 cas 1) : il n'est pas éteint.
    pub format_impose_le_transcodage: bool,
}

/// La voie par laquelle la chaîne atteint le lecteur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoieUpnp {
    /// Les octets de la source, tels quels : rien n'est traité.
    Untouched,
    /// WAV servi au fil de l'eau, chaîne appliquée par le relais (T04).
    ProgressiveLpcm,
    /// Piste ré-encodée (FLAC, ou WAV pour un lecteur sans FLAC) avant le
    /// premier octet, chaîne cuite dedans (T04).
    FileTranscode,
    #[serde(other)]
    Unknown,
}

/// Pourquoi la voie n'est pas le relais progressif.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotifDeVoie {
    /// Aucun étage effectif : bit-perfect.
    NothingToProcess,
    /// Le lecteur n'annonce pas le LPCM à cette profondeur (ou rien ne
    /// l'établit) : pas de relais progressif.
    RendererWithoutLpcm,
    /// Le lecteur accepte le LPCM, mais l'opt-in `dsp_progressif_reseau` est
    /// désarmé : la piste est ré-encodée avant le premier son.
    ProgressiveOff,
    /// Service dont le flux ne se décode pas au fil de l'eau.
    ServiceNotStreamable,
    /// Source DSD convertie en PCM : la lecture ne la sert jamais en WAV
    /// progressif traité (`cible_wav_pour_traitement`), la chaîne est cuite
    /// dans la conversion.
    DsdConvertedToPcm,
    #[serde(other)]
    Unknown,
}

/// L'acheminement publié (`delivery` de `GET /zones/{id}/tess`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcheminementUpnp {
    pub route: VoieUpnp,
    /// `None` sur le relais progressif.
    pub reason: Option<MotifDeVoie>,
    pub detail: Option<String>,
    /// Ce que le lecteur dit du LPCM, à `lpcm_bits`.
    pub renderer_lpcm: LpcmDuLecteur,
    pub lpcm_bits: u8,
}

/// Détail de l'étage casque éteint faute de LPCM.
pub const DETAIL_SANS_LPCM: &str = "This renderer does not announce LPCM: crossfeed and \
     headphone plugins only reach it when Tune has to convert the track";

/// La famille d'un greffon sur ce transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Famille {
    /// Égaliseurs, convolution, correction de pièce : ils imposent le
    /// ré-encodage quand le relais progressif n'est pas ouvert
    /// (`eq_forces_transcode`).
    Reencodage,
    /// L'étage casque de `StreamingDsp` : crossfeed et greffons natifs tiers
    /// (`load_crossfeed_processor`). Il n'impose PAS le ré-encodage (#2742).
    Casque,
    /// Repli mono, réaffectation : hors de `StreamingDsp`, leur état réseau
    /// est déjà dit par T02 et T16 ; T12b n'y touche pas.
    HorsRelais,
}

fn famille(id: &str) -> Famille {
    match id {
        "equalizer-graphic"
        | "equalizer-parametric"
        | "equalizer-headphone"
        | "convolution"
        | "room-correction" => Famille::Reencodage,
        "mono-downmix" | "channel-remap" => Famille::HorsRelais,
        _ => Famille::Casque,
    }
}

fn greffons_effectifs(r: &Resolution) -> impl Iterator<Item = &EtapeResolue> {
    r.etapes
        .iter()
        .filter(|e| e.code == CodeEtape::Plugin && e.effective)
}

/// La voie de la chaîne résolue vers ce lecteur. Pure : la matrice se teste
/// sans orchestrateur.
pub fn acheminer(r: &Resolution, f: &FaitsUpnp) -> AcheminementUpnp {
    let reencodage = f.replaygain_actif
        || greffons_effectifs(r)
            .any(|e| famille(e.plugin_id.as_deref().unwrap_or("")) == Famille::Reencodage);
    let casque = greffons_effectifs(r)
        .any(|e| famille(e.plugin_id.as_deref().unwrap_or("")) == Famille::Casque);
    let lpcm = f.lpcm == LpcmDuLecteur::Accepted;
    // Une source DSD qui atteint la chaîne est forcément convertie en PCM
    // (natif et DoP l'éteignent dans la résolution) : la conversion est
    // imposée, et jamais en WAV progressif traité.
    let dsd_converti = source_dsd(r);
    let conversion_imposee = f.format_impose_le_transcodage || dsd_converti;
    let mut a = AcheminementUpnp {
        route: VoieUpnp::Untouched,
        reason: None,
        detail: None,
        renderer_lpcm: f.lpcm,
        lpcm_bits: f.lpcm_bits,
    };
    if !reencodage && !casque {
        a.reason = Some(MotifDeVoie::NothingToProcess);
        if conversion_imposee {
            a.route = VoieUpnp::FileTranscode;
            a.detail = Some("No TESS processing; the track is converted for this renderer".into());
        } else {
            a.detail = Some("Bit-perfect".into());
        }
        return a;
    }
    // #2742, cas 2 : un étage casque SEUL vaut consentement au WAV progressif.
    let consenti = f.progressif_arme || (casque && !reencodage);
    let au_fil = f.origine != OrigineDuFlux::Service || f.service_decodable_au_fil;
    if lpcm && consenti && au_fil && !dsd_converti {
        a.route = VoieUpnp::ProgressiveLpcm;
        return a;
    }
    let motif = if dsd_converti {
        MotifDeVoie::DsdConvertedToPcm
    } else if !lpcm {
        MotifDeVoie::RendererWithoutLpcm
    } else if !au_fil {
        MotifDeVoie::ServiceNotStreamable
    } else {
        MotifDeVoie::ProgressiveOff
    };
    a.reason = Some(motif);
    // Un service part au pré-transcodage dès qu'un traitement est actif ; la
    // bibliothèque quand un étage de ré-encodage ou le format l'impose
    // (#2742, cas 1) ; sinon elle part telle quelle (cas 3).
    if reencodage || conversion_imposee || f.origine == OrigineDuFlux::Service {
        a.route = VoieUpnp::FileTranscode;
        a.detail = Some(match motif {
            MotifDeVoie::DsdConvertedToPcm => {
                "DSD source converted to PCM: processing is applied in the conversion".into()
            }
            MotifDeVoie::RendererWithoutLpcm => format!(
                "This renderer does not announce LPCM at {} bits: the track is converted \
                 before the first note",
                f.lpcm_bits
            ),
            MotifDeVoie::ServiceNotStreamable => {
                "This stream cannot be decoded progressively: the track is converted before \
                 the first note"
                    .into()
            }
            _ => "Progressive processing is off (Settings → Playback): the track is converted \
                  before the first note"
                .into(),
        });
    } else {
        a.route = VoieUpnp::Untouched;
        a.detail = Some(DETAIL_SANS_LPCM.into());
    }
    a
}

/// La source de la résolution est-elle un DSD CONVERTI en PCM ? (En natif,
/// en DoP ou quand le mode est inconnu, la résolution éteint déjà la chaîne :
/// rien n'est converti, la source part telle quelle.)
fn source_dsd(r: &Resolution) -> bool {
    let params = r
        .etapes
        .iter()
        .find(|e| e.code == CodeEtape::Source)
        .and_then(|e| e.params.as_ref());
    let champ = |c: &str| params.and_then(|p| p.get(c)).and_then(|v| v.as_str());
    champ("kind") == Some("dsd") && champ("played_as") == Some("converted_to_pcm")
}

/// Reporte la voie sur la résolution : un étage que la voie ne porte pas est
/// éteint, avec le motif `renderer_without_lpcm`, et les étapes finales
/// (headroom automatique, quantification, badge) sont recalculées.
pub fn appliquer(r: &mut Resolution, a: &AcheminementUpnp) {
    if a.route != VoieUpnp::Untouched || a.reason != Some(MotifDeVoie::RendererWithoutLpcm) {
        return;
    }
    let mut change = false;
    for e in r.etapes.iter_mut().filter(|e| {
        e.code == CodeEtape::Plugin
            && e.effective
            && famille(e.plugin_id.as_deref().unwrap_or("")) == Famille::Casque
    }) {
        e.effective = false;
        e.reason = Some(MotifEtape::RendererWithoutLpcm);
        e.detail = Some(DETAIL_SANS_LPCM.into());
        change = true;
    }
    if change {
        r.recalculer_les_etapes_finales();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::TauxDsd;
    use crate::tess::chaine::{ChaineTess, MaillonGreffon};
    use crate::tess::greffon::catalogue_de_reference;
    use crate::tess::resolution::{
        Badge, ContexteExecution, ControleVolume, LectureDsd, SourceFlux, VolumeZone,
    };
    use serde_json::Value;

    fn tout(_: &str) -> bool {
        true
    }

    fn ctx(pure: bool, source: SourceFlux) -> ContexteExecution<'static> {
        ContexteExecution {
            transport: TransportZone::Dlna,
            source: Some(source),
            premium: true,
            greffon_disponible: &tout,
            pure,
            volume: VolumeZone {
                control: ControleVolume::Device,
                db: 0.0,
            },
        }
    }

    const PCM24: SourceFlux = SourceFlux::Pcm {
        sample_rate_hz: 96_000,
        bits: 24,
        channels: 2,
    };

    fn chaine(ids: &[&str]) -> ChaineTess {
        ChaineTess {
            plugins: ids
                .iter()
                .map(|id| MaillonGreffon {
                    id: (*id).into(),
                    enabled: true,
                    settings: Value::Null,
                })
                .collect(),
            ..ChaineTess::default()
        }
    }

    fn resoudre(ids: &[&str], pure: bool, source: SourceFlux) -> Resolution {
        chaine(ids).resoudre(&catalogue_de_reference(), &ctx(pure, source))
    }

    fn faits(lpcm: LpcmDuLecteur, arme: bool, origine: OrigineDuFlux) -> FaitsUpnp {
        FaitsUpnp {
            lpcm,
            lpcm_bits: 24,
            progressif_arme: arme,
            origine,
            replaygain_actif: false,
            service_decodable_au_fil: true,
            format_impose_le_transcodage: false,
        }
    }

    use LpcmDuLecteur::{Accepted, Refused, Unknown};
    use OrigineDuFlux::{Library, Service};

    #[test]
    fn rien_a_traiter_reste_intact_quel_que_soit_le_lecteur() {
        for (ids, pure) in [
            (&[][..], false),
            (&["equalizer-graphic", "crossfeed"][..], true),
        ] {
            let r = resoudre(ids, pure, PCM24);
            for l in [Accepted, Refused, Unknown] {
                for arme in [false, true] {
                    let a = acheminer(&r, &faits(l, arme, Library));
                    assert_eq!(a.route, VoieUpnp::Untouched, "{ids:?} pure={pure} {l:?}");
                    assert_eq!(a.reason, Some(MotifDeVoie::NothingToProcess));
                }
            }
            assert_eq!(r.badge, Badge::BitPerfect);
        }
    }

    #[test]
    fn le_dsd_natif_ou_dop_ne_part_jamais_au_relais_lpcm() {
        for played_as in [LectureDsd::Native, LectureDsd::Dop, LectureDsd::Unknown] {
            let dsd = SourceFlux::Dsd {
                rate: TauxDsd::Dsd64,
                channels: 2,
                played_as,
            };
            let r = resoudre(&["equalizer-graphic", "crossfeed"], false, dsd);
            let a = acheminer(&r, &faits(Accepted, true, Library));
            assert_eq!(a.route, VoieUpnp::Untouched, "{played_as:?}");
        }
    }

    /// Relecture du 23/11 : crossfeed seul, lecteur sans LPCM, mais le FORMAT
    /// impose la conversion : la lecture cuit le crossfeed dans le fichier.
    /// L'état ne l'éteint pas et ne dit pas « Bit-perfect ».
    #[test]
    fn le_format_qui_impose_la_conversion_garde_l_etage_casque() {
        let mut r = resoudre(&["crossfeed"], false, PCM24);
        let mut f = faits(Refused, false, Library);
        f.format_impose_le_transcodage = true;
        let a = acheminer(&r, &f);
        assert_eq!(a.route, VoieUpnp::FileTranscode);
        assert_eq!(a.reason, Some(MotifDeVoie::RendererWithoutLpcm));
        appliquer(&mut r, &a);
        let cf = r
            .etapes
            .iter()
            .find(|e| e.plugin_id.as_deref() == Some("crossfeed"))
            .unwrap();
        assert!(cf.effective, "cuit dans la conversion");
        assert_eq!(r.badge, Badge::ProcessedByTess);
        // Sans traitement : convertie, mais jamais « Bit-perfect » au détail.
        let r = resoudre(&[], false, PCM24);
        let a = acheminer(&r, &f);
        assert_eq!(a.route, VoieUpnp::FileTranscode);
        assert_eq!(a.reason, Some(MotifDeVoie::NothingToProcess));
        assert_ne!(a.detail.as_deref(), Some("Bit-perfect"));
        // Contre-épreuve : sans conversion imposée, la piste part telle quelle.
        f.format_impose_le_transcodage = false;
        let r = resoudre(&["crossfeed"], false, PCM24);
        assert_eq!(acheminer(&r, &f).route, VoieUpnp::Untouched);
    }

    /// Un DSD converti en PCM n'est jamais servi en WAV progressif traité : la
    /// chaîne est cuite dans la conversion, étage casque compris, même vers un
    /// lecteur qui annonce le LPCM et opt-in armé.
    #[test]
    fn le_dsd_converti_en_pcm_part_par_la_conversion_jamais_par_le_relais() {
        let dsd = SourceFlux::Dsd {
            rate: TauxDsd::Dsd64,
            channels: 2,
            played_as: LectureDsd::ConvertedToPcm,
        };
        for ids in [&["crossfeed"][..], &["equalizer-graphic", "crossfeed"][..]] {
            let mut r = resoudre(ids, false, dsd);
            for l in [Accepted, Refused, Unknown] {
                let a = acheminer(&r, &faits(l, true, Library));
                assert_eq!(a.route, VoieUpnp::FileTranscode, "{ids:?} {l:?}");
                assert_eq!(a.reason, Some(MotifDeVoie::DsdConvertedToPcm));
            }
            let a = acheminer(&r, &faits(Refused, false, Library));
            appliquer(&mut r, &a);
            assert!(
                r.etapes
                    .iter()
                    .filter(|e| e.code == CodeEtape::Plugin)
                    .all(|e| e.effective),
                "{ids:?} : rien n'est éteint"
            );
        }
        // Contre-épreuve : la même chaîne sur un PCM part au relais.
        let r = resoudre(&["crossfeed"], false, PCM24);
        assert_eq!(
            acheminer(&r, &faits(Accepted, true, Library)).route,
            VoieUpnp::ProgressiveLpcm
        );
    }

    #[test]
    fn matrice_de_la_bibliotheque() {
        // (greffons, LPCM, opt-in) → voie attendue
        let cas: &[(&[&str], LpcmDuLecteur, bool, VoieUpnp)] = &[
            (
                &["equalizer-graphic"],
                Accepted,
                true,
                VoieUpnp::ProgressiveLpcm,
            ),
            (
                &["equalizer-graphic"],
                Accepted,
                false,
                VoieUpnp::FileTranscode,
            ),
            (
                &["equalizer-graphic"],
                Refused,
                true,
                VoieUpnp::FileTranscode,
            ),
            (
                &["equalizer-graphic"],
                Unknown,
                true,
                VoieUpnp::FileTranscode,
            ),
            (&["convolution"], Refused, false, VoieUpnp::FileTranscode),
            (&["crossfeed"], Accepted, false, VoieUpnp::ProgressiveLpcm),
            (&["crossfeed"], Refused, true, VoieUpnp::Untouched),
            (&["crossfeed"], Unknown, false, VoieUpnp::Untouched),
            (&["crosstalk"], Refused, false, VoieUpnp::Untouched),
            (
                &["equalizer-graphic", "crossfeed"],
                Accepted,
                false,
                VoieUpnp::FileTranscode,
            ),
            (
                &["equalizer-graphic", "crossfeed"],
                Refused,
                false,
                VoieUpnp::FileTranscode,
            ),
        ];
        for (ids, l, arme, voie) in cas {
            let r = resoudre(ids, false, PCM24);
            let a = acheminer(&r, &faits(*l, *arme, Library));
            assert_eq!(a.route, *voie, "{ids:?} {l:?} arme={arme}");
            if *voie != VoieUpnp::ProgressiveLpcm {
                assert!(
                    a.reason.is_some() && a.detail.is_some(),
                    "{ids:?} : motif publié"
                );
            }
        }
    }

    #[test]
    fn un_service_part_au_pre_transcodage_des_qu_un_traitement_est_actif() {
        let r = resoudre(&["crossfeed"], false, PCM24);
        let a = acheminer(&r, &faits(Refused, false, Service));
        assert_eq!(a.route, VoieUpnp::FileTranscode);
        let mut f = faits(Accepted, true, Service);
        f.service_decodable_au_fil = false;
        let a = acheminer(&r, &f);
        assert_eq!(a.route, VoieUpnp::FileTranscode);
        assert_eq!(a.reason, Some(MotifDeVoie::ServiceNotStreamable));
    }

    #[test]
    fn le_replaygain_seul_impose_le_reencodage_et_porte_l_etage_casque() {
        let r = resoudre(&["crossfeed"], false, PCM24);
        let mut f = faits(Refused, false, Library);
        f.replaygain_actif = true;
        assert_eq!(acheminer(&r, &f).route, VoieUpnp::FileTranscode);
    }

    #[test]
    fn sans_lpcm_l_etage_casque_seul_est_eteint_et_le_badge_redit_bit_perfect() {
        for id in ["crossfeed", "crosstalk"] {
            let mut r = resoudre(&[id], false, PCM24);
            assert_eq!(r.badge, Badge::ProcessedByTess, "{id}");
            assert!(r.headroom_db < 0.0, "{id} : headroom automatique armé");
            let a = acheminer(&r, &faits(Refused, false, Library));
            assert_eq!(a.route, VoieUpnp::Untouched);
            assert_eq!(a.reason, Some(MotifDeVoie::RendererWithoutLpcm));
            appliquer(&mut r, &a);
            let g = r
                .etapes
                .iter()
                .find(|e| e.plugin_id.as_deref() == Some(id))
                .unwrap();
            assert!(!g.effective, "{id}");
            assert_eq!(g.reason, Some(MotifEtape::RendererWithoutLpcm), "{id}");
            assert_eq!(g.detail.as_deref(), Some(DETAIL_SANS_LPCM));
            assert_eq!(r.badge, Badge::BitPerfect, "{id}");
            assert_eq!(r.headroom_db, 0.0, "{id}");
            let h = r
                .etapes
                .iter()
                .find(|e| e.code == CodeEtape::Headroom)
                .unwrap();
            assert_eq!(h.reason, Some(MotifEtape::NothingToProtect), "{id}");
            let q = r
                .etapes
                .iter()
                .find(|e| e.code == CodeEtape::Quantization)
                .unwrap();
            assert!(!q.effective, "{id}");
            assert_eq!(q.reason, Some(MotifEtape::NothingToQuantize), "{id}");
        }
    }

    #[test]
    fn contre_epreuve_avec_lpcm_ou_reencodage_rien_n_est_eteint() {
        for (ids, l) in [
            (&["crossfeed"][..], Accepted),
            (&["equalizer-graphic", "crossfeed"][..], Refused),
        ] {
            let mut r = resoudre(ids, false, PCM24);
            let avant = r.clone();
            let a = acheminer(&r, &faits(l, false, Library));
            appliquer(&mut r, &a);
            assert_eq!(r, avant, "{ids:?} {l:?}");
            assert_eq!(r.badge, Badge::ProcessedByTess);
        }
    }

    #[test]
    fn la_famille_upnp_est_celle_du_plan() {
        let dedans: Vec<_> = TransportZone::ALL
            .into_iter()
            .filter(|t| famille_upnp(*t))
            .collect();
        assert_eq!(
            dedans,
            [
                TransportZone::Dlna,
                TransportZone::OpenHome,
                TransportZone::BluOs,
                TransportZone::Squeezebox,
                TransportZone::Chromecast
            ]
        );
    }

    #[test]
    fn la_voie_se_serialise_en_snake_case_et_lit_l_inconnu() {
        let a = AcheminementUpnp {
            route: VoieUpnp::ProgressiveLpcm,
            reason: None,
            detail: None,
            renderer_lpcm: Accepted,
            lpcm_bits: 24,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["route"], "progressive_lpcm");
        assert_eq!(v["renderer_lpcm"], "accepted");
        let lu: AcheminementUpnp = serde_json::from_value(serde_json::json!({
            "route": "future_route", "reason": "future", "detail": null,
            "renderer_lpcm": "maybe", "lpcm_bits": 16
        }))
        .unwrap();
        assert_eq!(lu.route, VoieUpnp::Unknown);
        assert_eq!(lu.reason, Some(MotifDeVoie::Unknown));
        assert_eq!(lu.renderer_lpcm, LpcmDuLecteur::Unknown);
    }
}
