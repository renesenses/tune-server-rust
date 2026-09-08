//! Préconfigurer un appareil reconnu — **sans jamais écraser un réglage posé
//! à la main** (#3589, volet B).
//!
//! ## Deux sources, un ordre : les quirks d'abord
//!
//! Arbitrage de Bertrand du 08/09/2026 : les deux leviers s'appliquent, en
//! commençant par les **quirks** du catalogue embarqué, puis les **réglages de
//! zone** du catalogue « Tune tested » téléchargé.
//!
//! « En commençant par » se lit ici comme une **priorité**, pas comme un simple
//! ordre d'écriture : sur une clé que les deux sources nomment, c'est le quirk
//! qui est retenu, et le réglage validé ne fait que compléter ce que les quirks
//! ne couvrent pas. Écrire puis réécrire la même clé aurait fait de l'ordre un
//! détail sans effet, et rendu la trace illisible : deux écritures pour une
//! valeur, la seconde effaçant la première.
//!
//! ## 🔴 Aucune traduction inventée
//!
//! Les deux vocabulaires ne se recouvrent que sur trois noms
//! (`dlna_native_flac`, `dlna_wav24`, `dlna_play_delay_ms`), plus deux
//! correspondances **déjà établies dans le dépôt**, pas inventées ici :
//! `force_16bit` → `dlna_cap_16bit` et `max_sample_rate` combiné en `min`
//! (`device_catalog.rs`, section « Quirks câblés vs framework »).
//!
//! Les quirks `force_mime`, `no_gapless`, `pcm_only` et
//! `dlna_no_extra_headers` n'ont **aucun** réglage de zone correspondant. Ils
//! ne sont donc pas préconfigurés : leur donner une destination supposerait une
//! équivalence que personne n'a établie.
//!
//! ## Ce que « posé à la main » veut dire ici
//!
//! Ce module ne lit pas la base : il reçoit un prédicat `deja_pose`. La façon
//! dont l'appelant répond à ce prédicat est le vrai sujet, et elle est
//! documentée là où elle vit (`tune-server/src/routes/zones/preconfiguration.rs`).
//! Ici, la règle est absolue : **une clé pour laquelle `deja_pose` rend `true`
//! ne produit aucune proposition**, quelle que soit sa source.

use crate::cloud::tune_tested::AppareilValide;
use crate::device_catalog::DeviceQuirks;

/// La valeur d'une proposition, dans le type de la colonne visée.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Valeur {
    Drapeau(bool),
    /// `dlna_play_delay_ms`, en millisecondes.
    Duree(u64),
    /// `max_sample_rate`, en Hz.
    Frequence(u32),
    /// `gain_trim_db`, en dB. Peut arriver entier du JSON — voir
    /// [`AppareilValide::nombre`].
    Trim(f64),
}

/// D'où vient la proposition. Sert la trace : quand une préconfiguration
/// surprend un utilisateur, il faut pouvoir dire laquelle des deux sources l'a
/// produite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origine {
    /// Catalogue embarqué (`device_catalog.json`), quirks du modèle.
    Quirks,
    /// Catalogue « Tune tested » téléchargé depuis mozaiklabs.
    TuneTested,
}

/// Un réglage que la préconfiguration propose de poser.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposition {
    pub cle: &'static str,
    pub valeur: Valeur,
    pub origine: Origine,
}

/// Les seules clés que la préconfiguration sait poser.
///
/// La liste est **fermée à dessein** : un nom que le site ajouterait demain
/// dans `settings` ne doit pas devenir une écriture par accident. Un nom
/// inconnu est ignoré, et la trace le dit.
pub const CLES_CONNUES: [&str; 9] = [
    "dlna_native_flac",
    "alac_passthrough",
    "aac_passthrough",
    "dlna_lpcm",
    "dlna_cap_16bit",
    "dlna_wav24",
    "dlna_play_delay_ms",
    "gain_trim_db",
    "upnp_silence",
];

/// Les clés du vocabulaire `tune.renderer.v1` qui sont des booléens.
const DRAPEAUX: [&str; 6] = [
    "dlna_native_flac",
    "alac_passthrough",
    "aac_passthrough",
    "dlna_lpcm",
    "dlna_cap_16bit",
    "dlna_wav24",
];

/// Ce que la préconfiguration propose pour un appareil.
///
/// `quirks` vient du catalogue embarqué, `valide` du catalogue téléchargé
/// (`None` si l'appareil n'est pas validé, ou si rien n'a pu être téléchargé —
/// c'est le repli hors ligne). `deja_pose` répond « ce réglage a-t-il été posé
/// à la main ? » ; une clé pour laquelle il rend `true` est écartée.
///
/// L'ordre du vecteur rendu est celui de l'application : quirks d'abord.
pub fn preconfigurer(
    quirks: &DeviceQuirks,
    valide: Option<&AppareilValide>,
    deja_pose: &dyn Fn(&str) -> bool,
) -> Vec<Proposition> {
    let mut out: Vec<Proposition> = Vec::new();
    let mut pousser = |cle: &'static str, valeur: Valeur, origine: Origine| {
        if deja_pose(cle) {
            return;
        }
        if out.iter().any(|p| p.cle == cle) {
            // Les quirks sont passés avant : ils gardent la clé.
            return;
        }
        out.push(Proposition {
            cle,
            valeur,
            origine,
        });
    };

    // --- 1. Les quirks du catalogue embarqué. -------------------------------
    //
    // Un quirk booléen à `false` est la valeur NEUTRE de `DeviceQuirks` : il ne
    // dit rien, et ne doit donc rien poser. Seul un `true` est une affirmation.
    if quirks.dlna_native_flac {
        pousser("dlna_native_flac", Valeur::Drapeau(true), Origine::Quirks);
    }
    if quirks.dlna_wav24 {
        pousser("dlna_wav24", Valeur::Drapeau(true), Origine::Quirks);
    }
    if quirks.force_16bit {
        // Correspondance déjà établie par `device_catalog.rs` — pas inventée.
        pousser("dlna_cap_16bit", Valeur::Drapeau(true), Origine::Quirks);
    }
    if let Some(ms) = quirks.dlna_play_delay_ms {
        pousser("dlna_play_delay_ms", Valeur::Duree(ms), Origine::Quirks);
    }
    if let Some(hz) = quirks.max_sample_rate {
        pousser("max_sample_rate", Valeur::Frequence(hz), Origine::Quirks);
    }

    // --- 2. Les réglages validés, en `tune.renderer.v1`. --------------------
    if let Some(dev) = valide {
        for cle in DRAPEAUX {
            if let Some(v) = dev.drapeau(cle) {
                pousser(cle, Valeur::Drapeau(v), Origine::TuneTested);
            }
        }
        if let Some(v) = dev.drapeau("upnp_silence") {
            pousser("upnp_silence", Valeur::Drapeau(v), Origine::TuneTested);
        }
        if let Some(ms) = dev.nombre("dlna_play_delay_ms") {
            if ms >= 0.0 {
                pousser(
                    "dlna_play_delay_ms",
                    Valeur::Duree(ms as u64),
                    Origine::TuneTested,
                );
            }
        }
        // 🔴 `-3` arrive entier : `nombre` le lit, un `f64` strict l'aurait
        // rejeté. Même borne que le PATCH de zone (±12 dB).
        if let Some(db) = dev.nombre("gain_trim_db") {
            pousser(
                "gain_trim_db",
                Valeur::Trim(db.clamp(-12.0, 12.0)),
                Origine::TuneTested,
            );
        }
    }

    out
}

/// Les noms de `settings` que le site a servis et que ce serveur ne sait pas
/// poser. Sert la trace, pas le comportement : un vocabulaire qui s'enrichit
/// doit se voir dans le journal, pas se deviner.
pub fn cles_ignorees(valide: &AppareilValide) -> Vec<String> {
    valide
        .settings
        .keys()
        .filter(|k| !CLES_CONNUES.contains(&k.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valide(settings: &str) -> AppareilValide {
        serde_json::from_str(&format!(
            r#"{{"brand":"Eversolo","model":"DMP-A8","output_type":"dlna","settings":{settings}}}"#
        ))
        .expect("appareil de test lisible")
    }

    fn rien(_: &str) -> bool {
        false
    }

    /// Le cœur du volet B : un réglage posé à la main n'est JAMAIS proposé,
    /// quelle que soit la source qui le nomme.
    #[test]
    fn un_reglage_pose_a_la_main_n_est_jamais_propose() {
        let quirks = DeviceQuirks {
            dlna_native_flac: true,
            force_16bit: true,
            ..Default::default()
        };
        let dev = valide(r#"{"dlna_native_flac":false,"gain_trim_db":-3,"alac_passthrough":true}"#);

        // La main a posé `dlna_native_flac` (source quirks) ET `gain_trim_db`
        // (source Tune tested).
        let pose = |c: &str| matches!(c, "dlna_native_flac" | "gain_trim_db");
        let props = preconfigurer(&quirks, Some(&dev), &pose);
        let cles: Vec<&str> = props.iter().map(|p| p.cle).collect();

        assert!(
            !cles.contains(&"dlna_native_flac"),
            "un réglage posé à la main a été proposé quand même : {cles:?}"
        );
        assert!(
            !cles.contains(&"gain_trim_db"),
            "un réglage posé à la main a été proposé quand même : {cles:?}"
        );
        // Le reste passe : la garde protège, elle ne stérilise pas.
        assert!(cles.contains(&"dlna_cap_16bit"));
        assert!(cles.contains(&"alac_passthrough"));
    }

    /// Quirks d'abord : sur une clé nommée par les deux, c'est le quirk qui
    /// gagne, et il n'y a QU'UNE proposition — pas une écriture puis sa
    /// réécriture.
    #[test]
    fn sur_une_cle_partagee_le_quirk_gagne_et_ne_s_ecrit_qu_une_fois() {
        let quirks = DeviceQuirks {
            dlna_native_flac: true,
            ..Default::default()
        };
        let dev = valide(r#"{"dlna_native_flac":false}"#);
        let props = preconfigurer(&quirks, Some(&dev), &rien);

        let sur_la_cle: Vec<&Proposition> = props
            .iter()
            .filter(|p| p.cle == "dlna_native_flac")
            .collect();
        assert_eq!(
            sur_la_cle.len(),
            1,
            "une clé partagée doit produire UNE proposition, pas deux : {props:?}"
        );
        assert_eq!(sur_la_cle[0].origine, Origine::Quirks);
        assert_eq!(sur_la_cle[0].valeur, Valeur::Drapeau(true));
    }

    /// 🔴 Le trim rond, de bout en bout : `-3` entier dans le JSON du site
    /// ressort en `Trim(-3.0)`.
    #[test]
    fn le_trim_rond_traverse_la_preconfiguration() {
        let dev = valide(r#"{"gain_trim_db":-3}"#);
        assert!(
            dev.settings["gain_trim_db"].is_i64(),
            "le JSON de test doit porter un ENTIER, sinon ce test ne garde rien"
        );
        let props = preconfigurer(&DeviceQuirks::default(), Some(&dev), &rien);
        assert_eq!(
            props,
            vec![Proposition {
                cle: "gain_trim_db",
                valeur: Valeur::Trim(-3.0),
                origine: Origine::TuneTested,
            }]
        );
    }

    /// Hors ligne : pas de catalogue téléchargé, seuls les quirks embarqués
    /// jouent — exactement ce que l'instance faisait déjà.
    #[test]
    fn hors_ligne_seuls_les_quirks_embarques_proposent() {
        let quirks = DeviceQuirks {
            max_sample_rate: Some(48_000),
            force_16bit: true,
            ..Default::default()
        };
        let props = preconfigurer(&quirks, None, &rien);
        assert_eq!(
            props,
            vec![
                Proposition {
                    cle: "dlna_cap_16bit",
                    valeur: Valeur::Drapeau(true),
                    origine: Origine::Quirks,
                },
                Proposition {
                    cle: "max_sample_rate",
                    valeur: Valeur::Frequence(48_000),
                    origine: Origine::Quirks,
                },
            ]
        );
    }

    /// Un profil de quirks neutre ne pose RIEN : `false` est la valeur par
    /// défaut de la structure, pas une affirmation du catalogue.
    #[test]
    fn un_profil_neutre_ne_pose_rien() {
        assert!(preconfigurer(&DeviceQuirks::default(), None, &rien).is_empty());
    }

    /// Les quirks sans réglage de zone correspondant ne se traduisent pas.
    #[test]
    fn les_quirks_sans_correspondance_ne_s_inventent_pas_de_destination() {
        let quirks = DeviceQuirks {
            force_mime: Some("audio/x-flac".into()),
            no_gapless: true,
            pcm_only: true,
            dlna_no_extra_headers: true,
            ..Default::default()
        };
        assert!(
            preconfigurer(&quirks, None, &rien).is_empty(),
            "aucune traduction ne doit être inventée pour ces quatre quirks"
        );
    }

    /// Un nom que le serveur ne sait pas poser est ignoré, et se voit.
    #[test]
    fn un_nom_inconnu_est_ignore_et_se_dit() {
        let dev = valide(r#"{"dlna_native_flac":true,"reglage_de_demain":42}"#);
        let props = preconfigurer(&DeviceQuirks::default(), Some(&dev), &rien);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].cle, "dlna_native_flac");
        assert_eq!(cles_ignorees(&dev), vec!["reglage_de_demain".to_string()]);
    }

    /// Le trim validé reste dans la borne du PATCH de zone.
    #[test]
    fn le_trim_valide_est_borne_comme_le_patch_de_zone() {
        let dev = valide(r#"{"gain_trim_db":-40}"#);
        let props = preconfigurer(&DeviceQuirks::default(), Some(&dev), &rien);
        assert_eq!(props[0].valeur, Valeur::Trim(-12.0));
    }
}
