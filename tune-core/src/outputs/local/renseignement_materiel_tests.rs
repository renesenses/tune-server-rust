use super::{
    AudioDevice, hardware_detail, hardware_detail_from_description, merge_linux_duplicate_variant,
};
use cpal::{DeviceDescription, DeviceDescriptionBuilder};

/// Ce que cpal rend sur WASAPI : `name` vient de
/// `DEVPKEY_Device_DeviceDesc` (générique — « Haut-Parleurs »), `driver` de
/// `DEVPKEY_DeviceInterface_FriendlyName` (le contrôleur).
fn description_wasapi(nom: &str, controleur: &str) -> DeviceDescription {
    DeviceDescriptionBuilder::new(nom)
        .driver(controleur)
        .build()
}

/// Le périphérique tel que l'énumération le publie, renseignement compris.
/// `raw_name` est le nom du pilote ; `display_name` celui qu'a posé la
/// désambiguïsation — c'est bien le BRUT qui nourrit la règle.
fn peripherique_publie(
    raw_name: &str,
    display_name: &str,
    endpoint_id: &str,
    description: &DeviceDescription,
) -> AudioDevice {
    AudioDevice {
        name: display_name.to_string(),
        endpoint_id: endpoint_id.to_string(),
        is_default: false,
        max_channels: 2,
        sample_rates: vec![44_100, 48_000],
        sample_rates_measured: false,
        backend: "Wasapi".to_string(),
        hardware_detail: hardware_detail_from_description(description, raw_name, endpoint_id),
    }
}

/// L'ÉPREUVE. Deux homonymes de contrôleurs différents doivent être
/// distinguables dans ce que le serveur PUBLIE — pas seulement dans ce que
/// la règle rend.
#[test]
fn deux_homonymes_de_controleurs_differents_sont_distinguables() {
    let topping = description_wasapi("Haut-Parleurs", "Topping D10s");
    let realtek = description_wasapi("Haut-Parleurs", "Realtek High Definition Audio");

    // Le second reçoit le suffixe « (2) » de la découverte : c'est
    // exactement l'écran de Marco Polo.
    let un = peripherique_publie(
        "Haut-Parleurs",
        "Haut-Parleurs",
        "Wasapi:{0.0.0.00000000}.{aaaaaaaa}",
        &topping,
    );
    let deux = peripherique_publie(
        "Haut-Parleurs",
        "Haut-Parleurs (2)",
        "Wasapi:{0.0.0.00000000}.{bbbbbbbb}",
        &realtek,
    );

    let charge_un = serde_json::to_value(&un).expect("AudioDevice sérialisable");
    let charge_deux = serde_json::to_value(&deux).expect("AudioDevice sérialisable");

    assert_eq!(
        charge_un["hardware_detail"],
        serde_json::json!("Topping D10s"),
        "la charge utile doit nommer le contrôleur du premier DAC. Reçu : {charge_un}"
    );
    assert_eq!(
        charge_deux["hardware_detail"],
        serde_json::json!("Realtek High Definition Audio"),
        "la charge utile doit nommer le contrôleur du second. Reçu : {charge_deux}"
    );
    assert_ne!(
        charge_un["hardware_detail"], charge_deux["hardware_detail"],
        "deux périphériques homonymes doivent être DISTINGUABLES dans ce \
         que le serveur publie : c'est toute la demande de #2272."
    );

    // Et le nom d'affichage n'a pas bougé d'un caractère : c'est lui que
    // les zones ont mémorisé et que `resolve_device` reconstruit (#3185).
    assert_eq!(charge_un["name"], serde_json::json!("Haut-Parleurs"));
    assert_eq!(charge_deux["name"], serde_json::json!("Haut-Parleurs (2)"));
}

/// Un renseignement absent ne casse rien, et ne publie AUCUN champ vide.
/// C'est le cas de macOS aujourd'hui : cpal 0.17.3 n'y renseigne ni
/// `manufacturer` ni `driver`.
#[test]
fn sans_renseignement_le_champ_est_absent_de_la_charge_utile() {
    let macos = DeviceDescriptionBuilder::new("USB Audio DAC").build();
    let publie = peripherique_publie(
        "USB Audio DAC",
        "USB Audio DAC",
        "CoreAudio:AppleUSBAudioEngine:1,2",
        &macos,
    );

    assert_eq!(publie.hardware_detail, None);
    let charge = serde_json::to_value(&publie).expect("AudioDevice sérialisable");
    assert!(
        charge.get("hardware_detail").is_none(),
        "un renseignement absent doit être ABSENT de la charge utile, pas \
         publié comme `null` ou comme chaîne vide : sinon l'écran affiche \
         un tiret et le vide se fait passer pour une information. \
         Reçu : {charge}"
    );
    // Le reste de la charge utile est intacte.
    assert_eq!(charge["name"], serde_json::json!("USB Audio DAC"));
    assert_eq!(
        charge["endpoint_id"],
        serde_json::json!("CoreAudio:AppleUSBAudioEngine:1,2")
    );
}

/// Sur ALSA, cpal met le PCM dans `driver` — la même chaîne que
/// `endpoint_id` porte déjà, préfixe d'hôte en plus. Ce n'est pas un nom de
/// contrôleur, et le publier comme tel serait du bruit.
#[test]
fn le_pcm_alsa_ne_se_fait_pas_passer_pour_un_controleur() {
    let alsa = DeviceDescriptionBuilder::new("HDA Intel PCH, ALC257 Analog")
        .driver("hw:CARD=PCH,DEV=0")
        .build();
    assert_eq!(
        hardware_detail_from_description(
            &alsa,
            "HDA Intel PCH, ALC257 Analog",
            "Alsa:hw:CARD=PCH,DEV=0"
        ),
        None,
        "le PCM ALSA est déjà l'identifiant d'endpoint : il ne distingue \
         rien de plus, et Linux doit retomber mot pour mot sur le \
         comportement d'avant"
    );
}

#[test]
fn le_fabricant_l_emporte_sur_le_pilote() {
    assert_eq!(
        hardware_detail(
            Some("Topping"),
            Some("USB Audio Device"),
            "Haut-Parleurs",
            "Wasapi:{0.0.0.00000000}.{aaaaaaaa}"
        ),
        Some("Topping".to_string()),
        "aucun backend de cpal 0.17.3 ne renseigne `manufacturer`, mais \
         c'est le champ dont la sémantique est la bonne : le jour où l'un \
         le remplit, il doit gagner sans qu'on y revienne"
    );
}

#[test]
fn un_candidat_que_le_nom_porte_deja_n_ajoute_rien() {
    assert_eq!(
        hardware_detail(None, Some("Topping D10s"), "Topping D10s", "Wasapi:{aaaa}"),
        None
    );
    assert_eq!(
        hardware_detail(None, Some("topping d10s"), "Topping D10s", "Wasapi:{aaaa}"),
        None,
        "la comparaison ignore la casse : WASAPI et le pilote n'ont pas \
         toujours la même"
    );
}

#[test]
fn un_renseignement_blanc_n_est_pas_un_renseignement() {
    assert_eq!(
        hardware_detail(Some("   "), Some(""), "Haut-Parleurs", "Wasapi:{aaaa}"),
        None
    );
    assert_eq!(
        hardware_detail(
            Some("  Topping D10s  "),
            None,
            "Haut-Parleurs",
            "Wasapi:{aaaa}"
        ),
        Some("Topping D10s".to_string()),
        "un renseignement encadré de blancs reste un renseignement, mais \
         il est publié propre"
    );
}

/// LE TÉMOIN. Un périphérique unique dont le nom porte déjà son identité
/// ne change de rien : ni de nom, ni de champ inventé.
#[test]
fn temoin_un_peripherique_unique_ne_change_de_rien() {
    let seul = description_wasapi("Topping D10s", "Topping D10s");
    let publie = peripherique_publie(
        "Topping D10s",
        "Topping D10s",
        "Wasapi:{0.0.0.00000000}.{cccccccc}",
        &seul,
    );
    assert_eq!(
        publie.name, "Topping D10s",
        "le nom d'affichage ne bouge pas : c'est lui que les zones ont \
         mémorisé (#3185)"
    );
    assert_eq!(
        publie.hardware_detail, None,
        "quand le nom porte déjà l'identité, il n'y a rien à ajouter — et \
         surtout pas « Topping D10s — Topping D10s »"
    );
    let charge = serde_json::to_value(&publie).expect("AudioDevice sérialisable");
    assert!(charge.get("hardware_detail").is_none());
}

/// LE TÉMOIN, suite : le dédoublonnage Linux ne change pas de conduite.
/// Quand le PCM matériel supplante le greffon (#3240/#1655), l'identité
/// bascule ENTIÈREMENT — endpoint, capacités, preuve, et désormais le
/// renseignement matériel. Le laisser en arrière l'accrocherait au greffon
/// qu'on vient d'écarter.
#[test]
fn l_identite_qui_bascule_emporte_le_renseignement_materiel() {
    let mut publie = AudioDevice {
        name: "Eversolo DAC-Z8, USB Audio".into(),
        endpoint_id: "Alsa:sysdefault:CARD=DACZ8,DEV=0".into(),
        is_default: false,
        max_channels: 32,
        sample_rates: vec![44_100, 384_000],
        sample_rates_measured: false,
        backend: "Alsa".into(),
        hardware_detail: Some("greffon".into()),
    };
    assert!(
        merge_linux_duplicate_variant(
            &mut publie,
            "Alsa:hw:CARD=DACZ8,DEV=0".into(),
            false,
            2,
            vec![44_100, 48_000],
            true,
            Some("Eversolo DAC-Z8".into()),
        ),
        "le PCM matériel doit l'emporter sur le greffon (#3240)"
    );
    assert_eq!(publie.endpoint_id, "Alsa:hw:CARD=DACZ8,DEV=0");
    assert_eq!(
        publie.hardware_detail.as_deref(),
        Some("Eversolo DAC-Z8"),
        "le renseignement matériel est le quatrième membre de l'identité \
         qui bascule : il doit suivre l'endpoint retenu"
    );

    // Et dans l'autre sens : le greffon qui arrive après ne reprend rien au
    // matériel déjà retenu, renseignement compris.
    let mut publie = AudioDevice {
        name: "Eversolo DAC-Z8, USB Audio".into(),
        endpoint_id: "Alsa:hw:CARD=DACZ8,DEV=0".into(),
        is_default: false,
        max_channels: 2,
        sample_rates: vec![44_100, 48_000],
        sample_rates_measured: true,
        backend: "Alsa".into(),
        hardware_detail: Some("Eversolo DAC-Z8".into()),
    };
    assert!(!merge_linux_duplicate_variant(
        &mut publie,
        "Alsa:dmix:CARD=DACZ8,DEV=0".into(),
        true,
        32,
        vec![44_100, 384_000],
        false,
        Some("greffon".into()),
    ));
    assert_eq!(publie.endpoint_id, "Alsa:hw:CARD=DACZ8,DEV=0");
    assert_eq!(publie.hardware_detail.as_deref(), Some("Eversolo DAC-Z8"));
}

/// Un enregistrement écrit avant ce champ reste lisible, et ne se voit pas
/// attribuer un renseignement qu'il n'a pas.
#[test]
fn un_enregistrement_anterieur_reste_lisible_sans_renseignement() {
    let ancien = serde_json::json!({
        "name": "Haut-Parleurs",
        "endpoint_id": "",
        "is_default": true,
        "max_channels": 2,
        "sample_rates": [44_100, 48_000],
        "backend": "Wasapi",
    });
    let relu: AudioDevice =
        serde_json::from_value(ancien).expect("AudioDevice reste rétro-compatible");
    assert_eq!(relu.hardware_detail, None);
    assert_eq!(relu.name, "Haut-Parleurs");
}
