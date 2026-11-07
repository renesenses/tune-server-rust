//! Reprise de la découverte mDNS quand le réseau arrive APRÈS Tune (#8100).
//!
//! Le cas du relevé (rc10, Linux, DHCP lent) : Tune démarre alors que la
//! machine n'a encore aucune adresse IPv4. Les recherches mDNS partent sur la
//! seule boucle locale, et le service Tune est annoncé sur `127.0.0.1`. Le
//! réseau arrive 50 s plus tard ; SSDP se rétablit, mDNS non.
//!
//! Ce que fait mdns-sd 0.20.2 de lui-même, et pourquoi cela ne suffit pas :
//! - son démon relit les interfaces toutes les 5 s (`check_ip_changes`) et,
//!   sur une interface nouvelle, envoie UNE requête PTR par type suivi
//!   (`add_interface`), sans aucune répétition. Les requêtes périodiques
//!   d'une recherche, elles, suivent un recul doublé depuis le démarrage
//!   (1, 2, 4… jusqu'à 3 600 s) : à l'arrivée du réseau, la prochaine est
//!   déjà loin, et chaque suivante deux fois plus loin. Un récepteur déjà
//!   allumé, qui ne s'annonce plus de lui-même, ne répond donc qu'à ce seul
//!   datagramme, émis à l'instant même où l'adresse apparaît ;
//! - il ne met à jour l'adresse d'un service enregistré QUE si ce service est
//!   en adresse automatique (`is_addr_auto`). `register_self` lui donne une
//!   adresse fixe — `127.0.0.1` faute de mieux — que le démon garde pour
//!   toujours et ne publie sur aucune interface réelle (il filtre les
//!   adresses par sous-réseau de l'interface).
//!
//! La reprise vit ici en deux morceaux : une DÉCISION pure
//! ([`decider_reprise`]), témoignée sans réseau, et un [`SurveillantReseau`]
//! qui la rejoue à chaque relevé d'adresses, sur des actions injectables.
//! Elle réutilise la détection d'adresse de SSDP
//! (`ssdp::local_ipv4_addresses`, `ssdp::get_local_ip`) au lieu d'en écrire
//! une deuxième.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::time::Duration;

/// Intervalle de relevé des adresses : celui du démon mdns-sd (5 s), pour
/// qu'un récepteur apparaisse dans les secondes qui suivent le réseau.
pub const INTERVALLE_RELEVE: Duration = Duration::from_secs(5);

/// Ce qu'un changement d'adresses commande.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reprise {
    /// Relancer les recherches de tous les types suivis (`stop_browse` puis
    /// `browse`) : le recul des requêtes repart de 1 s, sur toutes les
    /// interfaces présentes.
    pub relancer_recherches: bool,
    /// Réenregistrer les services de Tune sur cette adresse.
    pub reenregistrer: Option<Ipv4Addr>,
}

impl Reprise {
    pub fn rien(&self) -> bool {
        !self.relancer_recherches && self.reenregistrer.is_none()
    }
}

/// Décide de la reprise à partir de deux relevés d'adresses IPv4 locales
/// (hors boucle locale).
///
/// - aucune adresse : rien — il n'y a nulle part où chercher ;
/// - ensemble inchangé : rien — c'est ce qui empêche toute tempête ;
/// - ensemble changé et non vide : relance des recherches, et
///   réenregistrement si l'adresse que Tune choisirait (`choisie`) n'est pas
///   celle qu'il annonce (`annoncee`, `None` = `127.0.0.1`).
pub fn decider_reprise(
    precedentes: &BTreeSet<Ipv4Addr>,
    courantes: &BTreeSet<Ipv4Addr>,
    annoncee: Option<Ipv4Addr>,
    choisie: Option<Ipv4Addr>,
) -> Reprise {
    if courantes.is_empty() || courantes == precedentes {
        return Reprise::default();
    }
    Reprise {
        relancer_recherches: true,
        reenregistrer: choisie.filter(|ip| Some(*ip) != annoncee),
    }
}

/// Les gestes que la reprise commande, séparés du démon pour qu'un banc les
/// compte sans ouvrir de socket.
pub trait ActionsReprise {
    fn relancer_recherches(&self);
    fn reenregistrer(&self, ip: Ipv4Addr);
}

/// Rejoue [`decider_reprise`] relevé après relevé.
///
/// `releve` rend les adresses IPv4 locales du moment, `choix` l'adresse que
/// Tune annoncerait. `choix` n'est appelé que sur changement : c'est la
/// détection de SSDP, qui sonde et journalise.
pub struct SurveillantReseau<R, C> {
    releve: R,
    choix: C,
    precedentes: BTreeSet<Ipv4Addr>,
    annoncee: Option<Ipv4Addr>,
}

impl<R, C> SurveillantReseau<R, C>
where
    R: FnMut() -> BTreeSet<Ipv4Addr>,
    C: FnMut() -> Option<Ipv4Addr>,
{
    /// `precedentes` : le relevé pris au démarrage des recherches ;
    /// `annoncee` : l'adresse que porte l'annonce de Tune (`None` =
    /// `127.0.0.1`).
    pub fn new(
        releve: R,
        choix: C,
        precedentes: BTreeSet<Ipv4Addr>,
        annoncee: Option<Ipv4Addr>,
    ) -> Self {
        Self {
            releve,
            choix,
            precedentes,
            annoncee,
        }
    }

    /// Un relevé : décide, agit, retient. Rend la décision prise.
    pub fn pas(&mut self, actions: &impl ActionsReprise) -> Reprise {
        let courantes = (self.releve)();
        let choisie = if courantes.is_empty() || courantes == self.precedentes {
            None
        } else {
            (self.choix)()
        };
        let reprise = decider_reprise(&self.precedentes, &courantes, self.annoncee, choisie);
        if reprise.relancer_recherches {
            actions.relancer_recherches();
        }
        if let Some(ip) = reprise.reenregistrer {
            actions.reenregistrer(ip);
            self.annoncee = Some(ip);
        }
        self.precedentes = courantes;
        reprise
    }
}

/// Relevé réel : les adresses IPv4 hors boucle locale, celles de SSDP.
pub fn releve_systeme() -> BTreeSet<Ipv4Addr> {
    crate::discovery::ssdp::local_ipv4_addresses()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn ens(ips: &[[u8; 4]]) -> BTreeSet<Ipv4Addr> {
        ips.iter().map(|o| Ipv4Addr::from(*o)).collect()
    }

    const LAN: [u8; 4] = [192, 168, 1, 20];
    const LAN2: [u8; 4] = [192, 168, 1, 21];
    const VPN: [u8; 4] = [10, 8, 0, 2];

    #[test]
    fn sans_adresse_puis_adresse_relance_et_reenregistre() {
        let r = decider_reprise(&ens(&[]), &ens(&[LAN]), None, Some(LAN.into()));
        assert!(r.relancer_recherches);
        assert_eq!(r.reenregistrer, Some(LAN.into()));
    }

    #[test]
    fn adresse_inchangee_ne_fait_rien() {
        let r = decider_reprise(
            &ens(&[LAN]),
            &ens(&[LAN]),
            Some(LAN.into()),
            Some(LAN.into()),
        );
        assert!(r.rien());
        // Même si l'annonce est restée sur 127.0.0.1 : sans changement
        // d'adresses, aucun geste — la décision ne boucle jamais.
        let r = decider_reprise(&ens(&[LAN]), &ens(&[LAN]), None, Some(LAN.into()));
        assert!(r.rien());
    }

    #[test]
    fn toujours_sans_adresse_ne_fait_rien() {
        assert!(decider_reprise(&ens(&[]), &ens(&[]), None, None).rien());
    }

    #[test]
    fn perte_du_reseau_ne_fait_rien() {
        // Plus d'adresse : nulle part où chercher ni annoncer.
        assert!(decider_reprise(&ens(&[LAN]), &ens(&[]), Some(LAN.into()), None).rien());
    }

    #[test]
    fn nouvelle_adresse_annexe_relance_sans_reenregistrer() {
        // Un VPN monte : on relance les recherches (nouvelle interface),
        // mais l'adresse choisie reste celle du LAN, déjà annoncée.
        let r = decider_reprise(
            &ens(&[LAN]),
            &ens(&[LAN, VPN]),
            Some(LAN.into()),
            Some(LAN.into()),
        );
        assert!(r.relancer_recherches);
        assert_eq!(r.reenregistrer, None);
    }

    #[test]
    fn changement_d_adresse_reenregistre_sur_la_nouvelle() {
        let r = decider_reprise(
            &ens(&[LAN]),
            &ens(&[LAN2]),
            Some(LAN.into()),
            Some(LAN2.into()),
        );
        assert!(r.relancer_recherches);
        assert_eq!(r.reenregistrer, Some(LAN2.into()));
    }

    #[derive(Default)]
    struct Compteur {
        relances: RefCell<u32>,
        enregistrements: RefCell<Vec<Ipv4Addr>>,
    }

    impl ActionsReprise for Compteur {
        fn relancer_recherches(&self) {
            *self.relances.borrow_mut() += 1;
        }
        fn reenregistrer(&self, ip: Ipv4Addr) {
            self.enregistrements.borrow_mut().push(ip);
        }
    }

    /// Le scénario du relevé, avec un faux fournisseur d'adresses : démarrage
    /// sans réseau, DHCP au 4e relevé, puis un réseau stable. Une seule
    /// relance, un seul réenregistrement, et rien ensuite.
    #[test]
    fn surveillant_dhcp_lent_une_seule_reprise() {
        let releves = RefCell::new(vec![
            ens(&[]),
            ens(&[]),
            ens(&[]),
            ens(&[LAN]),
            ens(&[LAN]),
            ens(&[LAN]),
            ens(&[LAN]),
        ]);
        let choix_appeles = RefCell::new(0u32);
        let mut s = SurveillantReseau::new(
            || releves.borrow_mut().remove(0),
            || {
                *choix_appeles.borrow_mut() += 1;
                Some(LAN.into())
            },
            ens(&[]),
            None,
        );
        let actions = Compteur::default();
        let decisions: Vec<Reprise> = (0..7).map(|_| s.pas(&actions)).collect();

        assert_eq!(*actions.relances.borrow(), 1);
        assert_eq!(*actions.enregistrements.borrow(), vec![Ipv4Addr::from(LAN)]);
        assert!(decisions[3].relancer_recherches);
        assert!(
            decisions
                .iter()
                .enumerate()
                .all(|(i, d)| i == 3 || d.rien())
        );
        // La détection (qui sonde et journalise) n'est sollicitée que sur
        // changement, pas à chaque relevé.
        assert_eq!(*choix_appeles.borrow(), 1);
    }

    /// Réseau qui tombe puis revient sur la même adresse : relance au retour
    /// (les recherches repartent), sans réenregistrer — l'annonce porte déjà
    /// la bonne adresse.
    #[test]
    fn surveillant_reseau_qui_revient() {
        let releves = RefCell::new(vec![ens(&[]), ens(&[LAN]), ens(&[LAN])]);
        let mut s = SurveillantReseau::new(
            || releves.borrow_mut().remove(0),
            || Some(LAN.into()),
            ens(&[LAN]),
            Some(LAN.into()),
        );
        let actions = Compteur::default();
        for _ in 0..3 {
            s.pas(&actions);
        }
        assert_eq!(*actions.relances.borrow(), 1);
        assert!(actions.enregistrements.borrow().is_empty());
    }

    /// Réseau déjà là au démarrage : aucun geste, jamais.
    #[test]
    fn surveillant_reseau_present_des_le_depart() {
        let mut s = SurveillantReseau::new(
            || ens(&[LAN]),
            || panic!("la détection ne doit pas être sollicitée sans changement"),
            ens(&[LAN]),
            Some(LAN.into()),
        );
        let actions = Compteur::default();
        for _ in 0..10 {
            assert!(s.pas(&actions).rien());
        }
        assert_eq!(*actions.relances.borrow(), 0);
    }
}
