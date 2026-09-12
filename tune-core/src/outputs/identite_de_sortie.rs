//! Retrouver une sortie locale que son NOM ne désigne plus (#2269, #3737).
//!
//! ## Le défaut
//!
//! Une zone locale est identifiée par `local:{nom du périphérique}` —
//! `LocalOutput::with_options` (`outputs/local.rs`) construit ce `device_id`,
//! et `zones.output_device_id` le stocke tel quel. **Le nom EST l'identité.**
//! Il ne devrait pas l'être : Windows renomme l'endpoint au changement de taux
//! d'échantillonnage (#2269, DEvir), et un flash de micrologiciel peut
//! renommer un DAC. La zone ne retrouve alors plus rien, et l'appareil revenu
//! sous son nouveau nom se voit offrir une zone NEUVE à côté de l'ancienne —
//! réglages, volume et file restés sur la ligne orpheline.
//!
//! L'identifiant qui traverse un renommage EXISTE pourtant depuis #2403 :
//! `AudioDevice::endpoint_id`, capturé à la découverte
//! (`outputs/local.rs`, `device.id()`), et lu EN PREMIER par
//! [`resolve_device`](crate::outputs::local). Rien ne le persistait : `zones`
//! ne portait que `output_device_id`, donc un nom. Ce module est la règle qui
//! s'appuie sur la colonne `zones.output_endpoint_id` désormais posée.
//!
//! ## Ce que ce module fait, et ce qu'il ne fait pas
//!
//! Il ne décide que deux gestes, et REFUSE partout ailleurs :
//!
//! * [`Decision::Apprend`] — la zone enregistre l'identifiant du périphérique
//!   auquel son NOM la lie déjà. Aucun changement d'identité : c'est la
//!   reprise de l'existant, et c'est ce qui rend la colonne utile aux zones
//!   nées avant elle.
//! * [`Decision::Reassocie`] — l'appareil est là, sous un AUTRE nom, et son
//!   identifiant est celui que la zone a enregistré. La zone le retrouve.
//!
//! Il ne fusionne aucune zone (c'est le chantier voisin des zones en double),
//! n'en crée aucune, n'en supprime aucune, et ne renomme jamais un
//! périphérique.
//!
//! ⚠️ **Il ne règle pas le DÉBRANCHEMENT.** Un appareil absent de
//! l'énumération reste introuvable, quel que soit l'identifiant qu'on lui
//! connaisse : les deux résolutions du dépôt (`resolve_device`,
//! `select_wasapi_endpoint`) ne cherchent que parmi les périphériques énumérés
//! à l'instant. Le cas de Jean-Luc Cassé — DAC audio-gd débranché, endpoint
//! absent de l'énumération WASAPI — n'est pas de ceux que ce module rattrape,
//! et [`Decision::Rien`] est alors le verdict honnête.
//!
//! ## Pourquoi une liste BLANCHE de backends, et non une liste noire
//!
//! Le 13 août, une adresse IP d'Apple TV a été reprise par un Sonos : une
//! ré-association fondée sur un identifiant RÉÉMISSIBLE a fait un vrai dégât.
//! La règle qui en découle tient en une phrase — **un identifiant qui peut
//! désigner un autre appareil physique n'identifie rien** — et elle commande
//! ici de juger chaque backend sur ce que cpal 0.17.3 met réellement dans
//! `Device::id()` :
//!
//! | backend | ce que `id()` rend | d'où cpal 0.17.3 le tire | ré-associable |
//! |---|---|---|---|
//! | WASAPI | `wasapi:{0.0.0.00000000}.{guid}` | `IMMDevice::GetId()` (`host/wasapi/device.rs`) | **oui** |
//! | CoreAudio | `coreaudio:<UID>` | `kAudioDevicePropertyDeviceUID` (`host/coreaudio/macos/device.rs`) | **oui** |
//! | ALSA | `alsa:hw:CARD=DACZ8,DEV=0` | `self.pcm_id` (`host/alsa/mod.rs`) | non |
//! | ASIO | `asio:<nom du pilote>` | `driver.name()` (`host/asio/device.rs`) | non |
//!
//! **ASIO est refusé parce que son identifiant EST le nom.** `driver.name()`
//! et rien d'autre : ré-associer là-dessus ne traverserait aucun renommage —
//! c'est la même chaîne des deux côtés — et surtout un pilote générique
//! (« ASIO4ALL v2 ») porte le MÊME identifiant pour des appareils physiques
//! différents. C'est le cas Apple TV / Sonos, à la lettre.
//!
//! **ALSA est refusé parce que son identifiant est un chemin, pas un
//! appareil.** `hw:CARD=X` nomme la carte telle qu'ALSA l'a indexée au
//! branchement ; deux exemplaires du même DAC donnent `X` et `X_1` selon
//! l'ordre de détection, si bien que `hw:CARD=X` peut désigner l'un puis
//! l'autre d'un démarrage à l'autre. Et le nom de carte dérive de la même
//! chaîne produit que le nom d'affichage : il ne survivrait donc pas non plus
//! au renommage qu'on prétend rattraper.
//!
//! La mesure de terrain du 09/09/2026 (#3575, DAC DENAFRIPS sur Tune OS) le
//! dit encore mieux que l'argument : l'identifiant relevé au journal est
//! `alsa:hw:CARD=2,DEV=0` — un **numéro de carte**, attribué à la détection.
//! Le même appareil rebranché après un autre porterait `CARD=3`, et `CARD=2`
//! désignerait alors quelqu'un d'autre. C'est, à la lettre, l'adresse d'Apple
//! TV reprise par un Sonos.
//!
//! Cette même mesure établit aussi que **le côté ALSA n'a pas le défaut de ce
//! ticket** : l'`endpoint_id` y est capturé, retrouvé et utilisé à chaque
//! énumération (dix passes en 43 minutes, appareil retrouvé à chaque tour).
//! Le renommage qui casse une zone est un défaut **WASAPI**, celui de
//! Jean-Luc Cassé, et c'est là que la persistance manquait.
//!
//! Conséquence à dire tout haut, et non à laisser deviner : **cette
//! correction porte sous Windows et macOS, pas sous Linux.** Sous Linux la
//! zone continue d'apprendre son identifiant — il sert au diagnostic — mais
//! aucune ré-association n'en découle.
//!
//! ## Les quatre refus, et pourquoi chacun est un refus et non un pari
//!
//! Même sur un backend autorisé, la ré-association n'a lieu que si elle est
//! CERTAINE. Quatre situations la font renoncer, et chacune est nommée pour
//! qu'un journal puisse la dire :
//!
//! 1. [`Refus::BackendNonReassociable`] — l'identifiant peut être réémis.
//! 2. [`Refus::IdentifiantAmbiguDansLeParc`] — deux périphériques énumérés
//!    portent le même identifiant. Il n'en désigne donc aucun.
//! 3. [`Refus::IdentifiantRevendiqueParPlusieursZones`] — deux zones ont
//!    enregistré le même identifiant. Choisir laquelle rattacher serait un
//!    tirage au sort.
//! 4. [`Refus::NomDejaPrisParUneAutreZone`] — une autre zone porte déjà
//!    `local:{nouveau nom}`. Y déplacer celle-ci serait une FUSION de deux
//!    zones, avec des réglages à arbitrer : ce n'est pas ce module qui en
//!    décide, et l'index unique partiel sur `zones.output_device_id` le
//!    refuserait de toute façon.

use std::collections::HashMap;

/// Le backend d'où vient un identifiant d'endpoint, lu sur son préfixe.
///
/// `cpal::DeviceId` s'affiche `"{hôte}:{identifiant}"`, l'hôte en minuscules
/// (`platform::HostId::Display` rend `name().to_lowercase()`). La comparaison
/// est néanmoins insensible à la casse : les enregistrements écrits à la main
/// dans le dépôt portent `Alsa:` ou `WASAPI:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrigineDeLIdentifiant {
    /// `IMMDevice::GetId()` — un GUID d'endpoint, indépendant du nom.
    Wasapi,
    /// `kAudioDevicePropertyDeviceUID` — l'UID CoreAudio.
    CoreAudio,
    /// Le nom de PCM ALSA (`hw:CARD=…`), attribué à la détection.
    Alsa,
    /// Le nom du pilote ASIO — c'est-à-dire le nom lui-même.
    Asio,
    /// Aucun préfixe reconnu : origine inconnue, donc rien de garanti.
    Inconnue,
}

impl OrigineDeLIdentifiant {
    /// Cet identifiant désigne-t-il un APPAREIL PHYSIQUE que le système ne
    /// réémettra pas à un autre ?
    ///
    /// C'est la seule question qui autorise une ré-association. Voir la
    /// table du module pour la mesure backend par backend.
    pub fn designe_un_appareil_physique(self) -> bool {
        matches!(self, Self::Wasapi | Self::CoreAudio)
    }
}

/// L'origine d'un identifiant d'endpoint, lue sur son préfixe d'hôte.
///
/// Le découpage se fait au PREMIER deux-points, et à lui seul : un PCM ALSA
/// (`hw:CARD=X,DEV=0`) comme un UID CoreAudio
/// (`AppleUSBAudioEngine:Topping:…`) en contiennent d'autres.
pub fn origine_de_l_identifiant(endpoint_id: &str) -> OrigineDeLIdentifiant {
    let Some((tete, _)) = endpoint_id.split_once(':') else {
        return OrigineDeLIdentifiant::Inconnue;
    };
    if tete.eq_ignore_ascii_case("wasapi") {
        OrigineDeLIdentifiant::Wasapi
    } else if tete.eq_ignore_ascii_case("coreaudio") {
        OrigineDeLIdentifiant::CoreAudio
    } else if tete.eq_ignore_ascii_case("alsa") {
        OrigineDeLIdentifiant::Alsa
    } else if tete.eq_ignore_ascii_case("asio") {
        OrigineDeLIdentifiant::Asio
    } else {
        OrigineDeLIdentifiant::Inconnue
    }
}

/// Ce qu'une zone locale sait d'elle-même, tel que la base le porte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneLocale {
    pub id: i64,
    /// `zones.output_device_id`, préfixe `local:` compris.
    pub output_device_id: String,
    /// `zones.output_endpoint_id`. `None` (ou vide) = zone née avant la
    /// colonne, ou périphérique jamais énuméré depuis. **Jamais inventé.**
    pub output_endpoint_id: Option<String>,
    /// `zones.is_hidden` — une zone SUPPRIMÉE est masquée, pas effacée.
    ///
    /// Elle ne bouge jamais : déplacer une zone que l'utilisateur a supprimée
    /// n'est pas une réparation, et la faire réapparaître ailleurs le serait
    /// encore moins. Elle compte en revanche dans les collisions, parce
    /// qu'elle détient bel et bien son `output_device_id` — l'index unique
    /// partiel `idx_zones_output_device_id` ne fait pas la différence.
    pub masquee: bool,
}

/// Un périphérique tel que l'énumération vient de le voir.
///
/// `nom` est le nom d'AFFICHAGE, suffixe `(n)` de désambiguïsation compris —
/// exactement ce que `AudioDevice::name` porte et ce dont `device_id` est
/// dérivé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortieEnumeree {
    pub nom: String,
    /// `AudioDevice::endpoint_id`. Vide sur un hôte qui n'en expose aucun.
    pub endpoint_id: String,
}

impl SortieEnumeree {
    /// L'identifiant de zone que cette sortie porte : `local:{nom}`.
    pub fn device_id(&self) -> String {
        format!("local:{}", self.nom)
    }
}

/// Pourquoi une ré-association possible en apparence n'a PAS eu lieu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refus {
    /// L'identifiant peut être réémis à un autre appareil (ALSA, ASIO,
    /// origine inconnue). Voir la table du module.
    BackendNonReassociable(OrigineDeLIdentifiant),
    /// Plusieurs périphériques énumérés portent cet identifiant.
    IdentifiantAmbiguDansLeParc { candidats: usize },
    /// Plusieurs zones ont enregistré cet identifiant.
    IdentifiantRevendiqueParPlusieursZones { zones: usize },
    /// `local:{nouveau nom}` appartient déjà à une autre zone : ce serait une
    /// fusion, pas une ré-association.
    NomDejaPrisParUneAutreZone { zone: i64 },
}

impl Refus {
    /// Le motif, en une phrase, pour un journal ou un message d'écran.
    pub fn motif(&self) -> String {
        match self {
            Self::BackendNonReassociable(origine) => format!(
                "l'identifiant vient de {origine:?} et peut désigner un autre appareil : \
                 aucune ré-association possible sans risque de confusion"
            ),
            Self::IdentifiantAmbiguDansLeParc { candidats } => format!(
                "{candidats} périphériques énumérés portent le même identifiant : \
                 il n'en désigne aucun"
            ),
            Self::IdentifiantRevendiqueParPlusieursZones { zones } => format!(
                "{zones} zones ont enregistré le même identifiant : \
                 les départager serait un tirage au sort"
            ),
            Self::NomDejaPrisParUneAutreZone { zone } => format!(
                "la zone {zone} porte déjà cet identifiant de sortie : \
                 les réunir serait une fusion de zones, pas une ré-association"
            ),
        }
    }
}

/// Ce qu'il faut faire d'une zone locale, une fois l'énumération connue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Rien à changer : le nom désigne toujours un périphérique présent, ou
    /// l'appareil est absent et aucun identifiant ne le fera revenir.
    Rien { zone_id: i64 },
    /// La zone enregistre l'identifiant du périphérique que son nom désigne
    /// déjà. Aucune identité ne change.
    Apprend { zone_id: i64, endpoint_id: String },
    /// L'appareil est revenu sous un autre nom : la zone le retrouve.
    Reassocie {
        zone_id: i64,
        ancien_device_id: String,
        nouveau_device_id: String,
        endpoint_id: String,
    },
    /// Une ré-association aurait été possible par l'identifiant, mais elle
    /// n'était pas CERTAINE.
    Refuse { zone_id: i64, refus: Refus },
}

impl Decision {
    pub fn zone_id(&self) -> i64 {
        match self {
            Self::Rien { zone_id }
            | Self::Apprend { zone_id, .. }
            | Self::Reassocie { zone_id, .. }
            | Self::Refuse { zone_id, .. } => *zone_id,
        }
    }
}

/// La règle, entière et pure.
///
/// Ni base ni matériel : on lui donne les zones locales et ce que
/// l'énumération vient de rendre, elle rend une décision par zone. C'est ce
/// qui la rend éprouvable sur la machine de compilation, où ni WASAPI ni
/// CoreAudio n'existent.
///
/// L'ordre des zones en entrée est conservé en sortie.
pub fn decider(zones: &[ZoneLocale], enumerees: &[SortieEnumeree]) -> Vec<Decision> {
    // Combien de périphériques énumérés portent chaque identifiant ? Un
    // identifiant porté deux fois ne désigne rien (refus 2).
    let mut parc_par_endpoint: HashMap<&str, Vec<&SortieEnumeree>> = HashMap::new();
    for sortie in enumerees {
        if !sortie.endpoint_id.is_empty() {
            parc_par_endpoint
                .entry(sortie.endpoint_id.as_str())
                .or_default()
                .push(sortie);
        }
    }
    // Combien de périphériques énumérés portent chaque nom ? La découverte
    // désambiguïse déjà les homonymes par un suffixe `(n)`, donc ce compte
    // vaut 1 partout — mais si jamais il ne le valait pas, apprendre depuis
    // un nom ambigu écrirait l'identifiant du mauvais appareil.
    let mut parc_par_device_id: HashMap<String, Vec<&SortieEnumeree>> = HashMap::new();
    for sortie in enumerees {
        parc_par_device_id
            .entry(sortie.device_id())
            .or_default()
            .push(sortie);
    }
    // Combien de zones revendiquent chaque identifiant ? (refus 3)
    let mut zones_par_endpoint: HashMap<&str, usize> = HashMap::new();
    for zone in zones {
        if let Some(id) = zone.output_endpoint_id.as_deref().filter(|i| !i.is_empty()) {
            *zones_par_endpoint.entry(id).or_default() += 1;
        }
    }
    // Qui détient quel `output_device_id` ? (refus 4)
    let mut zone_par_device_id: HashMap<&str, i64> = HashMap::new();
    for zone in zones {
        zone_par_device_id
            .entry(zone.output_device_id.as_str())
            .or_insert(zone.id);
    }

    zones
        .iter()
        .map(|zone| {
            decider_une(
                zone,
                &parc_par_endpoint,
                &parc_par_device_id,
                &zones_par_endpoint,
                &zone_par_device_id,
            )
        })
        .collect()
}

fn decider_une(
    zone: &ZoneLocale,
    parc_par_endpoint: &HashMap<&str, Vec<&SortieEnumeree>>,
    parc_par_device_id: &HashMap<String, Vec<&SortieEnumeree>>,
    zones_par_endpoint: &HashMap<&str, usize>,
    zone_par_device_id: &HashMap<&str, i64>,
) -> Decision {
    let rien = Decision::Rien { zone_id: zone.id };
    // Ce module ne parle que des sorties LOCALES. Une zone réseau porte une
    // identité d'une tout autre nature (UUID UPnP, MAC), tenue ailleurs.
    if !zone.output_device_id.starts_with("local:") {
        return rien;
    }
    // Une zone supprimée est masquée, pas effacée. Elle a déjà été comptée
    // dans les collisions ci-dessus — c'est tout ce qu'on lui demande.
    if zone.masquee {
        return rien;
    }

    // Le nom de la zone désigne-t-il encore un périphérique présent ?
    let presente_sous_son_nom = parc_par_device_id.get(&zone.output_device_id);

    let endpoint_connu = zone
        .output_endpoint_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());

    match (presente_sous_son_nom, endpoint_connu) {
        // Le nom marche encore. Si la zone ne connaît pas encore son
        // identifiant, c'est le moment de l'APPRENDRE — et c'est le seul :
        // l'appareil est là, et c'est bien LUI que la zone désigne déjà.
        (Some(candidats), None) => {
            // Un nom qui désignerait deux périphériques n'apprend rien : on
            // ne saurait pas duquel des deux prendre l'identifiant.
            if candidats.len() != 1 {
                return rien;
            }
            let endpoint_id = candidats[0].endpoint_id.trim();
            if endpoint_id.is_empty() {
                // L'hôte n'expose aucun identifiant. Écrire une chaîne vide
                // ferait passer une absence de renseignement pour un
                // renseignement.
                return rien;
            }
            Decision::Apprend {
                zone_id: zone.id,
                endpoint_id: endpoint_id.to_string(),
            }
        }
        // Le nom marche encore et l'identifiant est déjà connu : rien à faire.
        // On ne le RÉÉCRIT pas même s'il a changé — ce serait effacer la
        // seule trace de l'identité d'origine sur la foi d'un nom, c'est-à-dire
        // exactement l'inverse de ce chantier.
        (Some(_), Some(_)) => rien,
        // Le nom ne désigne plus rien, et la zone n'a aucun identifiant : il
        // n'y a rien à quoi se raccrocher. C'est le cas des zones nées avant
        // la colonne dont le DAC a été renommé entre-temps : elles ne peuvent
        // plus être rattrapées, seulement ne plus se reproduire.
        (None, None) => rien,
        // Le nom ne désigne plus rien, mais la zone connaît son identifiant.
        // C'est ici, et seulement ici, que la ré-association se joue.
        (None, Some(endpoint_id)) => {
            let origine = origine_de_l_identifiant(endpoint_id);
            if !origine.designe_un_appareil_physique() {
                return Decision::Refuse {
                    zone_id: zone.id,
                    refus: Refus::BackendNonReassociable(origine),
                };
            }
            let Some(candidats) = parc_par_endpoint.get(endpoint_id) else {
                // L'appareil n'est pas là. Aucun identifiant ne fait revenir
                // un périphérique débranché — c'est le cas de Jean-Luc Cassé,
                // et le verdict honnête est de ne rien faire.
                return rien;
            };
            if candidats.len() != 1 {
                return Decision::Refuse {
                    zone_id: zone.id,
                    refus: Refus::IdentifiantAmbiguDansLeParc {
                        candidats: candidats.len(),
                    },
                };
            }
            let revendications = zones_par_endpoint.get(endpoint_id).copied().unwrap_or(0);
            if revendications > 1 {
                return Decision::Refuse {
                    zone_id: zone.id,
                    refus: Refus::IdentifiantRevendiqueParPlusieursZones {
                        zones: revendications,
                    },
                };
            }
            let nouveau_device_id = candidats[0].device_id();
            if let Some(&autre) = zone_par_device_id.get(nouveau_device_id.as_str())
                && autre != zone.id
            {
                return Decision::Refuse {
                    zone_id: zone.id,
                    refus: Refus::NomDejaPrisParUneAutreZone { zone: autre },
                };
            }
            Decision::Reassocie {
                zone_id: zone.id,
                ancien_device_id: zone.output_device_id.clone(),
                nouveau_device_id,
                endpoint_id: endpoint_id.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(id: i64, device_id: &str, endpoint: Option<&str>) -> ZoneLocale {
        ZoneLocale {
            id,
            output_device_id: device_id.to_string(),
            output_endpoint_id: endpoint.map(str::to_string),
            masquee: false,
        }
    }

    fn zone_masquee(id: i64, device_id: &str, endpoint: Option<&str>) -> ZoneLocale {
        ZoneLocale {
            masquee: true,
            ..zone(id, device_id, endpoint)
        }
    }

    fn sortie(nom: &str, endpoint: &str) -> SortieEnumeree {
        SortieEnumeree {
            nom: nom.to_string(),
            endpoint_id: endpoint.to_string(),
        }
    }

    const AUDIO_GD: &str = "wasapi:{0.0.0.00000000}.{e0ea21cf-56cc-445f-8454-880d431d7cb0}";
    const HAUT_PARLEURS: &str = "wasapi:{0.0.0.00000000}.{7bd3a1de-0000-4444-9999-1a2b3c4d5e6f}";

    // ── Le témoin 1 : un appareil RENOMMÉ est retrouvé ────────────────────

    /// #2269, mot pour mot : Windows renomme l'endpoint au changement de taux
    /// d'échantillonnage. La zone porte l'ancien nom, l'appareil est là sous
    /// le nouveau, et le GUID d'endpoint n'a pas bougé.
    #[test]
    fn un_appareil_renomme_est_retrouve_par_son_identifiant() {
        let zones = vec![zone(1, "local:audio-gd USB audio", Some(AUDIO_GD))];
        let parc = vec![sortie("audio-gd USB audio (44,1 kHz)", AUDIO_GD)];

        assert_eq!(
            decider(&zones, &parc),
            vec![Decision::Reassocie {
                zone_id: 1,
                ancien_device_id: "local:audio-gd USB audio".into(),
                nouveau_device_id: "local:audio-gd USB audio (44,1 kHz)".into(),
                endpoint_id: AUDIO_GD.into(),
            }],
            "la zone doit suivre son appareil sous son nouveau nom : c'est \
             tout l'objet de #2269"
        );
    }

    // ── Le témoin 2 : un AUTRE appareil n'est PAS pris pour l'ancien ──────

    /// Le contre-exemple qui commande tout le module : le 13/08, une adresse
    /// d'Apple TV reprise par un Sonos. Ici l'appareil présent porte un AUTRE
    /// identifiant — il ne doit hériter de rien.
    #[test]
    fn un_autre_appareil_nest_pas_pris_pour_lancien() {
        let zones = vec![zone(1, "local:audio-gd USB audio", Some(AUDIO_GD))];
        // Le DAC est débranché ; il ne reste que les haut-parleurs, qui
        // portent un GUID différent — c'est exactement le parc de Jean-Luc.
        let parc = vec![sortie("Haut-parleurs", HAUT_PARLEURS)];

        assert_eq!(
            decider(&zones, &parc),
            vec![Decision::Rien { zone_id: 1 }],
            "un identifiant qui ne correspond à rien ne doit RIEN rattacher : \
             rabattre la zone sur le seul appareil présent est le dégât du 13/08"
        );
    }

    /// La même chose sous l'angle du nom : deux appareils différents peuvent
    /// porter le même NOM. Seul l'identifiant tranche, et il ne tranche que
    /// dans un sens.
    #[test]
    fn un_homonyme_au_mauvais_identifiant_ne_capte_pas_la_zone() {
        let zones = vec![zone(1, "local:Topping D10s", Some(AUDIO_GD))];
        // Un second exemplaire du même modèle, branché ailleurs : même nom
        // rendu par le pilote, GUID différent.
        let parc = vec![sortie("Topping D10s (2)", HAUT_PARLEURS)];

        assert_eq!(decider(&zones, &parc), vec![Decision::Rien { zone_id: 1 }]);
    }

    // ── Les quatre refus ──────────────────────────────────────────────────

    /// ASIO : `Device::id()` rend `driver.name()`. Un pilote générique porte
    /// le même identifiant pour des appareils différents.
    #[test]
    fn asio_ne_reassocie_jamais_son_identifiant_est_le_nom() {
        let zones = vec![zone(
            1,
            "local:HoloAudio ASIO Driver",
            Some("asio:ASIO4ALL v2"),
        )];
        let parc = vec![sortie("Autre DAC ASIO", "asio:ASIO4ALL v2")];

        assert_eq!(
            decider(&zones, &parc),
            vec![Decision::Refuse {
                zone_id: 1,
                refus: Refus::BackendNonReassociable(OrigineDeLIdentifiant::Asio),
            }],
            "« ASIO4ALL v2 » désigne le PILOTE, pas l'appareil : deux DAC \
             derrière lui porteraient le même identifiant"
        );
    }

    /// ALSA : `hw:CARD=X` est un rang de détection, pas un appareil.
    #[test]
    fn alsa_ne_reassocie_jamais_son_identifiant_est_un_rang() {
        let zones = vec![zone(1, "local:DACZ8", Some("alsa:hw:CARD=DACZ8,DEV=0"))];
        let parc = vec![sortie("Un tout autre DAC", "alsa:hw:CARD=DACZ8,DEV=0")];

        assert_eq!(
            decider(&zones, &parc),
            vec![Decision::Refuse {
                zone_id: 1,
                refus: Refus::BackendNonReassociable(OrigineDeLIdentifiant::Alsa),
            }]
        );
    }

    #[test]
    fn un_identifiant_sans_prefixe_connu_ne_reassocie_pas() {
        let zones = vec![zone(
            1,
            "local:Ancien",
            Some("{0.0.0.00000000}.{sans-prefixe}"),
        )];
        let parc = vec![sortie("Nouveau", "{0.0.0.00000000}.{sans-prefixe}")];

        assert_eq!(
            decider(&zones, &parc),
            vec![Decision::Refuse {
                zone_id: 1,
                refus: Refus::BackendNonReassociable(OrigineDeLIdentifiant::Inconnue),
            }]
        );
    }

    #[test]
    fn deux_peripheriques_au_meme_identifiant_ne_designent_rien() {
        let zones = vec![zone(1, "local:Ancien nom", Some(AUDIO_GD))];
        let parc = vec![sortie("Premier", AUDIO_GD), sortie("Second", AUDIO_GD)];

        assert_eq!(
            decider(&zones, &parc),
            vec![Decision::Refuse {
                zone_id: 1,
                refus: Refus::IdentifiantAmbiguDansLeParc { candidats: 2 },
            }]
        );
    }

    #[test]
    fn deux_zones_au_meme_identifiant_ne_se_departagent_pas() {
        let zones = vec![
            zone(1, "local:Ancien nom", Some(AUDIO_GD)),
            zone(2, "local:Autre ancien nom", Some(AUDIO_GD)),
        ];
        let parc = vec![sortie("Nouveau nom", AUDIO_GD)];

        assert_eq!(
            decider(&zones, &parc),
            vec![
                Decision::Refuse {
                    zone_id: 1,
                    refus: Refus::IdentifiantRevendiqueParPlusieursZones { zones: 2 },
                },
                Decision::Refuse {
                    zone_id: 2,
                    refus: Refus::IdentifiantRevendiqueParPlusieursZones { zones: 2 },
                },
            ]
        );
    }

    /// Le recoupement avec le chantier des zones en double : rattacher ici
    /// ferait DEUX zones sur un seul appareil, ce que l'index unique partiel
    /// sur `output_device_id` refuse de toute façon.
    #[test]
    fn une_reassociation_qui_serait_une_fusion_est_refusee() {
        let zones = vec![
            zone(1, "local:Ancien nom", Some(AUDIO_GD)),
            zone(2, "local:Nouveau nom", None),
        ];
        let parc = vec![sortie("Nouveau nom", AUDIO_GD)];

        let decisions = decider(&zones, &parc);
        assert_eq!(
            decisions[0],
            Decision::Refuse {
                zone_id: 1,
                refus: Refus::NomDejaPrisParUneAutreZone { zone: 2 },
            },
            "fusionner deux zones est un autre chantier, et un autre arbitrage"
        );
        // La zone 2, elle, apprend tranquillement l'identifiant de l'appareil
        // qu'elle désigne déjà par son nom.
        assert_eq!(
            decisions[1],
            Decision::Apprend {
                zone_id: 2,
                endpoint_id: AUDIO_GD.into(),
            }
        );
    }

    // ── La reprise de l'existant ──────────────────────────────────────────

    /// Le point délicat de la migration : les zones VIVANTES d'aujourd'hui
    /// n'ont pas d'identifiant. Elles ne doivent RIEN perdre, et l'apprendre
    /// ne change pas leur identité.
    #[test]
    fn une_zone_dhier_apprend_son_identifiant_sans_changer_didentite() {
        let zones = vec![zone(1, "local:audio-gd USB audio", None)];
        let parc = vec![sortie("audio-gd USB audio", AUDIO_GD)];

        assert_eq!(
            decider(&zones, &parc),
            vec![Decision::Apprend {
                zone_id: 1,
                endpoint_id: AUDIO_GD.into(),
            }],
            "`output_device_id` ne bouge pas : aucun réglage accroché à la \
             zone ne se détache"
        );
    }

    /// Un hôte qui n'expose aucun identifiant ne doit pas en faire écrire un
    /// vide : une absence de renseignement n'est pas un renseignement.
    #[test]
    fn un_endpoint_vide_ne_sapprend_pas() {
        let zones = vec![zone(1, "local:Sortie sans identifiant", None)];
        let parc = vec![sortie("Sortie sans identifiant", "")];

        assert_eq!(decider(&zones, &parc), vec![Decision::Rien { zone_id: 1 }]);
    }

    /// Une zone dont le nom marche encore n'est jamais touchée — même si un
    /// autre appareil du parc porte l'identifiant qu'elle a enregistré.
    #[test]
    fn une_zone_qui_marche_nest_jamais_deplacee() {
        let zones = vec![zone(1, "local:Topping D10s", Some(AUDIO_GD))];
        let parc = vec![
            sortie("Topping D10s", HAUT_PARLEURS),
            sortie("Ailleurs", AUDIO_GD),
        ];

        assert_eq!(
            decider(&zones, &parc),
            vec![Decision::Rien { zone_id: 1 }],
            "déplacer une zone qui joue, sur la foi d'un identifiant, \
             couperait le son de quelqu'un"
        );
    }

    /// Une zone SUPPRIMÉE (masquée) ne se déplace jamais — mais elle détient
    /// bel et bien son identifiant de sortie, et le fait savoir.
    #[test]
    fn une_zone_supprimee_ne_bouge_pas_et_bloque_encore_son_nom() {
        let masquee = zone_masquee(1, "local:Ancien nom", Some(AUDIO_GD));
        let parc = vec![sortie("Nouveau nom", AUDIO_GD)];
        assert_eq!(
            decider(&[masquee.clone()], &parc),
            vec![Decision::Rien { zone_id: 1 }],
            "déplacer une zone que l'utilisateur a supprimée la ferait \
             réapparaître ailleurs"
        );

        // Et son `output_device_id` reste pris : une zone vivante ne peut pas
        // s'y installer, l'index unique partiel le refuserait.
        let vivante = zone(2, "local:Autre ancien nom", Some(HAUT_PARLEURS));
        let parc = vec![sortie("Ancien nom", HAUT_PARLEURS)];
        assert_eq!(
            decider(&[masquee, vivante], &parc)[1],
            Decision::Refuse {
                zone_id: 2,
                refus: Refus::NomDejaPrisParUneAutreZone { zone: 1 },
            }
        );
    }

    /// Une zone RÉSEAU ne relève pas de ce module, quel que soit ce qu'elle
    /// porte.
    #[test]
    fn une_zone_reseau_est_hors_de_portee() {
        let zones = vec![ZoneLocale {
            id: 1,
            output_device_id: "uuid:1234-5678".into(),
            output_endpoint_id: Some(AUDIO_GD.into()),
            masquee: false,
        }];
        let parc = vec![sortie("Nouveau nom", AUDIO_GD)];

        assert_eq!(decider(&zones, &parc), vec![Decision::Rien { zone_id: 1 }]);
    }

    /// Le parc VIDE — énumération en cours, pilote happé par une autre
    /// application — ne doit rien décider du tout. C'est la garde que #3737
    /// réclame nommément pour son point 1.
    #[test]
    fn un_parc_vide_ne_decide_rien() {
        let zones = vec![
            zone(1, "local:audio-gd USB audio", Some(AUDIO_GD)),
            zone(2, "local:Haut-parleurs", None),
        ];

        assert_eq!(
            decider(&zones, &[]),
            vec![Decision::Rien { zone_id: 1 }, Decision::Rien { zone_id: 2 }]
        );
    }

    // ── La lecture du préfixe ─────────────────────────────────────────────

    #[test]
    fn le_prefixe_dhote_se_lit_sans_se_laisser_prendre_aux_deux_points_internes() {
        assert_eq!(
            origine_de_l_identifiant("alsa:hw:CARD=DACZ8,DEV=0"),
            OrigineDeLIdentifiant::Alsa
        );
        assert_eq!(
            origine_de_l_identifiant("coreaudio:AppleUSBAudioEngine:Topping:D10s:14200000:1"),
            OrigineDeLIdentifiant::CoreAudio
        );
        assert_eq!(
            origine_de_l_identifiant("WASAPI:{0.0.0.00000000}.{abc}"),
            OrigineDeLIdentifiant::Wasapi
        );
        assert_eq!(
            origine_de_l_identifiant(""),
            OrigineDeLIdentifiant::Inconnue
        );
        assert_eq!(
            origine_de_l_identifiant("sans-deux-points"),
            OrigineDeLIdentifiant::Inconnue
        );
    }

    #[test]
    fn seuls_wasapi_et_coreaudio_designent_un_appareil_physique() {
        assert!(OrigineDeLIdentifiant::Wasapi.designe_un_appareil_physique());
        assert!(OrigineDeLIdentifiant::CoreAudio.designe_un_appareil_physique());
        assert!(!OrigineDeLIdentifiant::Alsa.designe_un_appareil_physique());
        assert!(!OrigineDeLIdentifiant::Asio.designe_un_appareil_physique());
        assert!(!OrigineDeLIdentifiant::Inconnue.designe_un_appareil_physique());
    }
}
