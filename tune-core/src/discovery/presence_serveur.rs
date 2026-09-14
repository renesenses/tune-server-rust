//! Phase 1 du chantier « unifier les serveurs UPnP et la bibliothèque »
//! (#2219) : la FRAÎCHEUR du registre des serveurs multimédia.
//!
//! Le registre disait deux choses fausses en même temps sur le `.18`, le
//! 13/09/2026 : trois serveurs vus il y a **84 194 s** (23 h 23) y figuraient
//! encore comme des serveurs à proposer, et deux Sonos allumés n'y figuraient
//! pas. Le second défaut est de la DÉCOUVERTE (voir le journal de la PR) ;
//! celui-ci est du registre, et c'est lui qu'on répare ici : un serveur qu'on
//! n'a plus revu depuis assez longtemps doit le DIRE.
//!
//! Rien n'est supprimé. C'est la doctrine déjà écrite trois fois dans le
//! dépôt, et on ne fait que l'appliquer une quatrième :
//!
//! - `ssdp.rs:33-41` (forum 1425) — « marquer ceux qui ne répondent plus
//!   plutôt que de les retirer » ;
//! - `zones/presence.rs:19` — `RECENTE_SECS`, un champ purement descriptif,
//!   qui ne masque ni ne supprime rien ;
//! - `favorites_reconcile.rs:22` — on ne supprime qu'après une observation
//!   COMPLÈTE et saine.
//!
//! Ce module est PUR : pas d'horloge, pas de réseau, pas de base. Il prend des
//! âges en secondes et rend un verdict. C'est ce qui le rend testable sans
//! fabriquer d'`Instant` dans le passé — piège documenté à `ssdp.rs:168-172`,
//! où `Instant::checked_sub` rend `None` sur une machine démarrée depuis moins
//! longtemps que le recul demandé.

use std::time::Duration;

/// Au-delà de ce silence, un serveur multimédia est **absent** : il reste dans
/// le registre, il reste visible et cherchable, mais il cesse d'être proposé.
///
/// # Pourquoi 5 400 s, et pas un chiffre deviné
///
/// La valeur est TROIS FOIS le `max-age` que les serveurs du réseau annoncent
/// réellement. Mesure du 13/09/2026, M-SEARCH `ST:
/// urn:schemas-upnp-org:device:MediaServer:1` depuis le réseau local — les
/// cinq serveurs multimédia joignables répondent en moins de 500 ms et
/// annoncent tous la même chose :
///
/// ```text
/// 192.168.1.18  Tune/0.9.147          CACHE-CONTROL: max-age=1800
/// 192.168.1.42  Tune/0.9.146          CACHE-CONTROL: max-age=1800
/// 192.168.1.15  Tune/0.9.120          CACHE-CONTROL: max-age=1800
/// 192.168.1.20  Sonos/86.8-78270      CACHE-CONTROL: max-age = 1800
/// 192.168.1.19  Sonos/86.8-78270      CACHE-CONTROL: max-age = 1800
/// ```
///
/// 1 800 s est aussi le plancher qu'impose UPnP Device Architecture 1.1
/// §1.2.2, qui demande au device de se réannoncer AVANT l'échéance. Un serveur
/// conforme se manifeste donc au moins toutes les ~900 s, et le silence de
/// 1 800 s est la tolérance que le protocole définit lui-même —
/// c'est exactement le raisonnement de `MEDIA_SERVER_MIN_MAX_AGE`
/// (`ssdp.rs:32`), qu'on ne refait pas, on le réutilise.
///
/// Trois fenêtres, donc, et pas une : `MEDIA_SERVER_STALE_AFTER` (900 s,
/// `ssdp.rs:41`) marque déjà « non joignable », et ce marquage-là est
/// cosmétique. L'absence, elle, RETIRE le serveur des propositions : elle doit
/// coûter plus cher qu'un hoquet de Wi-Fi. Trois annonces manquées d'affilée
/// ne sont plus un hoquet.
///
/// On ne prend pas les 24 h du précédent des zones (`RECENTE_SECS`,
/// `zones/presence.rs:19`) : ce seuil-là qualifie sans agir — une zone
/// « absente_depuis » reste proposée. Ici l'absence a une conséquence, donc
/// elle se mesure sur l'horloge du protocole, pas sur celle de l'usage.
pub const SERVEUR_ABSENT_APRES: Duration = Duration::from_secs(3 * 1_800);

/// Part du registre au-delà de laquelle une bascule en absence n'est plus
/// crédible comme une somme d'extinctions individuelles.
///
/// Même geste que `PART_MAX_PURGE = 0.20` (`routes/system/scan.rs:490`), payé
/// par #1943 et les 21 277 pistes de Yacine, et même raisonnement que
/// `cloud/library_reconcile.rs:152` : « si `SELECT id FROM artists` rend zéro
/// ligne, ce n'est pas *tout a disparu*, c'est une base non montée. » Ici :
/// si la moitié du réseau se tait d'un coup, ce n'est pas *tout le monde a
/// éteint son NAS*, c'est NOTRE lien réseau qui est tombé.
///
/// Le plafond est plus haut que celui de la purge (0,50 contre 0,20) parce que
/// le registre compte des unités, pas des dizaines de milliers de pistes :
/// sur trois serveurs, un seul qu'on éteint fait déjà 33 %.
pub const PART_MAX_ABSENCE_SIMULTANEE: f64 = 0.50;

/// En deçà de ce nombre de serveurs, le plafond ne s'applique pas.
///
/// Sur un registre à un ou deux serveurs, « la moitié » ne veut rien dire :
/// éteindre l'unique NAS de la maison ferait 100 % et resterait indéfiniment
/// proposé. Le plafond protège d'une panne de réseau, et une panne de réseau
/// ne se déduit pas d'un échantillon de deux.
pub const PLANCHER_PLAFOND_ABSENCE: usize = 3;

/// Ce que le plafond accorde quand il refuse la bascule : le délai est
/// DOUBLÉ, pas annulé.
///
/// C'est la différence avec la purge de bibliothèque, et elle est délibérée.
/// `scan.rs` peut refuser pour toujours parce qu'un humain peut confirmer avec
/// `?confirm_purge=N` ; ici personne ne confirmera, et un veto perpétuel
/// rendrait le deuxième livrable faux — un réseau réellement éteint ne
/// passerait JAMAIS absent, et la liste se remettrait à mentir, dans l'autre
/// sens. Le discriminant entre une coupure et une extinction, c'est le temps :
/// une coupure dure des secondes ou des minutes, une extinction dure. On
/// laisse donc courir une seconde fenêtre complète avant d'acter.
pub const FACTEUR_CONFIRMATION_MASSE: u32 = 2;

/// Reprendre, dans le registre partagé, la fraîcheur et l'identité que le
/// balayage tient à jour.
///
/// # Pourquoi ce geste existe
///
/// Il y a DEUX cartes de serveurs multimédia. `ScannerState.media_servers`
/// (`ssdp.rs`) est tenue à jour : une réannonce y remet `last_seen` à zéro EN
/// PLACE. `AppState.media_servers` (`tune-server/src/state.rs`) en est une
/// COPIE, et elle n'a qu'un seul écrivain — l'évènement
/// `SsdpEvent::MediaServerDiscovered`, émis à la seule PREMIÈRE découverte. La
/// copie était donc un instantané gelé : son `Instant` ne bougeait plus jamais,
/// et tout ce qui en dérivait mentait d'un temps qui grandissait tout seul.
///
/// # Ce que la fonction NE fait pas, et pourquoi
///
/// **Elle n'insère jamais.** `get_mut` est le geste, et il est délibéré : le
/// rideau de #3688 écarte NOTRE PROPRE serveur multimédia du registre partagé
/// (`discovery_setup.rs`, `est_notre_propre_serveur_multimedia`) alors que le
/// balayage, lui, le voit et le garde dans sa carte. Une insertion aveugle
/// ferait donc revenir Tune dans sa propre liste de serveurs du réseau —
/// exactement ce que #3688 a retiré.
///
/// **Elle ne retire jamais.** L'oubli est un acte à part, il a son évènement
/// (`MediaServerLost`) et son unique chemin (`retirer_serveur_multimedia`).
/// Un serveur absent d'un cycle de balayage n'est pas un serveur disparu — la
/// doctrine de `ssdp.rs` est de MARQUER, pas de retirer.
///
/// Rend le nombre d'entrées reprises.
pub fn reprendre_la_fraicheur(
    registre: &mut std::collections::HashMap<String, crate::discovery::ssdp::MediaServerInfo>,
    vue_du_balayage: Vec<crate::discovery::ssdp::MediaServerInfo>,
) -> usize {
    let mut reprises = 0usize;
    for frais in vue_du_balayage {
        if let Some(place) = registre.get_mut(&frais.id) {
            // L'identité entière, pas seulement la date : une `LOCATION` qui
            // change réécrit l'hôte, le port et l'URL de contrôle
            // (`ssdp.rs`, « ssdp_location_changee_appareil_reenregistre »), et
            // la copie gardait l'ancienne à vie.
            *place = frais;
            reprises += 1;
        }
    }
    reprises
}

/// Ce que le registre sait d'un serveur, au moment de le qualifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationServeur<'a> {
    /// L'UDN — l'identité stable. Le port change, pas lui
    /// (`discovery/redecouverte.rs:1-45`).
    pub udn: &'a str,
    /// Secondes écoulées depuis la dernière observation. `None` = jamais
    /// observé, ce qui n'arrive que sur une ligne écrite à la main.
    pub age_secs: Option<i64>,
    /// La couche SSDP a CONFIRMÉ la disparition : `ssdp:byebye` vérifié par
    /// une sonde unicast, ou `max-age` écoulé puis sonde échouée
    /// (`ssdp.rs:1644-1660`). Jamais un simple silence.
    pub disparition_confirmee: bool,
}

/// Pourquoi un serveur n'est plus proposé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaisonAbsence {
    /// Plus rien reçu depuis [`SERVEUR_ABSENT_APRES`].
    SilenceProlonge,
    /// La couche SSDP a confirmé la disparition — c'est un CONSTAT, pas une
    /// déduction sur l'horloge.
    DisparitionConfirmee,
    /// Jamais observé : la ligne existe, l'observation n'a jamais eu lieu.
    JamaisObserve,
}

impl RaisonAbsence {
    pub fn code(self) -> &'static str {
        match self {
            Self::SilenceProlonge => "silence_prolonge",
            Self::DisparitionConfirmee => "disparition_confirmee",
            Self::JamaisObserve => "jamais_observe",
        }
    }
}

/// Le verdict pour UN serveur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceServeur {
    /// Vu assez récemment : proposé.
    Present,
    /// Pas revu depuis assez longtemps : visible, cherchable, mais plus
    /// proposé.
    Absent {
        depuis_secs: i64,
        raison: RaisonAbsence,
    },
}

impl PresenceServeur {
    pub fn code(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent { .. } => "absent",
        }
    }

    /// Un serveur absent n'est plus proposé. C'est la SEULE conséquence de
    /// l'absence — il n'est ni masqué, ni supprimé.
    pub fn proposable(self) -> bool {
        matches!(self, Self::Present)
    }

    pub fn raison(self) -> Option<RaisonAbsence> {
        match self {
            Self::Present => None,
            Self::Absent { raison, .. } => Some(raison),
        }
    }
}

/// Le plafond a refusé une bascule en masse. Publié tel quel, avec les
/// nombres — comme le refus de purge de `scan.rs:531-536`, qui publie le
/// compte à remettre dans `?confirm_purge=N`. Un refus muet serait
/// indébogable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasculeEnMasseRefusee {
    /// Combien de serveurs auraient basculé.
    pub candidats: usize,
    /// Sur combien au total.
    pub total: usize,
    /// Le nombre au-delà duquel le plafond se déclenche.
    pub plafond: usize,
    /// Le délai, doublé, au bout duquel la bascule sera actée malgré tout.
    pub confirmation_apres_secs: i64,
}

/// Le verdict pour tout le registre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictRegistre {
    /// Un verdict par serveur, dans l'ordre reçu.
    pub presences: Vec<(String, PresenceServeur)>,
    /// Renseigné quand le plafond a retenu une bascule.
    pub bascule_refusee: Option<BasculeEnMasseRefusee>,
}

impl VerdictRegistre {
    pub fn pour(&self, udn: &str) -> Option<PresenceServeur> {
        self.presences
            .iter()
            .find(|(id, _)| id == udn)
            .map(|(_, p)| *p)
    }
}

/// Le seuil individuel, en secondes.
fn seuil_secs() -> i64 {
    SERVEUR_ABSENT_APRES.as_secs() as i64
}

/// Qualifier tout le registre d'un coup.
///
/// D'un coup, et pas serveur par serveur : le plafond de bascule en masse ne
/// peut se juger que sur l'ensemble. C'est la leçon de `verdict_purge`
/// (`scan.rs:454-481`) — le verdict porte sur la LISTE, jamais sur la ligne.
pub fn qualifier_le_registre(observations: &[ObservationServeur<'_>]) -> VerdictRegistre {
    let seuil = seuil_secs();

    // Premier tour : qui serait absent, au seuil simple.
    let candidats: Vec<bool> = observations
        .iter()
        .map(|o| verdict_simple(o, seuil).is_some())
        .collect();

    let total = observations.len();
    // Une disparition CONFIRMÉE n'entre pas dans le décompte du plafond : le
    // plafond protège d'une déduction sur l'horloge, et un byebye vérifié par
    // sonde unicast n'est pas une déduction, c'est une mesure.
    let candidats_sur_silence = observations
        .iter()
        .zip(&candidats)
        .filter(|(o, c)| **c && !o.disparition_confirmee)
        .count();

    let plafond = (total as f64 * PART_MAX_ABSENCE_SIMULTANEE).floor() as usize;
    let plafond_applicable = total >= PLANCHER_PLAFOND_ABSENCE;
    let seuil_confirmation = seuil * i64::from(FACTEUR_CONFIRMATION_MASSE);

    let masse = plafond_applicable && candidats_sur_silence > plafond;

    // Deuxième tour : sous plafond, seul un silence qui a duré DEUX fenêtres
    // est acté. Le reste reste présent, et le refus est publié.
    let mut retenus = 0usize;
    let presences = observations
        .iter()
        .map(|o| {
            let seuil_effectif = if masse && !o.disparition_confirmee {
                seuil_confirmation
            } else {
                seuil
            };
            let presence = match verdict_simple(o, seuil_effectif) {
                Some(p) => p,
                None => PresenceServeur::Present,
            };
            if masse && !o.disparition_confirmee && presence == PresenceServeur::Present {
                // Il aurait basculé au seuil simple, il ne bascule pas : c'est
                // ce que le plafond a retenu.
                if verdict_simple(o, seuil).is_some() {
                    retenus += 1;
                }
            }
            (o.udn.to_string(), presence)
        })
        .collect();

    VerdictRegistre {
        presences,
        bascule_refusee: (masse && retenus > 0).then_some(BasculeEnMasseRefusee {
            candidats: candidats_sur_silence,
            total,
            plafond,
            confirmation_apres_secs: seuil_confirmation,
        }),
    }
}

/// Le verdict d'UN serveur contre UN seuil, sans le plafond. Rend `None`
/// quand le serveur est présent.
fn verdict_simple(o: &ObservationServeur<'_>, seuil: i64) -> Option<PresenceServeur> {
    if o.disparition_confirmee {
        return Some(PresenceServeur::Absent {
            depuis_secs: o.age_secs.unwrap_or(0).max(0),
            raison: RaisonAbsence::DisparitionConfirmee,
        });
    }
    match o.age_secs {
        None => Some(PresenceServeur::Absent {
            depuis_secs: 0,
            raison: RaisonAbsence::JamaisObserve,
        }),
        // `>=` et pas `>` : au seuil exact, le serveur bascule. Le test le
        // fixe, pour qu'un jour personne ne « corrige » l'inégalité.
        Some(age) if age >= seuil => Some(PresenceServeur::Absent {
            depuis_secs: age,
            raison: RaisonAbsence::SilenceProlonge,
        }),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(udn: &str, age: i64) -> ObservationServeur<'_> {
        ObservationServeur {
            udn,
            age_secs: Some(age),
            disparition_confirmee: false,
        }
    }

    /// Témoin 2 — un serveur non revu passe absent AU-DELÀ du délai, et pas
    /// avant. Les deux moitiés comptent : un seuil qui bascule trop tôt fait
    /// disparaître un serveur vivant, ce que #2139 interdit.
    #[test]
    fn un_serveur_non_revu_bascule_au_delai_et_pas_avant() {
        let seuil = SERVEUR_ABSENT_APRES.as_secs() as i64;
        assert_eq!(seuil, 5_400, "trois fenêtres de 1 800 s mesurées le 13/09");

        // Une seconde avant : encore présent.
        let v = qualifier_le_registre(&[obs("a", seuil - 1)]);
        assert_eq!(v.pour("a"), Some(PresenceServeur::Present));
        assert!(v.pour("a").unwrap().proposable());

        // Au seuil exact : absent.
        let v = qualifier_le_registre(&[obs("a", seuil)]);
        assert_eq!(
            v.pour("a"),
            Some(PresenceServeur::Absent {
                depuis_secs: seuil,
                raison: RaisonAbsence::SilenceProlonge
            })
        );
        assert!(!v.pour("a").unwrap().proposable());
        assert_eq!(
            v.pour("a").unwrap().raison().unwrap().code(),
            "silence_prolonge"
        );

        // Les 84 194 s mesurées sur le `.18` le 13/09 : absent, évidemment.
        let v = qualifier_le_registre(&[obs("a", 84_194)]);
        assert!(!v.pour("a").unwrap().proposable());
    }

    /// Le marquage cosmétique de #2139 (900 s) et l'absence (5 400 s) ne sont
    /// pas le même seuil, et l'absence est le plus tolérant des deux.
    #[test]
    fn l_absence_est_plus_tolerante_que_le_marquage_non_joignable() {
        assert!(
            SERVEUR_ABSENT_APRES.as_secs() > 900,
            "un serveur non joignable n'est pas encore absent"
        );
        // Un serveur silencieux depuis 20 min : déjà « non joignable » pour
        // `ssdp.rs`, mais toujours proposé.
        let v = qualifier_le_registre(&[obs("a", 1_200)]);
        assert!(v.pour("a").unwrap().proposable());
    }

    /// Témoin 4 — une disparition de masse est plafonnée : les serveurs
    /// restent présents et le refus est publié avec ses nombres.
    #[test]
    fn une_disparition_de_masse_est_plafonnee_et_le_refus_est_publie() {
        let seuil = SERVEUR_ABSENT_APRES.as_secs() as i64;
        // Quatre serveurs, trois se taisent d'un coup : 3 > floor(4 * 0,5) = 2.
        let v = qualifier_le_registre(&[
            obs("a", seuil + 10),
            obs("b", seuil + 10),
            obs("c", seuil + 10),
            obs("d", 30),
        ]);
        for udn in ["a", "b", "c"] {
            assert_eq!(
                v.pour(udn),
                Some(PresenceServeur::Present),
                "{udn} retenu par le plafond : c'est le réseau qu'on soupçonne, pas le serveur"
            );
        }
        let refus = v.bascule_refusee.expect("le refus doit être publié");
        assert_eq!(refus.candidats, 3);
        assert_eq!(refus.total, 4);
        assert_eq!(refus.plafond, 2);
        assert_eq!(refus.confirmation_apres_secs, 2 * seuil);
    }

    /// Le plafond n'est pas un veto : la seconde fenêtre passée, la bascule
    /// est actée. Sans quoi un réseau réellement éteint ne passerait jamais
    /// absent, et la liste mentirait dans l'autre sens.
    #[test]
    fn le_plafond_retarde_la_bascule_il_ne_l_annule_pas() {
        let seuil = SERVEUR_ABSENT_APRES.as_secs() as i64;
        let v = qualifier_le_registre(&[
            obs("a", 2 * seuil),
            obs("b", 2 * seuil),
            obs("c", 2 * seuil),
            obs("d", 30),
        ]);
        for udn in ["a", "b", "c"] {
            assert!(
                !v.pour(udn).unwrap().proposable(),
                "{udn} acté après confirmation"
            );
        }
        assert_eq!(
            v.bascule_refusee, None,
            "plus rien n'est retenu : le refus n'a plus lieu d'être publié"
        );
        assert_eq!(v.pour("d"), Some(PresenceServeur::Present));
    }

    /// Une extinction ORDINAIRE — un serveur sur quatre — n'est pas plafonnée.
    /// Le plafond doit protéger d'une panne, pas gêner l'usage normal.
    #[test]
    fn une_extinction_ordinaire_n_est_pas_plafonnee() {
        let seuil = SERVEUR_ABSENT_APRES.as_secs() as i64;
        let v = qualifier_le_registre(&[
            obs("a", seuil + 10),
            obs("b", 30),
            obs("c", 30),
            obs("d", 30),
        ]);
        assert!(!v.pour("a").unwrap().proposable());
        assert_eq!(v.bascule_refusee, None);
    }

    /// Sous le plancher, le plafond ne s'applique pas : sur un registre à un
    /// seul serveur, « la moitié » ne veut rien dire et le NAS éteint doit
    /// pouvoir passer absent.
    #[test]
    fn le_plafond_ne_s_applique_pas_sous_le_plancher() {
        let seuil = SERVEUR_ABSENT_APRES.as_secs() as i64;
        for n in 1..PLANCHER_PLAFOND_ABSENCE {
            let obs: Vec<_> = (0..n).map(|i| obs_owned(i, seuil + 10)).collect();
            let refs: Vec<ObservationServeur<'_>> = obs
                .iter()
                .map(|(u, a)| ObservationServeur {
                    udn: u,
                    age_secs: Some(*a),
                    disparition_confirmee: false,
                })
                .collect();
            let v = qualifier_le_registre(&refs);
            assert_eq!(v.bascule_refusee, None, "registre de {n}");
            assert!(v.presences.iter().all(|(_, p)| !p.proposable()));
        }
    }

    fn obs_owned(i: usize, age: i64) -> (String, i64) {
        (format!("srv{i}"), age)
    }

    /// Une disparition CONFIRMÉE échappe au plafond : le plafond se défie
    /// d'une déduction sur l'horloge, or un `byebye` vérifié par sonde
    /// unicast est une mesure, pas une déduction.
    #[test]
    fn une_disparition_confirmee_echappe_au_plafond() {
        let confirme = |udn| ObservationServeur {
            udn,
            age_secs: Some(10),
            disparition_confirmee: true,
        };
        let v = qualifier_le_registre(&[confirme("a"), confirme("b"), confirme("c"), obs("d", 30)]);
        for udn in ["a", "b", "c"] {
            assert_eq!(
                v.pour(udn).unwrap().raison(),
                Some(RaisonAbsence::DisparitionConfirmee)
            );
            assert!(!v.pour(udn).unwrap().proposable());
        }
        assert_eq!(v.bascule_refusee, None);
        assert_eq!(v.pour("d"), Some(PresenceServeur::Present));
    }

    /// Une ligne jamais observée le dit, plutôt que de se faire passer pour
    /// fraîche — c'est exactement le travers de `zones.last_seen_at`
    /// (`migrations.rs:1659-1673`) : « poser la date de la mise a jour sur une
    /// zone morte depuis trois semaines la ferait passer pour recente ».
    #[test]
    fn jamais_observe_se_dit_et_ne_se_fait_pas_passer_pour_frais() {
        let v = qualifier_le_registre(&[ObservationServeur {
            udn: "a",
            age_secs: None,
            disparition_confirmee: false,
        }]);
        assert_eq!(
            v.pour("a").unwrap().raison(),
            Some(RaisonAbsence::JamaisObserve)
        );
        assert!(!v.pour("a").unwrap().proposable());
    }

    #[test]
    fn un_registre_vide_ne_publie_aucun_refus() {
        let v = qualifier_le_registre(&[]);
        assert!(v.presences.is_empty());
        assert_eq!(v.bascule_refusee, None);
    }

    // -----------------------------------------------------------------------
    // La reprise de fraîcheur — le défaut mesuré sur le `.18` le 14/09/2026.
    // -----------------------------------------------------------------------

    fn info(
        udn: &str,
        nom: &str,
        host: &str,
        port: u16,
    ) -> crate::discovery::ssdp::MediaServerInfo {
        crate::discovery::ssdp::MediaServerInfo {
            id: udn.into(),
            name: nom.into(),
            manufacturer: "MozAIk Labs".into(),
            model: "Tune".into(),
            location: format!("http://{host}:{port}/upnp/description.xml"),
            content_directory_url: format!("http://{host}:{port}/upnp/cd/control"),
            host: host.into(),
            port,
            last_seen: std::time::Instant::now(),
            max_age: Duration::from_secs(1800),
        }
    }

    /// Le témoin de l'anomalie : la copie partagée reprend la DATE que le
    /// balayage tient, au lieu de rester gelée à la première découverte.
    ///
    /// La preuve ne se joue pas sur un `Instant` reculé — `Instant::checked_sub`
    /// rend `None` sur une machine fraîchement démarrée, piège documenté plus
    /// haut. Elle se joue sur l'IDENTITÉ de l'`Instant` : après reprise, la
    /// date du registre est CELLE du balayage, et non plus celle qu'il avait.
    #[test]
    fn la_copie_partagee_reprend_la_date_du_balayage() {
        let mut registre = std::collections::HashMap::new();
        let ancienne = info("uuid:2c35bec3", "Tune Server", "192.168.1.42", 8888);
        let date_gelee = ancienne.last_seen;
        registre.insert("uuid:2c35bec3".to_string(), ancienne);

        // Le balayage a revu le serveur depuis.
        std::thread::sleep(Duration::from_millis(20));
        let frais = info("uuid:2c35bec3", "Tune Server", "192.168.1.42", 8888);
        let date_fraiche = frais.last_seen;
        assert!(date_fraiche > date_gelee, "le témoin doit bien avancer");

        assert_eq!(reprendre_la_fraicheur(&mut registre, vec![frais]), 1);
        assert_eq!(
            registre["uuid:2c35bec3"].last_seen, date_fraiche,
            "la copie doit porter la date du balayage, pas la sienne"
        );
    }

    /// Le rideau de #3688 : la reprise n'INSÈRE jamais. Notre propre serveur
    /// multimédia est dans la carte du balayage et volontairement absent du
    /// registre partagé ; une insertion aveugle le ferait revenir dans la liste
    /// « Serveurs multimédia » que le testeur voit.
    #[test]
    fn la_reprise_n_insere_jamais_et_le_rideau_3688_tient() {
        let mut registre = std::collections::HashMap::new();
        registre.insert(
            "uuid:connu".to_string(),
            info("uuid:connu", "Tune Server", "192.168.1.42", 8888),
        );

        let reprises = reprendre_la_fraicheur(
            &mut registre,
            vec![
                info("uuid:connu", "Tune Server", "192.168.1.42", 8888),
                // Nous-mêmes, que le rideau a écartés du registre partagé.
                info("uuid:nous-memes", "Tune Server", "192.168.1.18", 8888),
            ],
        );

        assert_eq!(reprises, 1, "une seule entrée était connue");
        assert_eq!(registre.len(), 1, "rien n'a été inséré");
        assert!(
            !registre.contains_key("uuid:nous-memes"),
            "notre propre serveur ne doit pas revenir dans sa propre liste (#3688)"
        );
    }

    /// L'identité ENTIÈRE est reprise, pas seulement la date : une `LOCATION`
    /// qui change réécrit l'hôte, le port et l'URL de contrôle. La copie gelée
    /// gardait l'ancienne adresse à vie.
    #[test]
    fn la_reprise_reecrit_l_identite_entiere_pas_seulement_la_date() {
        let mut registre = std::collections::HashMap::new();
        registre.insert(
            "uuid:asset".to_string(),
            info(
                "uuid:asset",
                "Asset UPnP: ancien-nom",
                "192.168.1.41",
                26125,
            ),
        );

        reprendre_la_fraicheur(
            &mut registre,
            vec![info(
                "uuid:asset",
                "Asset UPnP: Mac-Studio-6",
                "192.168.1.41",
                26126,
            )],
        );

        let repris = &registre["uuid:asset"];
        assert_eq!(repris.name, "Asset UPnP: Mac-Studio-6");
        assert_eq!(repris.port, 26126);
        assert_eq!(
            repris.location, "http://192.168.1.41:26126/upnp/description.xml",
            "l'adresse de description suit le déménagement"
        );
    }

    /// La reprise ne RETIRE jamais : l'oubli a son évènement
    /// (`MediaServerLost`) et son unique chemin. Un serveur absent d'un cycle
    /// de balayage n'est pas un serveur disparu.
    #[test]
    fn la_reprise_ne_retire_jamais_ce_que_le_balayage_n_a_pas_vu() {
        let mut registre = std::collections::HashMap::new();
        registre.insert(
            "uuid:silencieux".to_string(),
            info("uuid:silencieux", "NAS", "192.168.1.50", 8200),
        );
        assert_eq!(reprendre_la_fraicheur(&mut registre, Vec::new()), 0);
        assert!(
            registre.contains_key("uuid:silencieux"),
            "marquer, pas retirer — la doctrine de ssdp.rs"
        );
    }
}
