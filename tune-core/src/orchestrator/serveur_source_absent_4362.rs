//! #4362 — le chemin de lecture CONSULTE enfin ce que la bibliothèque sait
//! déjà : « Serveur absent ».
//!
//! # Le défaut
//!
//! Une piste indexée depuis un serveur multimédia UPnP (#2219) porte son
//! adresse de lecture dans l'instantané, `track_metadata.upnp_res_url`, et son
//! IDENTITÉ dans `tracks.source_id` — un condensat `<udn>|<hex>`.
//! `resolve_direct` refusait déjà, avec un message clair, une piste **sans**
//! URL. Il ne se demandait pas si le SERVEUR de cette URL répondait encore :
//! l'URL existait, elle partait. Sur le `.18` le 17/09/2026, avec Asset arrêté
//! sur le Mac Studio (port 26125 refusé), la chaîne complète s'exécutait sans
//! une seule erreur — `upnp_url_de_lecture_lue_dans_l_instantane`,
//! `output_play_sent`, `orchestrator_play` — la zone passait « en lecture », et
//! l'Eversolo jouait du **silence**.
//!
//! Pendant ce temps, l'écran Bibliothèque affichait « Serveur absent » sur les
//! 27 albums de ce serveur. La connaissance existait ; elle n'était lue que par
//! la liste.
//!
//! # Où cette connaissance vit
//!
//! Dans le registre DURABLE `media_servers`
//! (`db/media_server_repo.rs`, #2219 phase 1) et dans la qualification PURE
//! qui en tire un verdict, [`qualifier_le_registre`]
//! (`discovery/presence_serveur.rs`). C'est exactement ce que
//! `GET /network/media-servers` publie sous `presence` / `proposable`, et donc
//! ce que le badge « Serveur absent » montre.
//!
//! # Ce que ce module N'invente PAS
//!
//! **Aucun seuil.** Le seuil des 24 h (`SERVEUR_ABSENT_APRES`), le plafond de
//! bascule en masse (`PART_MAX_ABSENCE_SIMULTANEE`, qui protège d'une coupure
//! de NOTRE lien réseau) et la confirmation doublée sont ceux de la
//! qualification, appelée telle quelle, sur le registre entier. Refaire ici un
//! « serveur absent » maison aurait produit deux horloges pour une seule
//! question — le travers que la phase 1 a payé et documenté.
//!
//! **Aucune sonde réseau.** L'issue en ouvre la possibilité (« HEAD /
//! connexion TCP, délai borné »), et la range elle-même dans « Non établi » :
//! le coût par piste sur une file longue n'est pas tranché. Ce module s'en
//! tient au fait acquis — le registre SAIT —, et une sonde pourra s'ajouter
//! devant lui sans rien déplacer.

use crate::db::media_server_repo::ServeurEnregistre;
use crate::discovery::presence_serveur::{
    ObservationServeur, PresenceServeur, RaisonAbsence, qualifier_le_registre,
};

/// L'UDN du serveur d'où vient une piste indexée, lu dans son `source_id`.
///
/// `tracks.source_id` porte `<udn>|<hex>` depuis la phase 2 de #2219, et
/// l'indexation s'en sert déjà dans l'autre sens : `identites_upnp.rs`
/// rapproche par le préfixe `format!("{udn}|")`. On lit donc ici la même
/// convention, et rien d'autre.
///
/// Rend `None` sur tout ce qui n'est pas ce condensat — un `source_id` qui est
/// une URL (`radio`, `podcast`, `bandcamp`), une chaîne vide, un préfixe vide.
/// `None` ne veut jamais dire « absent » : il veut dire « ce module n'a rien à
/// dire », et l'appelant laisse alors passer.
pub(super) fn udn_de_la_piste(source_id: &str) -> Option<&str> {
    let (udn, reste) = source_id.split_once('|')?;
    (!udn.is_empty() && !reste.is_empty()).then_some(udn)
}

/// Ce que le registre dit du serveur d'une piste, quand il en dit du mal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ServeurAbsent {
    pub nom: String,
    pub hote: Option<String>,
    pub depuis_secs: i64,
    pub raison: RaisonAbsence,
}

/// Le serveur `udn` est-il ABSENT, au sens exact de la bibliothèque ?
///
/// Le verdict porte sur la LISTE, jamais sur la ligne — c'est la règle de
/// `qualifier_le_registre`, et le plafond de bascule en masse n'a pas d'autre
/// moyen de se juger. On lui passe donc le registre entier, puis on lit la
/// case qui nous concerne.
///
/// Rend `None` quand le serveur est présent, **et aussi quand il est inconnu
/// du registre** : une ligne indexée dont le serveur n'a jamais été inscrit
/// n'est pas une ligne dont le serveur est éteint. On ne refuse que ce qu'on
/// SAIT, dans le sens où l'absence de preuve n'est pas une preuve d'absence.
pub(super) fn serveur_absent(registre: &[ServeurEnregistre], udn: &str) -> Option<ServeurAbsent> {
    if !registre.iter().any(|s| s.udn == udn) {
        return None;
    }
    let ages: Vec<Option<i64>> = registre.iter().map(ServeurEnregistre::age_secs).collect();
    let observations: Vec<ObservationServeur<'_>> = registre
        .iter()
        .zip(&ages)
        .map(|(s, age)| ObservationServeur {
            udn: &s.udn,
            age_secs: *age,
            disparition_confirmee: s.absence_reason.as_deref() == Some("disparition_confirmee"),
        })
        .collect();
    let ligne = registre.iter().find(|s| s.udn == udn)?;
    match qualifier_le_registre(&observations).pour(udn)? {
        PresenceServeur::Present => None,
        PresenceServeur::Absent {
            depuis_secs,
            raison,
        } => Some(ServeurAbsent {
            nom: if ligne.name.trim().is_empty() {
                udn.to_string()
            } else {
                ligne.name.clone()
            },
            hote: ligne
                .host
                .as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string),
            depuis_secs,
            raison,
        }),
    }
}

/// Le refus, tel qu'il est DIT à l'auditeur.
///
/// Il NOMME le serveur — c'est la demande explicite de l'issue, et c'est la
/// seule information qui rende le message actionnable : « ne répond pas » sans
/// dire QUI laisse l'auditeur devant sa bibliothèque entière.
///
/// Langue : le français, comme le refus voisin de `resolve_direct`
/// (« Lecture impossible : … aucune URL de lecture n'est enregistrée ») et
/// comme `motif_du_refus_oaat`. Les motifs d'erreur de l'orchestrateur ne
/// passent par aucun catalogue de traduction dans ce dépôt — il n'y en a pas —
/// et en inventer un pour une phrase aurait fait diverger ce chemin de ses
/// voisins.
pub(super) fn motif_du_refus(absent: &ServeurAbsent) -> String {
    let ou = match &absent.hote {
        Some(h) => format!(" ({h})"),
        None => String::new(),
    };
    let depuis = match absent.raison {
        RaisonAbsence::JamaisObserve => " (jamais observé depuis ce Tune)".to_string(),
        _ => format!(" (plus revu depuis {})", duree_en_clair(absent.depuis_secs)),
    };
    format!(
        "Lecture impossible : le serveur multimédia « {}{ou} » ne répond pas{depuis}. \
         Démarrez-le ou vérifiez le réseau, puis réessayez. Tune ne lui envoie pas \
         cette piste : le lecteur réseau irait chercher les octets chez lui et \
         jouerait du silence.",
        absent.nom
    )
}

/// #4362 (point 2) — ce qui est DIT quand une file enjambe des pistes dont le
/// serveur est absent, au lieu de refuser tout « Tout lire » sur la première.
///
/// `absent` est le serveur de la PREMIÈRE piste enjambée : une file mixte en
/// compte rarement plusieurs, et nommer le premier suffit à rendre le message
/// actionnable. `reprise` est le titre de la piste qui part à la place.
pub(super) fn motif_de_l_enjambee(
    absent: &ServeurAbsent,
    n: usize,
    reprise: Option<&str>,
) -> String {
    let ou = match &absent.hote {
        Some(h) => format!(" ({h})"),
        None => String::new(),
    };
    let pistes = if n == 1 {
        "1 piste sautée".to_string()
    } else {
        format!("{n} pistes sautées")
    };
    let suite = match reprise.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!(" Lecture reprise à « {t} »."),
        None => String::new(),
    };
    format!(
        "{pistes} : le serveur multimédia « {}{ou} » ne répond pas.{suite}",
        absent.nom
    )
}

/// Une durée en secondes, dite comme on la dit à voix haute.
///
/// Trois paliers seulement : l'auditeur veut savoir si c'est « tout à l'heure »
/// ou « avant-hier », pas à la seconde près.
fn duree_en_clair(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=3_599 => format!("{} min", s / 60),
        3_600..=86_399 => format!("{} h", s / 3_600),
        _ => format!("{} j", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::media_server_repo::horodatage_il_y_a;

    fn serveur(udn: &str, nom: &str, age_secs: i64) -> ServeurEnregistre {
        ServeurEnregistre {
            udn: udn.into(),
            name: nom.into(),
            manufacturer: None,
            model: None,
            device_type: "upnp_media_server".into(),
            location: "http://192.168.1.41:26125/desc.xml".into(),
            content_directory_url: None,
            host: Some("192.168.1.41".into()),
            port: Some(26_125),
            max_age_secs: Some(1_800),
            first_seen_at: horodatage_il_y_a(age_secs),
            last_seen_at: horodatage_il_y_a(age_secs),
            active: true,
            last_state: None,
            absence_reason: None,
        }
    }

    #[test]
    fn l_udn_se_lit_dans_le_condensat_et_nulle_part_ailleurs() {
        assert_eq!(
            udn_de_la_piste("uuid:258FC2D5-E2C3|9f3c4d"),
            Some("uuid:258FC2D5-E2C3")
        );
        // Un `source_id` de radio/podcast/bandcamp EST l'URL : rien à dire.
        assert_eq!(udn_de_la_piste("https://stream.example/live.mp3"), None);
        assert_eq!(udn_de_la_piste("uuid:sans-barre"), None);
        assert_eq!(udn_de_la_piste("|9f3c"), None);
        assert_eq!(udn_de_la_piste("uuid:rien-apres|"), None);
    }

    #[test]
    fn un_serveur_vu_a_l_instant_ne_bloque_rien() {
        let registre = vec![serveur("uuid:a", "Asset UPnP: Mac-Studio-6", 30)];
        assert_eq!(serveur_absent(&registre, "uuid:a"), None);
    }

    #[test]
    fn un_serveur_silencieux_depuis_deux_jours_est_absent_et_se_nomme() {
        let registre = vec![serveur("uuid:a", "Asset UPnP: Mac-Studio-6", 2 * 86_400)];
        let absent = serveur_absent(&registre, "uuid:a").expect("le registre le dit absent");
        assert_eq!(absent.raison, RaisonAbsence::SilenceProlonge);
        let motif = motif_du_refus(&absent);
        assert!(
            motif.contains("Asset UPnP: Mac-Studio-6") && motif.contains("192.168.1.41"),
            "le refus doit NOMMER le serveur et son adresse — lu : {motif}"
        );
    }

    #[test]
    fn un_serveur_inconnu_du_registre_n_est_pas_un_serveur_eteint() {
        let registre = vec![serveur("uuid:a", "Asset", 30)];
        assert_eq!(
            serveur_absent(&registre, "uuid:jamais-inscrit"),
            None,
            "l'absence de preuve n'est pas une preuve d'absence : une ligne \
             indexée dont le serveur n'est pas au registre doit continuer de \
             partir, sans quoi ce correctif casserait des lectures qui marchent"
        );
    }

    #[test]
    fn le_plafond_de_bascule_en_masse_protege_la_lecture() {
        // Quatre serveurs, tous muets depuis 25 h : au seuil simple, ils
        // basculeraient tous. Le plafond (la moitié du registre) refuse, et la
        // lecture ne doit PAS être refusée — c'est notre lien réseau qui est
        // tombé, pas quatre NAS qu'on aurait éteints ensemble.
        let registre: Vec<ServeurEnregistre> = (0..4)
            .map(|i| serveur(&format!("uuid:{i}"), &format!("Serveur {i}"), 25 * 3_600))
            .collect();
        assert_eq!(
            serveur_absent(&registre, "uuid:0"),
            None,
            "sous le plafond de bascule en masse, la lecture reste permise"
        );
    }

    #[test]
    fn l_enjambee_se_dit_en_nommant_le_serveur_et_la_reprise() {
        let registre = vec![serveur("uuid:a", "Asset UPnP: Mac-Studio-6", 2 * 86_400)];
        let absent = serveur_absent(&registre, "uuid:a").expect("absent");
        let m = motif_de_l_enjambee(&absent, 3, Some("So What"));
        assert!(m.starts_with("3 pistes sautées"), "{m}");
        assert!(
            m.contains("Asset UPnP: Mac-Studio-6") && m.contains("192.168.1.41"),
            "{m}"
        );
        assert!(m.contains("« So What »"), "{m}");
        assert!(motif_de_l_enjambee(&absent, 1, None).starts_with("1 piste sautée :"));
    }

    #[test]
    fn la_duree_se_dit_en_minutes_en_heures_puis_en_jours() {
        assert_eq!(duree_en_clair(59), "0 min");
        assert_eq!(duree_en_clair(3_599), "59 min");
        assert_eq!(duree_en_clair(3_600), "1 h");
        assert_eq!(duree_en_clair(86_399), "23 h");
        assert_eq!(duree_en_clair(2 * 86_400), "2 j");
        assert_eq!(duree_en_clair(-5), "0 min");
    }
}
