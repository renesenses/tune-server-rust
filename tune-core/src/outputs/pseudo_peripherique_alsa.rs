//! Les pseudo-périphériques ALSA : ce qui porte un nom de sortie sans conduire
//! nulle part.
//!
//! ## Le défaut
//!
//! Sur le `.18` (Ubuntu 24.04, mesuré le 18/09/2026), l'écran Zones affichait
//! une zone locale nommée :
//!
//! > « Discard all samples (playback) or generate zero samples (capture) »
//!
//! `GET /api/v1/zones` la donnait avec `"output_type": "local"` et
//! `"output_device_id": "local:Discard all samples (playback) or generate zero
//! samples (capture)"`. Ce n'est pas un nom d'appareil : c'est la **description**
//! que `alsa-lib` attache au PCM `null`. Et `null` jette tout par construction —
//! `aplay -L` sur cette machine ne rend d'ailleurs que lui :
//!
//! ```text
//! null
//!     Discard all samples (playback) or generate zero samples (capture)
//! ```
//!
//! Un utilisateur qui choisit cette zone envoie donc son audio dans un puits,
//! sans le moindre témoin.
//!
//! ## Sur QUOI on décide
//!
//! Sur le **nom de PCM ALSA**, jamais sur la description.
//!
//! La description est du texte libre : elle vient des fichiers de configuration
//! d'`alsa-lib`, elle varie d'une distribution et d'une version à l'autre, et
//! rien ne garantit qu'elle reste en anglais. Une garde qui cherche « Discard
//! all samples » dans un libellé est vraie tant que personne ne touche au
//! libellé.
//!
//! Le nom de PCM, lui, est un identifiant. cpal le rend tel quel dans
//! `Device::id()` — `DeviceId(HostId::Alsa, pcm_id)`, affiché « `Alsa:null` »
//! (`cpal-0.17.3`, `src/host/alsa/mod.rs:448` et `src/lib.rs:255`) — et ce
//! `pcm_id` est exactement ce que `snd_device_name_hint` a nommé, ou le
//! `hw:CARD=…,DEV=…` que cpal fabrique pour les cartes physiques
//! (`src/host/alsa/enumerate.rs`). C'est ce nom-là que cette famille lit.
//!
//! ## Ce qui est écarté, et ce qui ne l'est PAS
//!
//! Écarté :
//!
//! - le greffon `null` — le puits d'ALSA ;
//! - la carte `Dummy` — le module noyau `snd-dummy`, une VRAIE carte ALSA qui
//!   ne pilote aucun convertisseur.
//!
//! **Conservé**, et c'est la moitié qui compte : `hw:`, `plughw:`, `default`,
//! `sysdefault:`, `dmix:`, `front:`, `iec958:`, `pulse`, `pipewire`, `jack`…
//! Tous ces chemins mènent à du son. Tout Tune OS sur Raspberry Pi est déjà
//! sans sortie audio locale depuis le 20/05 : sur ces machines la liste est
//! vide ou presque, et une garde trop large y supprimerait la dernière zone.
//! Élargir la règle ci-dessous, c'est risquer exactement cela.
//!
//! ## Pourquoi ici, et pas sous la feature
//!
//! `outputs::local` vit sous `#[cfg(feature = "local-audio")]`, et la porte
//! `test` de la CI ne compile PAS cette feature pour `tune-core`. Une épreuve
//! posée là-bas ne tournerait jamais en intégration continue. Cette famille-ci
//! est une décision PURE — aucune FFI, aucun `cfg` de plateforme, aucun
//! périphérique — donc elle se juge partout, y compris sur une machine sans
//! carte son. Même patron que `identite_de_sortie` et que
//! `negociation_format_exclusif_3837`.

/// Le nom de PCM ALSA porté par un `endpoint_id` cpal, sans le préfixe d'hôte.
///
/// cpal rend `DeviceId` sous la forme `«hôte»:«pcm»` (`Display`, `cpal-0.17.3`
/// `src/lib.rs:255`), et le `pcm` d'ALSA est lui-même préfixé par son greffon
/// (`hw:CARD=…`, `dmix:CARD=…`). On ne retire donc QUE le préfixe d'hôte, et
/// seulement s'il est présent : certains enregistrements ne portent que le PCM.
pub fn pcm_alsa(endpoint_id: &str) -> &str {
    let Some((tete, reste)) = endpoint_id.split_once(':') else {
        return endpoint_id;
    };
    if tete.eq_ignore_ascii_case("alsa") {
        reste
    } else {
        endpoint_id
    }
}

/// Le greffon d'un PCM ALSA : sa tête, avant le premier `:`.
///
/// `hw:CARD=0,DEV=0` → `hw` ; `null` → `null` ; `default` → `default`.
pub fn greffon_alsa(pcm: &str) -> &str {
    pcm.split(':').next().unwrap_or(pcm)
}

/// La carte qu'un PCM ALSA nomme (`CARD=…`), quand il en nomme une.
///
/// `hw:CARD=Dummy,DEV=0` → `Some("Dummy")` ; `null` → `None`.
pub fn carte_alsa(pcm: &str) -> Option<&str> {
    pcm.split(',')
        .find_map(|champ| champ.split_once("CARD="))
        .map(|(_, carte)| carte)
}

/// Ce périphérique est-il un PUITS — un point de sortie qui jette les
/// échantillons au lieu de les rendre ?
///
/// `description` n'est PAS le critère : elle ne sert que de filet quand
/// l'endpoint est inconnu (`endpoint_id` vide, c'est-à-dire un `Device::id()`
/// en échec). Sur ALSA ce cas n'arrive pas — `id()` y rend toujours `Ok` —
/// mais un inventaire reconstruit d'ailleurs peut, lui, ne porter que le nom.
/// Le filet reprend alors mot pour mot la règle historique du 01/07/2026, pour
/// ne rien perdre de ce qu'elle attrapait déjà.
pub fn est_un_puits(endpoint_id: &str, description: &str) -> bool {
    if endpoint_id.is_empty() {
        return description_de_puits(description);
    }
    let pcm = pcm_alsa(endpoint_id);
    if greffon_alsa(pcm).eq_ignore_ascii_case("null") {
        return true;
    }
    carte_alsa(pcm).is_some_and(|carte| carte.eq_ignore_ascii_case("dummy"))
}

/// Le filet de dernier recours : la description d'`alsa-lib`, telle que la
/// règle du 01/07/2026 la lisait. Fragile par nature — voir l'en-tête du
/// module — d'où son unique appelant, l'endpoint manquant.
fn description_de_puits(description: &str) -> bool {
    description.contains("Discard all samples") || description.contains("Dummy")
}

/// #7263 — les greffons d'`alsa-plugins` qui n'ont AUCUN matériel propre :
/// serveur de son tiers (`jack`, `oss`), convertisseurs de cadence et de
/// canaux, encodeurs. Noms exacts.
const GREFFONS_LOGICIELS: &[&str] = &["jack", "oss", "speex", "upmix", "vdownmix", "a52", "dca"];

/// #7263 — les familles de convertisseurs de cadence, déclinées en variantes
/// (`speexrate_medium`, `samplerate_best`, `lavrate_high`…). Préfixes.
const FAMILLES_DE_CONVERTISSEURS: &[&str] = &["speexrate", "samplerate", "lavrate", "lavcrate"];

/// #7263 — ce PCM ALSA est-il un greffon LOGICIEL sans matériel propre, qu'il
/// ne faut pas OUVRIR pour dresser l'inventaire ?
///
/// JeromeQ (fils 2491, 2517, 2525 ; Linux, rc8.1) : le serveur meurt sans
/// panique Rust, chaque fois pendant l'inventaire des sorties, juste après
/// `local_audio_device_found … endpoint_id=alsa:speexrate`. Le PCM suivant de
/// toutes les énumérations complètes du même journal est `alsa:jack`. Sonder
/// ses capacités (`supported_output_configs`, puis `default_output_config`)
/// passe par `snd_pcm_open`, qui charge `libasound_module_pcm_jack.so`, donc
/// libjack (ou pipewire-jack) DANS le processus de Tune. Un plantage natif là
/// ne laisse aucune trace Rust. Et l'inventaire tournait toutes les 5 s tant
/// que l'écran « État du serveur » était ouvert.
///
/// Aucun de ces PCM n'est une sortie utile à Tune : ils passent tous par un
/// convertisseur ou par un autre serveur de son. Ils RESTENT dans la liste
/// (personne ne perd une zone déjà créée), mais avec des capacités
/// supposées, sans ouverture.
///
/// **Conservés et sondés comme avant** : `hw:`, `plughw:`, `default`,
/// `sysdefault:`, `dmix:`, `front:`, `iec958:`, `pulse`, `pipewire` — ce sont
/// les chemins vers les cartes et les DAC USB.
pub fn greffon_logiciel_sans_materiel(endpoint_id: &str) -> bool {
    let greffon = greffon_alsa(pcm_alsa(endpoint_id)).to_ascii_lowercase();
    GREFFONS_LOGICIELS.contains(&greffon.as_str())
        || FAMILLES_DE_CONVERTISSEURS
            .iter()
            .any(|famille| greffon.starts_with(famille))
}

/// #7263 — les capacités d'un point de sortie à l'inventaire : `sonder` ouvre
/// le PCM, `supposer` ne touche à rien. Un greffon logiciel sans matériel
/// ([`greffon_logiciel_sans_materiel`]) n'est JAMAIS sondé.
///
/// Générique pour être jugée sans carte son ni feature `local-audio` : la
/// porte `test` de la CI la joue, et c'est l'unique chemin par lequel
/// l'énumération (`outputs::local::parc`) obtient les capacités.
pub fn capacites_a_l_inventaire<T>(
    endpoint_id: &str,
    sonder: impl FnOnce() -> T,
    supposer: impl FnOnce() -> T,
) -> T {
    if greffon_logiciel_sans_materiel(endpoint_id) {
        supposer()
    } else {
        sonder()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un inventaire CONSTRUIT, sur le patron de ce que cpal rend :
    /// `(endpoint_id, description)`. Shrek n'a aucune carte son
    /// (`/proc/asound` absent) : la garde porte sur la DÉCISION, pas sur une
    /// énumération réelle.
    const INVENTAIRE: &[(&str, &str)] = &[
        // Le puits du .18, relevé le 18/09/2026 par `aplay -L`.
        (
            "Alsa:null",
            "Discard all samples (playback) or generate zero samples (capture)",
        ),
        // Le MÊME puits, dont la description aurait changé — traduite, ou
        // réécrite par une version d'alsa-lib. C'est cette ligne qui distingue
        // la règle d'aujourd'hui de celle du 01/07/2026 : sans elle, la garde
        // resterait verte sur l'ancienne.
        ("Alsa:null", "Jette tous les échantillons (lecture)"),
        // La carte physique du .18 (`/proc/asound/pcm` : CS4206 Analog),
        // telle que cpal la fabrique dans `enumerate.rs`.
        (
            "Alsa:hw:CARD=0,DEV=0",
            "HDA Intel PCH, CS4206 Analog\nDirect hardware device without any conversions",
        ),
        // Un vrai DAC USB, nommé par sa carte.
        ("Alsa:hw:CARD=DACZ8,DEV=0", "Eversolo DAC-Z8, USB Audio"),
    ];

    #[test]
    fn le_puits_est_ecarte_et_le_materiel_est_conserve() {
        let retenus: Vec<&str> = INVENTAIRE
            .iter()
            .filter(|(endpoint, desc)| !est_un_puits(endpoint, desc))
            .map(|(endpoint, _)| *endpoint)
            .collect();

        // Sens 1 : le pseudo-périphérique disparaît.
        assert!(
            !retenus.contains(&"Alsa:null"),
            "le PCM `null` doit être écarté : il jette tout par construction"
        );
        // Sens 2 — le piège : les VRAIES sorties restent. Une garde qui ne
        // prouve que le premier sens laisse passer un filtre qui vide la liste
        // et rend la lecture locale impossible.
        assert_eq!(
            retenus,
            vec!["Alsa:hw:CARD=0,DEV=0", "Alsa:hw:CARD=DACZ8,DEV=0"],
            "les deux sorties matérielles doivent survivre au filtre"
        );
    }

    #[test]
    fn le_puits_se_reconnait_au_pcm_pas_a_la_description() {
        // Description vide, traduite, ou changée par une mise à jour
        // d'alsa-lib : la décision ne bouge pas.
        assert!(est_un_puits("Alsa:null", ""));
        assert!(est_un_puits(
            "Alsa:null",
            "Jette tous les échantillons (lecture)"
        ));
        // La casse de l'hôte ne décide de rien non plus.
        assert!(est_un_puits("alsa:null", ""));
        // Et un PCM seul, sans préfixe d'hôte, se lit pareil.
        assert!(est_un_puits("null", ""));
    }

    #[test]
    fn la_carte_dummy_du_noyau_est_un_puits() {
        // `snd-dummy` enregistre une VRAIE carte ALSA qui ne pilote rien.
        assert!(est_un_puits("Alsa:hw:CARD=Dummy,DEV=0", "Dummy, Dummy PCM"));
        assert!(est_un_puits("Alsa:plughw:CARD=Dummy,DEV=0", ""));
        assert!(est_un_puits("Alsa:sysdefault:CARD=Dummy", ""));
    }

    #[test]
    fn les_greffons_qui_menent_a_du_son_sont_conserves() {
        // 🔴 Le piège. Tout Tune OS sur Raspberry Pi est sans sortie audio
        // locale depuis le 20/05 : écarter l'un de ces chemins y supprimerait
        // la dernière zone.
        for endpoint in [
            "Alsa:default",
            "Alsa:sysdefault:CARD=PCH",
            "Alsa:plughw:CARD=0,DEV=0",
            "Alsa:dmix:CARD=PCH,DEV=0",
            "Alsa:front:CARD=PCH,DEV=0",
            "Alsa:iec958:CARD=PCH,DEV=0",
            "Alsa:pulse",
            "Alsa:pipewire",
            "Alsa:jack",
            "Alsa:hw:CARD=0,DEV=0",
        ] {
            assert!(
                !est_un_puits(endpoint, "Default Audio Device"),
                "{endpoint} mène à du son : il doit être conservé"
            );
        }
    }

    #[test]
    fn les_autres_hotes_ne_sont_pas_concernes() {
        // Le préfixe n'est pas `Alsa:` : rien à décider, et surtout pas sur la
        // foi d'un nom qui contiendrait « null » par hasard.
        assert!(!est_un_puits(
            "Wasapi:{0.0.0.00000000}.{a1b2}",
            "Haut-Parleurs"
        ));
        assert!(!est_un_puits("CoreAudio:AppleHDA", "Haut-parleurs MacBook"));
        // Et un nom de greffon qui COMMENCE par « null » n'est pas `null`.
        assert!(!est_un_puits("Alsa:nullsink:CARD=X", ""));
    }

    #[test]
    fn sans_endpoint_le_filet_historique_reprend_la_main() {
        assert!(est_un_puits(
            "",
            "Discard all samples (playback) or generate zero samples (capture)"
        ));
        assert!(est_un_puits("", "Dummy, Dummy PCM"));
        // Et il ne mange rien d'autre.
        assert!(!est_un_puits("", "CS4206 Analog"));
        assert!(!est_un_puits("", ""));
    }

    #[test]
    fn decoupe_du_pcm() {
        assert_eq!(pcm_alsa("Alsa:hw:CARD=0,DEV=0"), "hw:CARD=0,DEV=0");
        assert_eq!(pcm_alsa("hw:CARD=0,DEV=0"), "hw:CARD=0,DEV=0");
        assert_eq!(pcm_alsa("Wasapi:{abc}"), "Wasapi:{abc}");
        assert_eq!(greffon_alsa("hw:CARD=0,DEV=0"), "hw");
        assert_eq!(greffon_alsa("null"), "null");
        assert_eq!(carte_alsa("hw:CARD=Dummy,DEV=0"), Some("Dummy"));
        assert_eq!(carte_alsa("null"), None);
    }

    /// Ouvre-t-on ce PCM à l'inventaire ? Compte les sondes réellement faites.
    fn sondes_pour(endpoint_id: &str) -> usize {
        let mut sondes = 0;
        capacites_a_l_inventaire(endpoint_id, || sondes += 1, || ());
        sondes
    }

    #[test]
    fn les_greffons_logiciels_ne_sont_jamais_ouverts_a_l_inventaire_7263() {
        // L'ordre du journal de JeromeQ : speexrate, puis jack — le processus
        // meurt pendant la sonde de jack.
        for endpoint in [
            "alsa:speexrate",
            "alsa:jack",
            "Alsa:jack",
            "alsa:oss",
            "alsa:speexrate_medium",
            "alsa:speexrate_best",
            "alsa:samplerate",
            "alsa:samplerate_best",
            "alsa:lavrate",
            "alsa:lavrate_high",
            "alsa:speex",
            "alsa:upmix",
            "alsa:vdownmix",
            "alsa:a52:CARD=PCH",
            "jack",
        ] {
            assert_eq!(
                sondes_pour(endpoint),
                0,
                "{endpoint} : greffon logiciel, l'ouvrir charge une bibliothèque tierce dans \
                 le processus (#7263)"
            );
        }
    }

    #[test]
    fn les_vraies_sorties_sont_toujours_sondees_7263() {
        // Le piège : une garde trop large ferait perdre les capacités mesurées
        // des cartes et des DAC USB.
        for endpoint in [
            "alsa:hw:CARD=0,DEV=0",
            "alsa:plughw:CARD=0,DEV=0",
            "alsa:hw:CARD=A6,DEV=0",
            "alsa:default",
            "alsa:sysdefault:CARD=PCH",
            "alsa:front:CARD=PCH,DEV=0",
            "alsa:dmix:CARD=PCH,DEV=0",
            "alsa:iec958:CARD=PCH,DEV=0",
            "alsa:pipewire",
            "alsa:pulse",
            "Wasapi:{0.0.0.00000000}.{a1b2}",
            "CoreAudio:jack",
        ] {
            assert_eq!(
                sondes_pour(endpoint),
                1,
                "{endpoint} mène à du matériel (ou n'est pas ALSA) : il doit être sondé"
            );
        }
    }
}
