use super::{PatchZone, fixed_volume_confirmation_required};
use tune_core::db::zone_repo::Zone;

fn zone(output_type: Option<&str>, fixed_volume: bool) -> Zone {
    Zone {
        id: Some(7),
        name: "Salon".into(),
        output_type: output_type.map(str::to_string),
        output_device_id: Some("renderer-1".into()),
        volume: 37.0,
        muted: false,
        online: true,
        gapless_enabled: false,
        group_id: None,
        sync_delay_ms: 0,
        last_position_ms: 0,
        last_track_id: None,
        last_track_source: None,
        last_track_source_id: None,
        max_sample_rate: None,
        fixed_volume,
        autoplay_enabled: false,
    }
}

/// #2271 — le nouveau champ de mode se deserialise, et l'ancien booleen
/// continue de se deserialiser seul. Les deux ensemble sont acceptes au
/// niveau serde ; c'est le handler qui tranche la precedence.
#[test]
fn autoplay_mode_se_deserialise() {
    let b: PatchZone = serde_json::from_str(r#"{"autoplay_mode":"similar"}"#).unwrap();
    assert_eq!(b.autoplay_mode.as_deref(), Some("similar"));
    assert_eq!(b.autoplay_enabled, None, "champ absent, pas `false`");

    let b: PatchZone = serde_json::from_str(r#"{"autoplay_enabled":true}"#).unwrap();
    assert_eq!(b.autoplay_enabled, Some(true));
    assert_eq!(
        b.autoplay_mode, None,
        "un client qui ne connait que le booleen n'envoie pas de mode"
    );

    let b: PatchZone = serde_json::from_str(r#"{}"#).unwrap();
    assert_eq!(b.autoplay_mode, None);
    assert_eq!(b.autoplay_enabled, None);
}

// #1320 (Cyrille) — « Aucune » ne persistait jamais : un `null` explicite
// sur `max_sample_rate` se désérialisait en `None` extérieur, donc le
// handler le confondait avec un champ absent et n'effaçait rien. Ces
// trois états sont le contrat du PATCH ; le premier test échoue contre
// le code d'avant (sans `deserialize_with = "double_option"`).

#[test]
fn explicit_null_means_clear_the_cap() {
    let p: PatchZone = serde_json::from_str(r#"{"max_sample_rate": null}"#).unwrap();
    assert_eq!(
        p.max_sample_rate,
        Some(None),
        "un null explicite doit demander l'effacement, pas être ignoré"
    );
}

#[test]
fn absent_field_means_leave_untouched() {
    let p: PatchZone = serde_json::from_str(r#"{"name": "Salon"}"#).unwrap();
    assert_eq!(p.max_sample_rate, None);
}

#[test]
fn value_means_set_the_cap() {
    let p: PatchZone = serde_json::from_str(r#"{"max_sample_rate": 705600}"#).unwrap();
    assert_eq!(p.max_sample_rate, Some(Some(705_600)));
}

/// #2395 — AUCUN type de sortie n'est dispensé de l'accord.
///
/// `local` et `browser` l'étaient jusqu'ici. La garde protège le niveau qui
/// sort des haut-parleurs, pas l'identité de ce qu'on commande : une zone
/// locale à 20 % monte bien à pleine échelle (`LocalOutput::set_volume` est
/// un vrai gain), et une zone `browser` — souvent un casque sur un portable
/// — voit son niveau appliqué par le client web à partir du volume de zone,
/// celui que l'armement met à 100.
///
/// Le `None` et le type inconnu sont dans la liste pour ce qu'ils prouvent :
/// la garde ne classe plus rien, donc elle ne peut plus se tromper de
/// classement.
#[test]
fn aucune_sortie_ne_s_arme_sans_accord() {
    let p: PatchZone = serde_json::from_str(r#"{"fixed_volume": true}"#).unwrap();
    for stored_type in [
        Some("dlna"),
        Some("airplay"),
        Some("chromecast"),
        Some("local"),
        Some("browser"),
        Some("un-type-que-personne-ne-connait"),
        None,
    ] {
        assert!(
            fixed_volume_confirmation_required(&zone(stored_type, false), &p),
            "{stored_type:?} : armer le volume fixe monte la zone a pleine echelle, \
             l'accord explicite est du quel que soit le type de sortie"
        );
    }
}

/// L'accord donné, l'armement passe — sur n'importe quelle sortie.
///
/// L'autre bord du test précédent : la garde exige un accord, elle ne
/// bloque pas le mode. Sans ce cas, un `return true` inconditionnel
/// passerait pour un correctif.
#[test]
fn l_accord_explicite_autorise_l_armement_sur_toute_sortie() {
    let p: PatchZone =
        serde_json::from_str(r#"{"fixed_volume": true, "confirm_full_volume": true}"#).unwrap();
    for stored_type in [
        Some("dlna"),
        Some("airplay"),
        Some("local"),
        Some("browser"),
        None,
    ] {
        assert!(
            !fixed_volume_confirmation_required(&zone(stored_type, false), &p),
            "{stored_type:?} : l'accord donne, l'armement doit passer"
        );
    }
}

/// Changer de type de sortie dans le PATCH qui arme ne change rien.
///
/// Ce cas gardait autrefois la précédence du type envoyé sur le type
/// stocké — une zone locale basculée en AirPlay ne devait pas profiter de
/// l'exemption. Il n'y a plus d'exemption ni de lecture du type, donc plus
/// de précédence à tenir ; le cas reste, comme garde de non-régression :
/// aucune combinaison de types, dans un sens ou dans l'autre, ne doit
/// rouvrir un chemin d'armement sans accord.
#[test]
fn un_changement_de_type_dans_le_meme_patch_reste_protege() {
    for (stocke, demande) in [
        (Some("local"), "airplay"),
        (Some("dlna"), "local"),
        (Some("browser"), "dlna"),
        (Some("airplay"), "browser"),
    ] {
        let p: PatchZone = serde_json::from_str(&format!(
            r#"{{"output_type": "{demande}", "fixed_volume": true}}"#
        ))
        .unwrap();
        assert!(
            fixed_volume_confirmation_required(&zone(stocke, false), &p),
            "{stocke:?} -> {demande} : toujours un accord"
        );
    }
}

/// Ce qui ne monte PAS le volume passe sans rien demander.
///
/// Le contre-poids des deux premiers : la garde ne se déclenche que sur la
/// transition qui monte réellement à pleine échelle. Une zone déjà armée
/// qu'on réaffirme ne monte rien — le saut a eu lieu — et un désarmement
/// fait redescendre. Sans ces cas, exiger l'accord partout se confondrait
/// avec l'exiger tout le temps.
///
/// La liste ne contient plus `local` ni `browser` : ces deux chemins
/// montent bel et bien la zone à 100 %, et ils sont désormais éprouvés dans
/// `aucune_sortie_ne_s_arme_sans_accord`. L'ancien nom de cet essai
/// affirmait qu'ils « ne montent pas le volume » ; c'était faux.
#[test]
fn ce_qui_ne_monte_pas_le_volume_passe_sans_accord() {
    for (stored_type, stored_fixed, payload) in [
        // Déjà armée : le PATCH réaffirme, il ne monte rien.
        (Some("dlna"), true, r#"{"fixed_volume": true}"#),
        (Some("local"), true, r#"{"fixed_volume": true}"#),
        (Some("browser"), true, r#"{"fixed_volume": true}"#),
        // Désarmement : on redescend.
        (Some("dlna"), true, r#"{"fixed_volume": false}"#),
        (Some("local"), true, r#"{"fixed_volume": false}"#),
        // Le PATCH ne parle pas de volume fixe du tout.
        (Some("dlna"), false, r#"{"name": "Salon"}"#),
    ] {
        let p: PatchZone = serde_json::from_str(payload).unwrap();
        assert!(
            !fixed_volume_confirmation_required(&zone(stored_type, stored_fixed), &p),
            "le chemin {stored_type:?}/{stored_fixed}/{payload} ne monte aucune zone \
             a pleine echelle : rien a confirmer"
        );
    }
}
