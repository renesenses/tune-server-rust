//! Résolution d'une chaîne à l'exécution : ce qui est demandé, ce qui est
//! réellement appliqué, et pourquoi.
//!
//! La résolution ne REJETTE JAMAIS une chaîne : un droit perdu, un paquet
//! désinstallé, un flux 5.1 sur un greffon stéréo, une zone hors TESS, une
//! chaîne d'une version plus récente éteignent seulement les étages concernés,
//! chacun avec un motif stable (modèle de `crossfeed_status` et de #7632).
//! C'est elle que consomment l'exécuteur (T03/T04), l'API (T08) et le chemin
//! du signal (T16).
//!
//! Ordre réel des étapes (décision Q9) :
//!
//! ```text
//! Source → Headroom → greffons (ordre de la liste) → Upsampling → Volume
//!        → Quantization (étape finale séparée) → Output
//! ```
//!
//! Exception de Q9, upsampling vers DSD : `… → Upsampling → Volume →
//! DsdModulator → Output`, le volume juste avant le modulateur (réponse plus
//! lente acceptée). L'écran TESS montre CET ordre ; les maquettes A1 et A4,
//! qui dessinent le volume avant l'upsampling, sont à corriger.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::chaine::{
    ChaineTess, Dither, HEADROOM_AUTO_DB, Headroom, MaillonGreffon, TauxDsd, Upsampling,
    VERSION_CHAINE,
};
use super::greffon::{Canaux, Catalogue};
use super::zones::{Applicabilite, TransportZone};

/// Code stable d'une étape du chemin du signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeEtape {
    Source,
    Headroom,
    Plugin,
    Upsampling,
    Volume,
    Quantization,
    DsdModulator,
    Output,
    #[serde(other)]
    Unknown,
}

/// Pourquoi une étape demandée n'est pas appliquée (ou n'a rien à faire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotifEtape {
    /// La zone est hors TESS (Q8) ; le détail dit pourquoi.
    OutsideTess,
    /// Mode Pure : aucun traitement, bit-perfect.
    PureMode,
    /// « Bypass all ».
    BypassAll,
    /// Source DSD jouée en natif ou en DoP : rien à traiter en PCM.
    DsdSourceNative,
    /// Chaîne écrite par une version plus récente : tout est éteint plutôt
    /// qu'appliqué à moitié.
    ChainVersionUnsupported,
    /// Réglage lu `unknown` (version plus récente).
    UnknownValue,
    /// Greffon absent du catalogue de ce serveur.
    UnknownPlugin,
    /// Paquet désinstallé ou désactivé.
    PluginUnavailable,
    /// Droit Premium absent (ou perdu en cours de route).
    PremiumRequired,
    /// Greffon stéréo seulement, flux d'un autre nombre de canaux.
    StereoOnly,
    /// Écarté par un greffon plus prioritaire du même groupe exclusif.
    ReplacedInGroup,
    /// Headroom automatique : aucun traitement effectif à protéger.
    NothingToProtect,
    /// Volume réglé par l'appareil ou volume fixe.
    VolumeNotSoftware,
    /// Rien ne modifie les échantillons : aucune requantification.
    NothingToQuantize,
    #[serde(other)]
    Unknown,
}

/// Qui règle le volume de la zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControleVolume {
    /// Volume logiciel, en flottant, dans la chaîne TESS.
    Software,
    /// Volume de l'appareil (DLNA, AirPlay, Chromecast…), commande envoyée.
    Device,
    /// Volume fixe.
    Fixed,
    #[serde(other)]
    Unknown,
}

/// Où le volume logiciel est appliqué.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementVolume {
    /// En flottant au rendu, après l'anneau : réactif en moins de 50 ms (#7264).
    AtRender,
    /// Juste avant le modulateur DSD : réponse plus lente acceptée (Q9).
    BeforeDsdModulator,
    #[serde(other)]
    Unknown,
}

/// Le volume de la zone, tel que la zone le règle (entrée de la résolution).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VolumeZone {
    pub control: ControleVolume,
    /// Gain logiciel en dB (0 = unitaire) ; ignoré hors `software`.
    pub db: f64,
}

/// L'état du volume publié (sortie de la résolution).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EtatVolume {
    pub control: ControleVolume,
    /// `None` quand le volume logiciel n'est pas appliqué.
    pub placement: Option<PlacementVolume>,
    /// Gain logiciel appliqué ; `None` hors volume logiciel.
    pub db: Option<f64>,
}

/// Comment une source DSD est jouée.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LectureDsd {
    /// DSD natif jusqu'au DAC : hors traitement.
    Native,
    /// DoP : hors traitement.
    Dop,
    /// Converti en PCM par le transport (hors TESS) : la chaîne s'applique.
    ConvertedToPcm,
    #[serde(other)]
    Unknown,
}

/// Le flux qui entre dans la chaîne.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceFlux {
    Pcm {
        sample_rate_hz: u32,
        bits: u8,
        channels: u16,
    },
    Dsd {
        rate: TauxDsd,
        channels: u16,
        played_as: LectureDsd,
    },
    #[serde(other)]
    Unknown,
}

impl SourceFlux {
    fn canaux(&self) -> Option<u16> {
        match self {
            Self::Pcm { channels, .. } | Self::Dsd { channels, .. } => Some(*channels),
            Self::Unknown => None,
        }
    }

    fn dsd_hors_traitement(&self) -> bool {
        matches!(
            self,
            Self::Dsd {
                played_as: LectureDsd::Native | LectureDsd::Dop | LectureDsd::Unknown,
                ..
            }
        )
    }
}

/// Ce que la résolution doit savoir de la zone, du compte et du flux.
pub struct ContexteExecution<'a> {
    pub transport: TransportZone,
    /// `None` quand rien ne joue : les contraintes liées au flux ne sont pas
    /// évaluées.
    pub source: Option<SourceFlux>,
    pub premium: bool,
    /// Le paquet ou la référence est-il installé et activé ?
    pub greffon_disponible: &'a dyn Fn(&str) -> bool,
    /// Mode Pure de la zone (T02 y rattache `audiophile` et
    /// `strict_bitperfect`).
    pub pure: bool,
    pub volume: VolumeZone,
}

/// Une étape du chemin du signal, résolue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EtapeResolue {
    pub code: CodeEtape,
    pub plugin_id: Option<String>,
    /// Étape fixe (non déplaçable) : « FIXED » à l'écran.
    pub fixed: bool,
    /// Ce que la chaîne et la zone demandent.
    pub requested: bool,
    /// Ce qui est réellement appliqué aux échantillons.
    pub effective: bool,
    pub reason: Option<MotifEtape>,
    /// Le motif en clair, ou ce qui écarte l'étape.
    pub detail: Option<String>,
    /// Paramètres affichés (maquette A4) : dB, filtre, format… ; les
    /// paramètres propres à un greffon sont ajoutés par T16.
    pub params: Option<Value>,
    /// Latence de l'étage en trames, connue après sa préparation (T03).
    pub latency_frames: Option<u32>,
}

/// Le badge du chemin du signal (maquette A4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Badge {
    /// « Bit-perfect » : aucune étape ne modifie les échantillons.
    BitPerfect,
    /// « Processed by TESS · 64-bit ».
    ProcessedByTess,
    /// Zone hors TESS.
    OutsideTess,
    #[serde(other)]
    Unknown,
}

/// Le résultat de la résolution.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub etapes: Vec<EtapeResolue>,
    pub headroom_db: f64,
    pub volume: EtatVolume,
    pub badge: Badge,
}

fn etape(code: CodeEtape, fixed: bool) -> EtapeResolue {
    EtapeResolue {
        code,
        plugin_id: None,
        fixed,
        requested: true,
        effective: true,
        reason: None,
        detail: None,
        params: None,
        latency_frames: None,
    }
}

impl EtapeResolue {
    fn eteindre(&mut self, motif: MotifEtape, detail: Option<String>) {
        self.effective = false;
        self.reason = Some(motif);
        self.detail = detail;
    }
}

fn params_upsampling(u: &Upsampling) -> Option<Value> {
    match u {
        Upsampling::Off {} | Upsampling::Unknown => None,
        Upsampling::Pcm {
            filter,
            max_ratio,
            max_rate_hz,
        } => Some(json!({
            "target": "pcm", "filter": filter, "max_ratio": max_ratio, "max_rate_hz": max_rate_hz
        })),
        Upsampling::Dsd { rate, filter } => {
            Some(json!({ "target": "dsd", "rate": rate, "filter": filter }))
        }
    }
}

impl ChaineTess {
    /// Résout la chaîne pour une zone et un flux. Ne rejette jamais.
    pub fn resoudre(&self, catalogue: &Catalogue, ctx: &ContexteExecution<'_>) -> Resolution {
        let applicabilite = ctx.transport.applicabilite();
        // Le blocage global, par ordre de priorité du motif.
        let blocage: Option<(MotifEtape, Option<String>)> = match &applicabilite {
            Applicabilite::OutsideTess { detail, .. } => {
                Some((MotifEtape::OutsideTess, Some(detail.clone())))
            }
            Applicabilite::Unknown => Some((MotifEtape::OutsideTess, None)),
            Applicabilite::Applied if !(1..=VERSION_CHAINE).contains(&self.version) => {
                Some((MotifEtape::ChainVersionUnsupported, None))
            }
            Applicabilite::Applied if ctx.pure => Some((MotifEtape::PureMode, None)),
            Applicabilite::Applied if ctx.source.is_some_and(|s| s.dsd_hors_traitement()) => {
                Some((MotifEtape::DsdSourceNative, None))
            }
            Applicabilite::Applied if self.bypass_all => Some((MotifEtape::BypassAll, None)),
            Applicabilite::Applied => None,
        };
        let bloque = |e: &mut EtapeResolue| {
            if let Some((m, d)) = &blocage {
                e.eteindre(*m, d.clone());
            }
        };

        let mut source = etape(CodeEtape::Source, true);
        source.params = ctx
            .source
            .map(|s| serde_json::to_value(s).unwrap_or(Value::Null));

        // Greffons.
        let mut canaux = ctx.source.and_then(|s| s.canaux());
        let mut greffons: Vec<EtapeResolue> = Vec::with_capacity(self.plugins.len());
        for m in &self.plugins {
            greffons.push(self.resoudre_greffon(m, catalogue, ctx, &mut canaux));
            if let Some(e) = greffons.last_mut()
                && e.requested
            {
                bloque(e);
            }
        }
        // Groupes exclusifs : le plus prioritaire des effectifs l'emporte.
        let groupes: Vec<_> = self
            .plugins
            .iter()
            .map(|m| catalogue.get(&m.id).and_then(|d| d.groupe_effectif()))
            .collect();
        for i in 0..greffons.len() {
            let Some(g) = &groupes[i] else { continue };
            if !greffons[i].effective {
                continue;
            }
            let gagnant = (0..greffons.len())
                .filter(|&j| greffons[j].effective)
                .filter_map(|j| {
                    groupes[j]
                        .as_ref()
                        .filter(|h| h.id == g.id)
                        .map(|h| (j, h.priority))
                })
                // Plus prioritaire d'abord ; à égalité, le premier de la chaîne.
                .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
                .map(|(j, _)| j);
            if let Some(j) = gagnant
                && j != i
            {
                let id = self.plugins[j].id.clone();
                greffons[i].eteindre(
                    MotifEtape::ReplacedInGroup,
                    Some(format!("Replaced by {id}")),
                );
                greffons[i].params = Some(json!({ "replaced_by": id }));
            }
        }

        // Upsampling.
        let up = &self.fixed.upsampling;
        let mut upsampling = etape(CodeEtape::Upsampling, true);
        upsampling.requested = up.demande();
        upsampling.effective = upsampling.requested;
        upsampling.params = params_upsampling(up);
        if upsampling.requested {
            if blocage.is_some() {
                bloque(&mut upsampling);
            } else if matches!(up, Upsampling::Unknown)
                || matches!(
                    up,
                    Upsampling::Dsd {
                        rate: TauxDsd::Unknown,
                        ..
                    }
                )
            {
                upsampling.eteindre(MotifEtape::UnknownValue, None);
            } else if !ctx.premium {
                upsampling.eteindre(MotifEtape::PremiumRequired, None);
            }
        }
        let vers_dsd = upsampling.effective && matches!(up, Upsampling::Dsd { .. });

        // Headroom (calculé après greffons et upsampling : l'automatique ne
        // protège que des traitements EFFECTIFS).
        let traitement_effectif = greffons.iter().any(|e| e.effective) || upsampling.effective;
        let mut headroom = etape(CodeEtape::Headroom, true);
        let (demande_db, auto) = match self.fixed.headroom {
            Headroom::Auto {} => (HEADROOM_AUTO_DB, true),
            Headroom::Manual { db } => (db, false),
            Headroom::Unknown => (0.0, false),
        };
        headroom.requested = if auto {
            self.plugins.iter().any(|m| m.enabled) || up.demande()
        } else {
            demande_db != 0.0
        };
        headroom.effective = headroom.requested;
        let mode = if auto { "auto" } else { "manual" };
        headroom.params = Some(json!({ "mode": mode, "db": demande_db }));
        if matches!(self.fixed.headroom, Headroom::Unknown) {
            headroom.requested = true;
            headroom.eteindre(MotifEtape::UnknownValue, None);
        } else if headroom.requested {
            if blocage.is_some() {
                bloque(&mut headroom);
            } else if auto && !traitement_effectif {
                headroom.eteindre(MotifEtape::NothingToProtect, None);
            }
        }
        let headroom_db = if headroom.effective { demande_db } else { 0.0 };

        // Volume.
        let mut volume = etape(CodeEtape::Volume, true);
        let logiciel = ctx.volume.control == ControleVolume::Software;
        volume.requested = logiciel && ctx.volume.db != 0.0;
        volume.effective = volume.requested;
        volume.params =
            Some(json!({ "control": ctx.volume.control, "db": logiciel.then_some(ctx.volume.db) }));
        if !logiciel {
            volume.effective = false;
            volume.reason = Some(MotifEtape::VolumeNotSoftware);
        } else if volume.requested
            && let Some((m @ (MotifEtape::OutsideTess | MotifEtape::DsdSourceNative), d)) = &blocage
        {
            // Le volume est un réglage de zone : « Bypass all » et une chaîne
            // illisible ne l'éteignent pas ; hors TESS ou en DSD natif, il
            // n'est pas appliqué en logiciel.
            //
            // T07 — en Pure, il RESTE appliqué : c'est le comportement réel
            // (`audio::audiophile`, `effective_volume`). Seul le verrou
            // `audiophile_lock_volume` (inactif par défaut) le tient à 100 %,
            // et la consigne arrive alors à 0 dB : rien n'est demandé.
            volume.eteindre(*m, d.clone());
        }
        let etat_volume = EtatVolume {
            control: ctx.volume.control,
            placement: volume.effective.then_some(if vers_dsd {
                PlacementVolume::BeforeDsdModulator
            } else {
                PlacementVolume::AtRender
            }),
            db: logiciel.then_some(ctx.volume.db),
        };

        // Étape finale.
        let modifie = headroom.effective
            || greffons.iter().any(|e| e.effective)
            || upsampling.effective
            || volume.effective;
        let finale = if vers_dsd {
            let mut f = etape(CodeEtape::DsdModulator, true);
            f.params = params_upsampling(up);
            f
        } else {
            let mut f = etape(CodeEtape::Quantization, true);
            f.params = Some(json!({ "dither": self.fixed.quantization.dither }));
            if !modifie {
                f.eteindre(MotifEtape::NothingToQuantize, Some("Bit-perfect".into()));
            } else if self.fixed.quantization.dither == Dither::Unknown {
                f.reason = Some(MotifEtape::UnknownValue);
            }
            f
        };

        let badge = if !applicabilite.appliquee() {
            Badge::OutsideTess
        } else if modifie {
            Badge::ProcessedByTess
        } else {
            Badge::BitPerfect
        };

        let mut etapes = Vec::with_capacity(self.plugins.len() + 6);
        etapes.push(source);
        etapes.push(headroom);
        etapes.extend(greffons);
        etapes.push(upsampling);
        etapes.push(volume);
        etapes.push(finale);
        etapes.push(etape(CodeEtape::Output, true));
        Resolution {
            etapes,
            headroom_db,
            volume: etat_volume,
            badge,
        }
    }

    fn resoudre_greffon(
        &self,
        m: &MaillonGreffon,
        catalogue: &Catalogue,
        ctx: &ContexteExecution<'_>,
        canaux: &mut Option<u16>,
    ) -> EtapeResolue {
        let mut e = etape(CodeEtape::Plugin, false);
        e.plugin_id = Some(m.id.clone());
        e.requested = m.enabled;
        e.effective = m.enabled;
        let Some(d) = catalogue.get(&m.id) else {
            if m.enabled {
                e.eteindre(MotifEtape::UnknownPlugin, None);
            }
            return e;
        };
        if !m.enabled {
            return e;
        }
        if !(ctx.greffon_disponible)(&m.id) {
            e.eteindre(MotifEtape::PluginUnavailable, None);
        } else if d.entitlement.demande_le_premium() && !ctx.premium {
            e.eteindre(MotifEtape::PremiumRequired, None);
        } else {
            match d.placement.channels {
                Canaux::StereoOnly | Canaux::Unknown => {
                    if let Some(n) = *canaux
                        && n != 2
                    {
                        e.eteindre(MotifEtape::StereoOnly, Some(format!("{n} channels")));
                    }
                }
                // Le nombre de sortie dépend des réglages ; il n'y a de toute
                // façon plus aucun greffon après (LastPlugin).
                Canaux::Remaps => *canaux = None,
                Canaux::Any => {}
            }
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::chaine::{EtagesFixes, Quantification};
    use crate::tess::greffon::catalogue_de_reference;
    use serde_json::json;

    fn tout_est_installe(_: &str) -> bool {
        true
    }

    fn sans_paquets(id: &str) -> bool {
        !matches!(id, "crossfeed-pro" | "crosstalk")
    }

    fn ctx(transport: TransportZone) -> ContexteExecution<'static> {
        ContexteExecution {
            transport,
            source: Some(SourceFlux::Pcm {
                sample_rate_hz: 96_000,
                bits: 24,
                channels: 2,
            }),
            premium: true,
            greffon_disponible: &tout_est_installe,
            pure: false,
            volume: VolumeZone {
                control: ControleVolume::Software,
                db: -18.0,
            },
        }
    }

    fn maillon(id: &str, enabled: bool) -> MaillonGreffon {
        MaillonGreffon {
            id: id.into(),
            enabled,
            settings: Value::Null,
        }
    }

    fn maquette_a1() -> ChaineTess {
        ChaineTess {
            plugins: vec![
                maillon("convolution", true),
                maillon("equalizer-graphic", true),
                maillon("crossfeed", false),
            ],
            ..ChaineTess::default()
        }
    }

    fn resoudre(c: &ChaineTess, x: &ContexteExecution<'_>) -> Resolution {
        c.resoudre(&catalogue_de_reference(), x)
    }

    fn codes(r: &Resolution) -> Vec<CodeEtape> {
        r.etapes.iter().map(|e| e.code).collect()
    }

    fn effectives(r: &Resolution) -> Vec<CodeEtape> {
        r.etapes
            .iter()
            .filter(|e| e.effective)
            .map(|e| e.code)
            .collect()
    }

    fn greffon<'a>(r: &'a Resolution, id: &str) -> &'a EtapeResolue {
        r.etapes
            .iter()
            .find(|e| e.plugin_id.as_deref() == Some(id))
            .unwrap()
    }

    use CodeEtape::{DsdModulator, Output, Plugin, Quantization, Source, Volume};
    const HEADROOM: CodeEtape = CodeEtape::Headroom;
    const UPSAMPLING: CodeEtape = CodeEtape::Upsampling;

    #[test]
    fn l_ordre_reel_suit_q9_en_pcm() {
        let mut c = maquette_a1();
        c.fixed.upsampling = Upsampling::Pcm {
            filter: "polyvalent".into(),
            max_ratio: Some(2),
            max_rate_hz: None,
        };
        let r = resoudre(&c, &ctx(TransportZone::Oaat));
        assert_eq!(
            codes(&r),
            [
                Source,
                HEADROOM,
                Plugin,
                Plugin,
                Plugin,
                UPSAMPLING,
                Volume,
                Quantization,
                Output
            ]
        );
        // Upsampling effectif AVANT le volume, quantification séparée, en dernier.
        assert_eq!(
            effectives(&r),
            [
                Source,
                HEADROOM,
                Plugin,
                Plugin,
                UPSAMPLING,
                Volume,
                Quantization,
                Output
            ]
        );
        assert_eq!(r.volume.placement, Some(PlacementVolume::AtRender));
        assert_eq!(r.headroom_db, HEADROOM_AUTO_DB);
        assert_eq!(r.badge, Badge::ProcessedByTess);
        let up = r.etapes.iter().find(|e| e.code == UPSAMPLING).unwrap();
        assert_eq!(up.params.as_ref().unwrap()["max_ratio"], 2);
    }

    #[test]
    fn exception_dsd_le_volume_juste_avant_le_modulateur() {
        let mut c = maquette_a1();
        c.fixed.upsampling = Upsampling::Dsd {
            rate: TauxDsd::Dsd128,
            filter: "polyvalent".into(),
        };
        let r = resoudre(&c, &ctx(TransportZone::Local));
        let o = codes(&r);
        let v = o.iter().position(|e| *e == Volume).unwrap();
        assert_eq!(o[v - 1], UPSAMPLING);
        assert_eq!(o[v + 1], DsdModulator);
        assert!(!o.contains(&Quantization));
        assert_eq!(
            r.volume.placement,
            Some(PlacementVolume::BeforeDsdModulator)
        );
        // Contre-épreuve : sans Premium, l'upsampling s'éteint et l'on revient
        // à la quantification PCM, volume au rendu.
        let mut x = ctx(TransportZone::Local);
        x.premium = false;
        let r = resoudre(&c, &x);
        assert!(codes(&r).contains(&Quantization));
        assert_eq!(r.volume.placement, Some(PlacementVolume::AtRender));
    }

    #[test]
    fn hors_tess_rien_n_est_annonce_comme_applique() {
        let mut c = maquette_a1();
        c.fixed.upsampling = Upsampling::Dsd {
            rate: TauxDsd::Dsd64,
            filter: "polyvalent".into(),
        };
        let r = resoudre(&c, &ctx(TransportZone::HqPlayer));
        assert_eq!(r.headroom_db, 0.0, "pas de headroom annoncé hors TESS");
        assert_eq!(r.volume.placement, None, "ni d'ordre DSD");
        assert_eq!(r.badge, Badge::OutsideTess);
        assert_eq!(effectives(&r), [Source, Output]);
        let conv = greffon(&r, "convolution");
        assert!(conv.requested && !conv.effective);
        assert_eq!(conv.reason, Some(MotifEtape::OutsideTess));
        assert!(conv.detail.as_deref().unwrap().contains("HQPlayer"));
        // Contre-épreuve : la même chaîne sur OAAT est appliquée.
        let r = resoudre(&c, &ctx(TransportZone::Oaat));
        assert_eq!(r.headroom_db, HEADROOM_AUTO_DB);
        assert_eq!(
            r.volume.placement,
            Some(PlacementVolume::BeforeDsdModulator)
        );
    }

    #[test]
    fn un_etage_se_degrade_seul_sans_rejeter_la_chaine() {
        // Droit perdu, paquet absent, flux 5.1 sur un greffon stéréo.
        let c = ChaineTess {
            plugins: vec![
                maillon("equalizer-parametric", true),
                maillon("crossfeed-pro", true),
                maillon("crossfeed", true),
                maillon("equalizer-graphic", true),
            ],
            ..ChaineTess::default()
        };
        let mut x = ctx(TransportZone::Local);
        x.premium = false;
        x.greffon_disponible = &sans_paquets;
        let r = resoudre(&c, &x);
        assert_eq!(
            greffon(&r, "equalizer-parametric").reason,
            Some(MotifEtape::PremiumRequired)
        );
        assert_eq!(
            greffon(&r, "crossfeed-pro").reason,
            Some(MotifEtape::PluginUnavailable)
        );
        // Le crossfeed intégré (gratuit) reprend la main : Pro est éteint.
        assert!(greffon(&r, "crossfeed").effective);
        assert!(greffon(&r, "equalizer-graphic").effective);
        x.source = Some(SourceFlux::Pcm {
            sample_rate_hz: 48_000,
            bits: 24,
            channels: 6,
        });
        let r = resoudre(&c, &x);
        let cf = greffon(&r, "crossfeed");
        assert_eq!(cf.reason, Some(MotifEtape::StereoOnly));
        assert_eq!(cf.detail.as_deref(), Some("6 channels"));
        // Contre-épreuve : un greffon qui ne déclare pas « stéréo seulement »
        // reste appliqué en 5.1.
        assert!(greffon(&r, "equalizer-graphic").effective);
        assert_eq!(r.badge, Badge::ProcessedByTess);
    }

    #[test]
    fn le_groupe_du_crossfeed_suit_la_priorite_actuelle() {
        let c = ChaineTess {
            plugins: vec![
                maillon("crossfeed", true),
                maillon("crossfeed-pro", true),
                maillon("crosstalk", true),
            ],
            ..ChaineTess::default()
        };
        let r = resoudre(&c, &ctx(TransportZone::Local));
        // Crosstalk l'emporte sur toute la famille (#7616).
        assert!(greffon(&r, "crosstalk").effective);
        for id in ["crossfeed", "crossfeed-pro"] {
            let e = greffon(&r, id);
            assert_eq!(e.reason, Some(MotifEtape::ReplacedInGroup), "{id}");
            assert_eq!(e.params.as_ref().unwrap()["replaced_by"], "crosstalk");
        }
        // Sans crosstalk, Pro remplace le crossfeed intégré.
        let c = ChaineTess {
            plugins: vec![maillon("crossfeed", true), maillon("crossfeed-pro", true)],
            ..ChaineTess::default()
        };
        let r = resoudre(&c, &ctx(TransportZone::Local));
        assert!(greffon(&r, "crossfeed-pro").effective);
        assert_eq!(
            greffon(&r, "crossfeed").reason,
            Some(MotifEtape::ReplacedInGroup)
        );
        // Un seul membre actif : rien d'écarté.
        let c = ChaineTess {
            plugins: vec![maillon("crossfeed", true), maillon("crosstalk", false)],
            ..ChaineTess::default()
        };
        let r = resoudre(&c, &ctx(TransportZone::Local));
        assert!(greffon(&r, "crossfeed").effective);
        assert_eq!(greffon(&r, "crosstalk").reason, None);
    }

    #[test]
    fn pure_et_chaine_vide_sont_bit_perfect() {
        let mut x = ctx(TransportZone::Local);
        x.pure = true;
        // Pure avec le verrou de volume (consigne tenue à 0 dB).
        x.volume.db = 0.0;
        let r = resoudre(&maquette_a1(), &x);
        assert_eq!(r.badge, Badge::BitPerfect);
        assert_eq!(effectives(&r), [Source, Output]);
        let q = r.etapes.iter().find(|e| e.code == Quantization).unwrap();
        assert!(!q.effective);
        assert_eq!(q.detail.as_deref(), Some("Bit-perfect"));
        // T07 — Pure SANS verrou (le défaut) : le volume logiciel reste
        // appliqué, comme aujourd'hui ; le badge ne ment pas.
        x.volume.db = -18.0;
        let r = resoudre(&maquette_a1(), &x);
        assert_eq!(effectives(&r), [Source, Volume, Quantization, Output]);
        assert_eq!(r.badge, Badge::ProcessedByTess);
        let conv = greffon(&r, "convolution");
        assert_eq!(conv.reason, Some(MotifEtape::PureMode));
        // Chaîne vide et volume à 0 dB : bit-perfect aussi.
        let mut x = ctx(TransportZone::Local);
        x.volume.db = 0.0;
        let r = resoudre(&ChaineTess::default(), &x);
        assert_eq!(r.badge, Badge::BitPerfect);
        assert_eq!(r.headroom_db, 0.0);
        // Contre-épreuve : le seul volume logiciel à −1 dB requantifie.
        x.volume.db = -1.0;
        let r = resoudre(&ChaineTess::default(), &x);
        assert_eq!(effectives(&r), [Source, Volume, Quantization, Output]);
        assert_eq!(r.badge, Badge::ProcessedByTess);
    }

    #[test]
    fn volume_de_l_appareil_et_bypass_all() {
        let mut x = ctx(TransportZone::Dlna);
        x.volume = VolumeZone {
            control: ControleVolume::Device,
            db: -18.0,
        };
        let mut c = maquette_a1();
        c.bypass_all = true;
        let r = resoudre(&c, &x);
        assert_eq!(
            r.volume,
            EtatVolume {
                control: ControleVolume::Device,
                placement: None,
                db: None
            }
        );
        let vol = r.etapes.iter().find(|e| e.code == Volume).unwrap();
        assert_eq!(vol.reason, Some(MotifEtape::VolumeNotSoftware));
        assert_eq!(
            greffon(&r, "convolution").reason,
            Some(MotifEtape::BypassAll)
        );
        assert_eq!(r.badge, Badge::BitPerfect);
        assert_eq!(r.headroom_db, 0.0);
    }

    #[test]
    fn une_source_dsd_native_n_est_pas_traitee() {
        let mut x = ctx(TransportZone::Local);
        x.source = Some(SourceFlux::Dsd {
            rate: TauxDsd::Dsd64,
            channels: 2,
            played_as: LectureDsd::Native,
        });
        let r = resoudre(&maquette_a1(), &x);
        assert_eq!(r.badge, Badge::BitPerfect);
        assert_eq!(
            greffon(&r, "convolution").reason,
            Some(MotifEtape::DsdSourceNative)
        );
        let src = &r.etapes[0];
        assert_eq!(src.params.as_ref().unwrap()["kind"], "dsd");
        // Contre-épreuve : convertie en PCM par le transport, la chaîne s'applique.
        x.source = Some(SourceFlux::Dsd {
            rate: TauxDsd::Dsd64,
            channels: 2,
            played_as: LectureDsd::ConvertedToPcm,
        });
        assert!(greffon(&resoudre(&maquette_a1(), &x), "convolution").effective);
    }

    #[test]
    fn une_chaine_plus_recente_s_eteint_avec_un_motif() {
        let texte = r#"{"version":2,"plugins":[{"id":"convolution","enabled":true}],
            "fixed":{"upsampling":{"mode":"oversample_x3"}}}"#;
        let c: ChaineTess = serde_json::from_str(texte).unwrap();
        let r = resoudre(&c, &ctx(TransportZone::Local));
        assert_eq!(
            greffon(&r, "convolution").reason,
            Some(MotifEtape::ChainVersionUnsupported)
        );
        // Contre-épreuve : en v1, seule la valeur inconnue s'éteint.
        let c = ChaineTess { version: 1, ..c };
        let r = resoudre(&c, &ctx(TransportZone::Local));
        assert!(greffon(&r, "convolution").effective);
        let up = r.etapes.iter().find(|e| e.code == UPSAMPLING).unwrap();
        assert_eq!(up.reason, Some(MotifEtape::UnknownValue));
    }

    #[test]
    fn headroom_automatique_seulement_s_il_protege_un_traitement() {
        let mut x = ctx(TransportZone::Local);
        x.premium = false;
        // Seul greffon coché : Premium, donc éteint ; le headroom n'a rien à protéger.
        let c = ChaineTess {
            plugins: vec![maillon("convolution", true)],
            ..ChaineTess::default()
        };
        let r = resoudre(&c, &x);
        let h = r.etapes.iter().find(|e| e.code == HEADROOM).unwrap();
        assert!(h.requested && !h.effective);
        assert_eq!(h.reason, Some(MotifEtape::NothingToProtect));
        assert_eq!(r.headroom_db, 0.0);
        // Contre-épreuve : avec le droit, −3 dB.
        x.premium = true;
        assert_eq!(resoudre(&c, &x).headroom_db, HEADROOM_AUTO_DB);
        // Manuel : appliqué tel quel.
        let c = ChaineTess {
            fixed: EtagesFixes {
                headroom: Headroom::Manual { db: -6.0 },
                upsampling: Upsampling::default(),
                quantization: Quantification::default(),
            },
            ..ChaineTess::default()
        };
        assert_eq!(resoudre(&c, &x).headroom_db, -6.0);
    }

    #[test]
    fn les_etapes_resolues_font_l_aller_retour_json() {
        let r = resoudre(&maquette_a1(), &ctx(TransportZone::Oaat));
        for e in &r.etapes {
            let v = serde_json::to_value(e).unwrap();
            assert_eq!(serde_json::from_value::<EtapeResolue>(v).unwrap(), *e);
        }
        // Un client v1 lit une étape plus récente.
        let futur = json!({"code":"limiter","plugin_id":null,"fixed":true,"requested":true,
            "effective":true,"reason":"thermal","detail":null,"params":{"ceiling":-1},
            "latency_frames":64,"color":"red"});
        let e: EtapeResolue = serde_json::from_value(futur).unwrap();
        assert_eq!(e.code, CodeEtape::Unknown);
        assert_eq!(e.reason, Some(MotifEtape::Unknown));
    }
}
