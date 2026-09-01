//! Reconnaître un renderer dont l'UDN a changé — sans jamais confondre deux
//! appareils (#2639).
//!
//! ## Le problème
//!
//! Un UDN UPnP n'est stable que par convention. Un Marantz ND8006 (enveloppe
//! HEOS/AiOS) a régénéré le sien entre le 22 et le 28/08/2026 **à `LOCATION`
//! inchangée** : le descripteur rendait `uuid:c0bfdbad-…` là où le magasin —
//! et le SSDP du même appareil, à la même seconde — portaient encore
//! `uuid:56fcb4ae-…`. Le re-sondage classait ce désaccord « rejet définitif »
//! et **effaçait** l'entrée persistée, donc la zone configurée dessus. Un
//! redémarrage, une mise à jour de micrologiciel ou une réinitialisation
//! suffisent à produire la même bascule.
//!
//! ## Pourquoi la garde ne peut pas simplement disparaître
//!
//! Elle protège d'un vrai risque : le bail DHCP de l'appareil expire, un
//! AUTRE appareil hérite de son adresse, et la zone du salon se met à piloter
//! l'ampli de la chambre. Retirer la comparaison d'UDN sans rien mettre à la
//! place remplacerait un défaut par un pire.
//!
//! ## Le critère retenu
//!
//! Une échelle de clés dont la stabilité **ne dépend pas de l'UDN**, de la
//! plus probante à la moins probante, qui **refuse de trancher** dès qu'elle
//! ne sait pas — plutôt que de deviner dans un sens ou dans l'autre :
//!
//! 1. **L'adresse MAC**, quand les DEUX côtés en ont une. C'est la seule clé
//!    attachée au matériel lui-même : une carte réseau ne change pas parce
//!    qu'un micrologiciel a régénéré un UDN, et deux appareils distincts n'en
//!    partagent jamais une. Elle tranche donc dans les deux sens.
//! 2. **Marque + modèle + nom**, quand les trois sont connus des deux côtés.
//!    Une réutilisation d'adresse par un autre appareil en change au moins un.
//! 3. **Le nom d'usage seul**, quand c'est tout ce qui reste — le cas des
//!    magasins écrits avant ce correctif, qui ne retenaient que lui.
//! 4. Sinon : **indécidable**, et l'appelant ne doit alors RIEN détruire.
//!
//! ## Ce qui rend le niveau 3 acceptable — et pourquoi il ne confond pas
//!
//! [`compare_at_same_location`] ne compare que deux observations faites **à
//! la même `LOCATION`, au sens littéral** : l'appelant a re-téléchargé le
//! descripteur depuis l'URL persistée mot pour mot. Adresse IP, port ET
//! chemin de description sont donc identiques par construction — et le chemin
//! est propre au constructeur (`/upnp/desc/aios_device/aios_device.xml` pour
//! une enveloppe HEOS, `/description.xml` ailleurs). Le niveau 3 se lit donc
//! « même IP, même port, même chemin de description, même nom d'usage ».
//!
//! Pour l'induire en erreur il faudrait qu'un appareil **d'un autre exemplaire
//! portant exactement le même nom** hérite de l'adresse du premier ET serve
//! sa description sur le même port et le même chemin. Et ce niveau ne sert
//! qu'une fois : dès le premier re-sondage réussi, l'appelant mémorise la MAC,
//! la marque et le modèle relevés, si bien que la décision suivante se prend
//! au niveau 1 ou 2.
//!
//! ## Ce que ce module ne fait PAS
//!
//! Il ne déduit rien d'une **absence**. `mac::arp_lookup` rend `None` sur un
//! cache ARP froid, et `DiscoveredDevice::mac_address` documente déjà qu'une
//! MAC manquante ou substituée n'est pas une preuve. Une clé inconnue d'un
//! côté fait descendre d'un niveau ; elle n'accuse jamais.

/// Ce qu'on sait d'un renderer — mémorisé d'un côté, observé de l'autre.
///
/// Toutes les clés sont des chaînes, la vide valant « inconnu » : les magasins
/// persistés d'avant #2639 ne portaient ni MAC, ni marque, ni modèle, et un
/// descripteur UPnP a le droit d'omettre `<manufacturer>` comme
/// `<modelName>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RendererIdentity<'a> {
    /// `uuid:…` — l'UDN de la description **racine**.
    pub udn: &'a str,
    /// `AA:BB:CC:DD:EE:FF`, tel que `mac::normalize_mac` l'écrit.
    pub mac: &'a str,
    /// `<friendlyName>` : le nom que l'utilisateur voit.
    pub friendly_name: &'a str,
    /// `<manufacturer>`.
    pub manufacturer: &'a str,
    /// `<modelName>`.
    pub model_name: &'a str,
}

/// La clé qui a permis de trancher — pour le dire à l'utilisateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// Niveau 1 : l'adresse MAC relevée dans le cache ARP.
    Mac,
    /// Niveau 2 : marque, modèle et nom d'usage.
    MakeAndModel,
    /// Niveau 3 : le nom d'usage, à la même adresse de description.
    FriendlyName,
}

impl Evidence {
    /// Formulation destinée au journal, donc à l'utilisateur.
    pub fn label(self) -> &'static str {
        match self {
            Self::Mac => "son adresse MAC",
            Self::MakeAndModel => "sa marque, son modèle et son nom",
            Self::FriendlyName => "son nom, à la même adresse de description",
        }
    }
}

/// Le verdict rendu sur un désaccord d'UDN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityVerdict {
    /// Rien à arbitrer : même UDN, ou l'un des deux est inconnu.
    ///
    /// L'UDN inconnu tombe ici **délibérément**. Une garde ne peut se
    /// déclencher que sur un désaccord constaté ; si l'un des deux côtés ne
    /// dit pas qui il est, il n'y a pas de désaccord, il y a une ignorance.
    NoDisagreement,
    /// L'UDN a changé, mais une clé stable prouve le MÊME matériel.
    SameHardware(Evidence),
    /// L'UDN a changé, et une clé stable prouve un AUTRE matériel.
    OtherHardware(Evidence),
    /// L'UDN a changé et aucune clé ne permet de trancher.
    Undecidable,
}

fn known(s: &str) -> bool {
    !s.trim().is_empty()
}

/// Comparaison tolérante à la casse et aux espaces de bord.
///
/// Un même UDN peut s'écrire `uuid:56FCB4AE-…` en SSDP et `uuid:56fcb4ae-…`
/// dans le descripteur ; une MAC sort de l'ARP en minuscules sur certains
/// systèmes. Ignorer la casse ne peut que **réduire** les faux désaccords :
/// deux valeurs réellement différentes le restent.
fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn decide(matches: bool, evidence: Evidence) -> IdentityVerdict {
    if matches {
        IdentityVerdict::SameHardware(evidence)
    } else {
        IdentityVerdict::OtherHardware(evidence)
    }
}

/// Trancher entre « même appareil, nouvel UDN » et « autre appareil, même
/// adresse ».
///
/// **Précondition portée par le nom** : `observed` doit avoir été relevé à la
/// `LOCATION` **exacte** dont `persisted` provient — c'est-à-dire que
/// l'appelant a re-téléchargé le descripteur depuis l'URL persistée telle
/// quelle. Sans cette précondition, le niveau 3 (nom seul) ne tiendrait pas :
/// c'est l'identité littérale de l'URL qui lui apporte l'IP, le port et le
/// chemin propre au constructeur.
pub fn compare_at_same_location(
    persisted: RendererIdentity<'_>,
    observed: RendererIdentity<'_>,
) -> IdentityVerdict {
    if !known(persisted.udn) || !known(observed.udn) || same(persisted.udn, observed.udn) {
        return IdentityVerdict::NoDisagreement;
    }

    // Niveau 1 — la MAC. Attachée au matériel, pas au micrologiciel : elle
    // survit à une régénération d'UDN et n'est jamais partagée par deux
    // appareils. On ne s'en sert que lorsque les DEUX côtés en ont une : un
    // cache ARP froid rend `None`, et une absence ne prouve rien.
    if known(persisted.mac) && known(observed.mac) {
        return decide(same(persisted.mac, observed.mac), Evidence::Mac);
    }

    // Niveau 2 — marque, modèle et nom. Les trois doivent être connus des deux
    // côtés, sinon on descend : comparer un modèle contre du vide accuserait
    // un appareil de ne pas être lui-même parce que le magasin est ancien.
    let make_and_model_known = known(persisted.manufacturer)
        && known(observed.manufacturer)
        && known(persisted.model_name)
        && known(observed.model_name)
        && known(persisted.friendly_name)
        && known(observed.friendly_name);
    if make_and_model_known {
        let all_match = same(persisted.manufacturer, observed.manufacturer)
            && same(persisted.model_name, observed.model_name)
            && same(persisted.friendly_name, observed.friendly_name);
        return decide(all_match, Evidence::MakeAndModel);
    }

    // Niveau 3 — le nom seul, à la même URL de description. Voir l'en-tête du
    // module : c'est l'identité littérale de la `LOCATION` qui porte le reste.
    if known(persisted.friendly_name) && known(observed.friendly_name) {
        return decide(
            same(persisted.friendly_name, observed.friendly_name),
            Evidence::FriendlyName,
        );
    }

    IdentityVerdict::Undecidable
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cas de Jean Valjean : magasin d'avant #2639 (ni MAC, ni marque, ni
    /// modèle), UDN régénéré, tout le reste inchangé à la même `LOCATION`.
    const MARANTZ_AVANT: RendererIdentity<'static> = RendererIdentity {
        udn: "uuid:56fcb4ae-e909-1c8d-0080-0006787c2e26",
        mac: "",
        friendly_name: "Marantz ND8006",
        manufacturer: "",
        model_name: "",
    };

    fn marantz_apres() -> RendererIdentity<'static> {
        RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "",
            friendly_name: "Marantz ND8006",
            manufacturer: "Marantz",
            model_name: "ND8006",
        }
    }

    #[test]
    fn un_udn_inchange_n_ouvre_aucun_arbitrage() {
        assert_eq!(
            compare_at_same_location(MARANTZ_AVANT, MARANTZ_AVANT),
            IdentityVerdict::NoDisagreement
        );
    }

    #[test]
    fn la_casse_de_l_udn_ne_fait_pas_un_desaccord() {
        let majuscules = RendererIdentity {
            udn: "UUID:56FCB4AE-E909-1C8D-0080-0006787C2E26",
            ..MARANTZ_AVANT
        };
        assert_eq!(
            compare_at_same_location(MARANTZ_AVANT, majuscules),
            IdentityVerdict::NoDisagreement,
            "le meme UDN ecrit en majuscules doit rester le meme UDN"
        );
    }

    #[test]
    fn un_udn_absent_d_un_cote_n_accuse_personne() {
        let sans_udn = RendererIdentity {
            udn: "",
            ..marantz_apres()
        };
        assert_eq!(
            compare_at_same_location(MARANTZ_AVANT, sans_udn),
            IdentityVerdict::NoDisagreement
        );
        assert_eq!(
            compare_at_same_location(
                RendererIdentity {
                    udn: "",
                    ..MARANTZ_AVANT
                },
                marantz_apres()
            ),
            IdentityVerdict::NoDisagreement
        );
    }

    // ── Le cas du ticket : la zone doit survivre ──────────────────────────

    #[test]
    fn un_udn_regenere_sur_un_magasin_ancien_reconnait_par_le_nom() {
        assert_eq!(
            compare_at_same_location(MARANTZ_AVANT, marantz_apres()),
            IdentityVerdict::SameHardware(Evidence::FriendlyName),
            "meme nom a la meme LOCATION : c'est le Marantz, sa zone doit survivre"
        );
    }

    #[test]
    fn une_mac_identique_prime_sur_tout_le_reste() {
        let avant = RendererIdentity {
            mac: "00:06:78:7C:2E:26",
            manufacturer: "Marantz",
            model_name: "ND8006",
            ..MARANTZ_AVANT
        };
        // Micrologiciel neuf : UDN regenere ET nom repasse au defaut d'usine.
        let apres = RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "00:06:78:7c:2e:26",
            friendly_name: "ND8006",
            manufacturer: "Marantz",
            model_name: "ND8006",
        };
        assert_eq!(
            compare_at_same_location(avant, apres),
            IdentityVerdict::SameHardware(Evidence::Mac),
            "la MAC est la seule cle attachee au materiel : elle doit primer"
        );
    }

    #[test]
    fn marque_modele_et_nom_identiques_reconnaissent_l_appareil() {
        let avant = RendererIdentity {
            manufacturer: "Marantz",
            model_name: "ND8006",
            ..MARANTZ_AVANT
        };
        assert_eq!(
            compare_at_same_location(avant, marantz_apres()),
            IdentityVerdict::SameHardware(Evidence::MakeAndModel)
        );
    }

    // ── Le piege symetrique : deux appareils ne se confondent jamais ──────

    #[test]
    fn deux_mac_differentes_denoncent_un_autre_appareil() {
        let salon = RendererIdentity {
            mac: "00:06:78:7C:2E:26",
            ..MARANTZ_AVANT
        };
        // Meme modele, MEME nom d'usine, autre exemplaire : seule la MAC les
        // separe — et elle suffit.
        let chambre = RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "00:06:78:AA:BB:CC",
            friendly_name: "Marantz ND8006",
            manufacturer: "Marantz",
            model_name: "ND8006",
        };
        assert_eq!(
            compare_at_same_location(salon, chambre),
            IdentityVerdict::OtherHardware(Evidence::Mac),
            "deux exemplaires du meme modele au meme nom doivent rester distincts"
        );
    }

    #[test]
    fn un_modele_different_denonce_un_autre_appareil() {
        let salon = RendererIdentity {
            manufacturer: "Marantz",
            model_name: "ND8006",
            ..MARANTZ_AVANT
        };
        let intrus = RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "",
            friendly_name: "Ampli chambre",
            manufacturer: "Denon",
            model_name: "AVR-X2700H",
        };
        assert_eq!(
            compare_at_same_location(salon, intrus),
            IdentityVerdict::OtherHardware(Evidence::MakeAndModel)
        );
    }

    #[test]
    fn un_nom_different_sur_un_magasin_ancien_denonce_un_autre_appareil() {
        let intrus = RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "",
            friendly_name: "Ampli chambre",
            manufacturer: "",
            model_name: "",
        };
        assert_eq!(
            compare_at_same_location(MARANTZ_AVANT, intrus),
            IdentityVerdict::OtherHardware(Evidence::FriendlyName)
        );
    }

    #[test]
    fn une_mac_qui_diverge_l_emporte_sur_un_nom_identique() {
        let salon = RendererIdentity {
            mac: "00:06:78:7C:2E:26",
            manufacturer: "Marantz",
            model_name: "ND8006",
            ..MARANTZ_AVANT
        };
        let intrus = RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "00:06:78:AA:BB:CC",
            friendly_name: "Marantz ND8006",
            manufacturer: "Marantz",
            model_name: "ND8006",
        };
        assert_eq!(
            compare_at_same_location(salon, intrus),
            IdentityVerdict::OtherHardware(Evidence::Mac),
            "un nom identique ne doit pas racheter une MAC differente"
        );
    }

    // ── L'ignorance ne se travestit ni en preuve ni en accusation ─────────

    #[test]
    fn une_mac_connue_d_un_seul_cote_fait_descendre_d_un_niveau() {
        let avant = RendererIdentity {
            mac: "00:06:78:7C:2E:26",
            ..MARANTZ_AVANT
        };
        // Cache ARP froid : la sonde n'a pas de MAC a opposer.
        assert_eq!(
            compare_at_same_location(avant, marantz_apres()),
            IdentityVerdict::SameHardware(Evidence::FriendlyName),
            "une MAC absente ne doit ni prouver ni accuser : on descend d'un niveau"
        );
    }

    #[test]
    fn un_modele_connu_d_un_seul_cote_fait_descendre_d_un_niveau() {
        // `persisted` sort d'un magasin ancien : ni marque ni modele. Comparer
        // « Marantz » contre du vide accuserait a tort.
        assert_eq!(
            compare_at_same_location(MARANTZ_AVANT, marantz_apres()),
            IdentityVerdict::SameHardware(Evidence::FriendlyName)
        );
    }

    #[test]
    fn sans_aucune_cle_le_verdict_est_indecidable() {
        let anonyme_avant = RendererIdentity {
            udn: "uuid:aaaa",
            ..Default::default()
        };
        let anonyme_apres = RendererIdentity {
            udn: "uuid:bbbb",
            ..Default::default()
        };
        assert_eq!(
            compare_at_same_location(anonyme_avant, anonyme_apres),
            IdentityVerdict::Undecidable,
            "sans nom, sans MAC, sans modele : on ne tranche pas, donc on ne detruit pas"
        );
    }

    #[test]
    fn chaque_niveau_nomme_la_cle_qui_a_tranche() {
        assert_eq!(Evidence::Mac.label(), "son adresse MAC");
        assert_eq!(
            Evidence::MakeAndModel.label(),
            "sa marque, son modèle et son nom"
        );
        assert_eq!(
            Evidence::FriendlyName.label(),
            "son nom, à la même adresse de description"
        );
    }
}
