//! La DISPOSITION DE CANAUX déclarée pour une zone — chantier « multicanal ».
//!
//! # La demande
//!
//! Bertrand, 19/09/2026 : « dans les réglages de l'appareil, permettre la
//! sélection du nombre de canaux », pour des utilisateurs en 5.1 — *« Je ne
//! peux pas le tester chez moi »*.
//!
//! # Ce qui existait déjà, et qu'on ne réécrit pas
//!
//! * les neuf dispositions nommées ([`crate::audio::channels::ChannelLayout`]) ;
//! * `output_capabilities.channel_layouts`, servi par `GET /zones`, DÉRIVÉ de
//!   `max_channels` ;
//! * le chemin du signal, qui dit déjà la vérité MESURÉE — « 6 → 2 canaux
//!   (mesuré) » quand le périphérique n'a pas ouvert ce qu'on lui demandait ;
//! * `cascade_de_formats`, qui porte le nombre de canaux et retombe sur la
//!   configuration de la source : le chemin local n'est pas figé en stéréo.
//!
//! Ce qui manque est le CHOIX : `max_channels` vaut `None` sur les quinze zones
//! du .18, `GET /devices/audio` y rend zéro sortie, et les dispositions étant
//! dérivées de `max_channels`, la liste à proposer est vide. Il n'y a
//! aujourd'hui rien à sélectionner, quel que soit le sens qu'on donne au
//! réglage.
//!
//! # 🔴 DÉCLARER, jamais FORCER
//!
//! On mémorise « mon DAC est en 5.1 » et on s'en sert pour PROPOSER et pour
//! ÉTIQUETER. On ne fabrique aucun canal. Un upmix qu'on ne peut éprouver
//! nulle part — ni chez Bertrand, ni ici — risquerait de rendre quatre voies
//! muettes sur six sans que personne le voie, et le défaut ne remonterait que
//! par un testeur, des semaines plus tard.
//!
//! # 🔴 Et un réglage qui ne s'applique pas doit le DIRE
//!
//! C'est la leçon de [`super::mono_downmix`] (#3254) : `zone_{id}_mono_downmix`
//! était accepté, persisté et relu sur n'importe quelle zone, mais n'agissait
//! que derrière `device_id.starts_with("local:")`. Sur une zone réseau :
//! accepté, persisté, relu… et sans effet. *« Le défaut n'est pas la règle —
//! le défaut est que le réglage ment. »*
//!
//! Le même piège guette ici, et plus fort : un renderer DLNA ou AirPlay
//! NÉGOCIE son format avec le serveur. Lui déclarer 5.1 ne change rien à ce
//! qu'il acceptera. On publie donc le même triplet
//! `requested` / `effective` / `unavailable`, avec le `reason` / `detail`
//! d'`ExclusiveModeStatus` et de `CrossfeedStatus`.

use super::channels::ChannelLayout;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use serde::Serialize;
use std::sync::Arc;

/// Pourquoi la disposition déclarée n'aura pas d'effet.
///
/// Codes STABLES, destinés à la machine ; le client les traduit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanauxContrainte {
    /// La zone ne sort ni par une carte son locale, ni par un renderer RÉSEAU
    /// dont Tune décode le flux ([`PorteeDeSortie`]) : AirPlay, OAAT, zone
    /// sans appareil. Ce qu'on déclare ici ne l'atteint pas.
    ///
    /// Fils 1914/1913 : ce motif couvrait aussi DLNA, OpenHome, Chromecast,
    /// BluOS et Squeezebox — alors que ce chemin sait replier une piste
    /// multicanale (#4573). Ils portent désormais la déclaration comme
    /// PLAFOND. Le code reste `sortie_non_locale` : c'est un contrat client.
    SortieNonLocale,
    /// L'appareil annonce moins de canaux que la disposition déclarée. On ne
    /// bloque pas la saisie — un pilote ment parfois, et l'utilisateur en sait
    /// plus que lui — mais on ne prétend pas non plus que ça marchera.
    AuDelaDeLAppareil,
}

impl CanauxContrainte {
    pub fn code(self) -> &'static str {
        match self {
            Self::SortieNonLocale => "sortie_non_locale",
            Self::AuDelaDeLAppareil => "au_dela_de_l_appareil",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Self::SortieNonLocale => {
                "cette zone ne sort pas par une carte son locale : le renderer négocie son propre format"
            }
            Self::AuDelaDeLAppareil => {
                "l'appareil annonce moins de canaux que la disposition choisie"
            }
        }
    }
}

/// Ce que vaut la disposition déclarée pour cette zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanauxStatus {
    /// Ce que l'utilisateur a déclaré. `None` = rien de déclaré, on suit
    /// l'appareil.
    pub requested: Option<ChannelLayout>,
    /// Ce dont le serveur tiendra compte.
    pub effective: Option<ChannelLayout>,
    /// `true` dès que la contrainte s'applique — **y compris quand rien n'a
    /// été déclaré**. C'est ce champ qui VERROUILLE le contrôle à l'écran : la
    /// question n'est pas « a-t-on choisi ? » mais « ce choix a-t-il un sens
    /// ici ? ».
    pub unavailable: bool,
    pub reason: Option<CanauxContrainte>,
    pub detail: Option<&'static str>,
    /// Fils 1914/1913 — comment la déclaration agit sur cette sortie, quand
    /// elle n'y agit pas comme sur une carte locale. `plafond_reseau` : Tune
    /// réduit une piste multicanale au nombre choisi quand il décode le flux
    /// (jamais d'ajout de canal, un DSD servi tel quel n'est pas touché).
    /// Absent partout ailleurs. Code STABLE ; le client le traduit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portee: Option<&'static str>,
}
/// Où la disposition déclarée a un chemin — fils 1914/1913.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PorteeDeSortie {
    /// Carte son locale : [`canaux_portes_par_la_sortie`].
    Locale,
    /// Renderer réseau dont Tune décode le flux
    /// ([`crate::orchestrator::is_network_output_type`]) : la déclaration y
    /// plafonne les canaux servis ([`super::canaux_reseau_4573`]).
    ReseauPlafond,
    /// Aucun chemin : AirPlay, OAAT, zone sans appareil.
    Aucune,
}
/// La portée de la déclaration sur la sortie d'une zone.
pub fn portee_de_la_sortie(
    output_device_id: Option<&str>,
    output_type: Option<&str>,
) -> PorteeDeSortie {
    if canaux_portes_par_la_sortie(output_device_id) {
        PorteeDeSortie::Locale
    } else if output_device_id.is_some() && crate::orchestrator::is_network_output_type(output_type)
    {
        PorteeDeSortie::ReseauPlafond
    } else {
        PorteeDeSortie::Aucune
    }
}
/// La clé de réglage de la disposition déclarée d'une zone.
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_channel_layout")
}
/// La disposition déclarée pour une zone, ou `None` (« suivre l'appareil »,
/// ou valeur inconnue en base — jamais devinée).
pub fn disposition_declaree(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<ChannelLayout> {
    let v = SettingsRepo::with_backend(db.clone())
        .get(&cle_de_zone(zone_id))
        .ok()
        .flatten()?;
    ChannelLayout::TOUTES
        .iter()
        .copied()
        .find(|d| d.as_str() == v)
}
/// [`canaux_status`] selon la portée de la sortie : une zone réseau que Tune
/// décode n'est plus « indisponible », elle dit comment la déclaration agit.
pub fn canaux_status_pour(
    requested: Option<ChannelLayout>,
    portee: PorteeDeSortie,
    max_channels: Option<u16>,
) -> CanauxStatus {
    let mut s = canaux_status(requested, portee != PorteeDeSortie::Aucune, max_channels);
    if portee == PorteeDeSortie::ReseauPlafond {
        s.portee = Some("plafond_reseau");
    }
    s
}

/// La disposition déclarée a-t-elle un chemin sur CETTE sortie ?
///
/// Exactement le prédicat de [`super::mono_downmix::mono_downmix_runs_on_output`].
/// Une zone sans périphérique rend `false` : elle n'a aucune sortie locale.
pub fn canaux_portes_par_la_sortie(output_device_id: Option<&str>) -> bool {
    output_device_id.is_some_and(|id| id.starts_with("local:"))
}

/// La règle, isolée de toute base et de tout `cfg`.
///
/// L'ordre des contraintes n'est pas neutre : une sortie NON LOCALE prime sur
/// le dépassement de capacité. Réduire la disposition ne rendrait rien à une
/// zone réseau, alors que l'inverse est vrai sur une zone locale — la raison
/// publiée doit être celle qui reste vraie quand l'autre disparaît.
pub fn canaux_status(
    requested: Option<ChannelLayout>,
    output_is_local: bool,
    max_channels: Option<u16>,
) -> CanauxStatus {
    let depasse = match (requested, max_channels) {
        // `max_channels == 0` veut dire « on ne sait pas », et « on ne sait
        // pas » ne se lit pas comme « zéro canal » : on ne contraint rien.
        (Some(d), Some(m)) if m > 0 => d.channel_count() > m,
        _ => false,
    };
    let reason = if !output_is_local {
        Some(CanauxContrainte::SortieNonLocale)
    } else if depasse {
        Some(CanauxContrainte::AuDelaDeLAppareil)
    } else {
        None
    };
    let unavailable = reason.is_some();
    CanauxStatus {
        requested,
        effective: if unavailable { None } else { requested },
        unavailable,
        reason,
        detail: reason.map(CanauxContrainte::detail),
        portee: None,
    }
}

/// Les dispositions à PROPOSER pour une zone.
///
/// 🔴 Quand l'appareil se tait — `max_channels` absent ou nul, ce qui est le
/// cas de TOUTES les zones mesurées — on propose la liste complète plutôt
/// qu'une liste vide. C'est tout le sujet : sans cela il n'y a rien à
/// sélectionner, et l'utilisateur en sait plus que son pilote.
///
/// Quand il annonce quelque chose, on s'y tient : proposer 7.1 sur une carte
/// qui déclare 2 voies serait promettre ce qu'elle a elle-même démenti.
pub fn dispositions_a_proposer(max_channels: Option<u16>) -> Vec<ChannelLayout> {
    match max_channels {
        Some(m) if m > 0 => ChannelLayout::jusqu_a(m),
        _ => ChannelLayout::TOUTES.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: bool = true;
    const RESEAU: bool = false;

    #[test]
    fn le_prefixe_local_est_le_meme_que_celui_du_repli_mono() {
        assert!(canaux_portes_par_la_sortie(Some("local:hw:0,0")));
        assert!(!canaux_portes_par_la_sortie(Some("dlna:uuid-123")));
        assert!(!canaux_portes_par_la_sortie(Some("airplay:AppleTV")));
        // Une zone orpheline n'a aucune sortie locale.
        assert!(!canaux_portes_par_la_sortie(None));
        // La MÊME règle que le repli mono, pour qu'écran et son ne divergent pas.
        for id in [Some("local:x"), Some("dlna:x"), None] {
            assert_eq!(
                canaux_portes_par_la_sortie(id),
                super::super::mono_downmix::mono_downmix_runs_on_output(id),
                "{id:?}"
            );
        }
    }

    #[test]
    fn sur_une_sortie_locale_la_declaration_est_honoree() {
        let s = canaux_status(Some(ChannelLayout::Surround51), LOCAL, Some(6));
        assert_eq!(s.effective, Some(ChannelLayout::Surround51));
        assert!(!s.unavailable);
        assert!(s.reason.is_none());
    }

    /// 🔴 La leçon de #3254 : un réglage sans effet doit le DIRE.
    #[test]
    fn sur_une_zone_reseau_le_reglage_se_declare_indisponible() {
        let s = canaux_status(Some(ChannelLayout::Surround51), RESEAU, Some(8));
        assert_eq!(
            s.requested,
            Some(ChannelLayout::Surround51),
            "on n'efface pas ce qu'il a choisi"
        );
        assert_eq!(s.effective, None, "mais rien ne s'applique");
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CanauxContrainte::SortieNonLocale));
        assert!(s.detail.is_some());
    }

    /// 🔴 …y compris quand RIEN n'a été déclaré : c'est `unavailable` qui
    /// verrouille le contrôle, pas la présence d'un choix.
    #[test]
    fn l_indisponibilite_ne_depend_pas_d_un_choix_deja_fait() {
        let s = canaux_status(None, RESEAU, Some(8));
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CanauxContrainte::SortieNonLocale));
    }

    #[test]
    fn au_dela_de_ce_que_l_appareil_annonce_on_le_dit_sans_bloquer() {
        let s = canaux_status(Some(ChannelLayout::Surround714), LOCAL, Some(2));
        assert_eq!(
            s.requested,
            Some(ChannelLayout::Surround714),
            "la saisie n'est pas refusée"
        );
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CanauxContrainte::AuDelaDeLAppareil));
    }

    /// 🔴 « On ne sait pas » ne se lit PAS comme « zéro canal » — et c'est le
    /// cas de TOUTES les zones mesurées sur le .18.
    #[test]
    fn un_appareil_muet_ne_contraint_rien() {
        for muet in [None, Some(0)] {
            let s = canaux_status(Some(ChannelLayout::Surround714), LOCAL, muet);
            assert!(!s.unavailable, "max_channels={muet:?}");
            assert_eq!(s.effective, Some(ChannelLayout::Surround714));
        }
    }

    /// L'ordre des contraintes : une sortie réseau PRIME sur le dépassement.
    #[test]
    fn la_sortie_reseau_prime_sur_le_depassement() {
        let s = canaux_status(Some(ChannelLayout::Immersive32), RESEAU, Some(2));
        assert_eq!(
            s.reason,
            Some(CanauxContrainte::SortieNonLocale),
            "réduire la disposition ne rendrait rien à une zone réseau"
        );
    }

    /// 🔴 Sans cela, il n'y a RIEN à sélectionner : c'est tout le chantier.
    #[test]
    fn un_appareil_muet_fait_proposer_la_liste_complete() {
        for muet in [None, Some(0)] {
            assert_eq!(
                dispositions_a_proposer(muet).len(),
                ChannelLayout::TOUTES.len(),
                "{muet:?}"
            );
        }
    }

    #[test]
    fn un_appareil_qui_parle_borne_la_liste() {
        let six = dispositions_a_proposer(Some(6));
        assert!(six.contains(&ChannelLayout::Surround51));
        assert!(!six.contains(&ChannelLayout::Surround71), "{six:?}");
        // Proposer 7.1 sur une carte qui déclare 2 voies serait promettre ce
        // qu'elle a elle-même démenti.
        let deux = dispositions_a_proposer(Some(2));
        assert_eq!(deux, vec![ChannelLayout::Mono, ChannelLayout::Stereo]);
    }
    /// 🔴 Fils 1914/1913 — un renderer DLNA n'est plus verrouillé : la
    /// déclaration y a un chemin, le plafond de #4573.
    #[test]
    fn une_zone_dlna_porte_la_declaration_comme_plafond() {
        let p = portee_de_la_sortie(Some("dlna:uuid-denon"), Some("dlna"));
        assert_eq!(p, PorteeDeSortie::ReseauPlafond);
        let s = canaux_status_pour(Some(ChannelLayout::Stereo), p, None);
        assert!(!s.unavailable);
        assert_eq!(s.reason, None);
        assert_eq!(s.effective, Some(ChannelLayout::Stereo));
        assert_eq!(s.portee, Some("plafond_reseau"));
        // Rien de choisi : libre aussi — « Suivre l'appareil ».
        assert!(!canaux_status_pour(None, p, None).unavailable);
    }
    /// Ce qui n'a toujours aucun chemin reste verrouillé, et le dit.
    #[test]
    fn airplay_oaat_et_zone_sans_appareil_restent_verrouilles() {
        for (id, t) in [
            (Some("airplay:AppleTV"), Some("airplay")),
            (Some("oaat:x"), Some("oaat")),
            (None, Some("dlna")),
        ] {
            let p = portee_de_la_sortie(id, t);
            assert_eq!(p, PorteeDeSortie::Aucune, "{id:?} {t:?}");
            let s = canaux_status_pour(None, p, None);
            assert!(s.unavailable);
            assert_eq!(s.reason, Some(CanauxContrainte::SortieNonLocale));
            assert_eq!(s.portee, None);
        }
    }
    #[test]
    fn une_sortie_locale_ne_change_pas() {
        let p = portee_de_la_sortie(Some("local:DAC"), Some("local"));
        assert_eq!(p, PorteeDeSortie::Locale);
        assert_eq!(
            canaux_status_pour(Some(ChannelLayout::Surround51), p, Some(6)),
            canaux_status(Some(ChannelLayout::Surround51), true, Some(6))
        );
    }
}
