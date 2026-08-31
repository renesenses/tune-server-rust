//! Classification des réglages qui ne doivent jamais sortir en clair (#2793).
//!
//! La table `settings` est un fourre-tout : elle porte le thème de l'interface
//! à côté du secret de signature JWT, de la graine Ed25519 d'un appairage
//! AirPlay et des clés `tunedev_` de l'API développeur. Les routes qui
//! publient cette table entretenaient chacune leur propre liste de retraits —
//! `/system/config` en enlevait deux et masquait trois sous-champs Qobuz,
//! `/system/config/export` en enlevait trois. Deux listes partielles, deux
//! occasions de rater la clé suivante : ni la graine AirPlay ni les clés
//! développeur n'y figuraient, et elles sortaient donc en clair.
//!
//! Ce module est la SEULE liste. Elle ne nomme pas les clés une par une : elle
//! décrit ce qui, dans un NOM de réglage, désigne une valeur secrète. Une clé
//! ajoutée demain sous un nom qui contient `token`, `secret` ou `password` est
//! couverte sans que personne ait à y penser — c'est le critère
//! d'acceptation « y compris celles ajoutées ultérieurement ».
//!
//! Le prix de cette règle est le faux positif : un nom peut contenir un
//! fragment sensible sans porter de secret. Ces cas sont nommés un par un dans
//! [`EXCEPTIONS`], avec leur lecteur, parce que masquer une valeur que
//! l'interface lit casse un écran sans aucun message d'erreur.

use serde_json::Value;

/// Ce qui remplace une valeur secrète. Même marqueur que celui déjà utilisé
/// pour les sous-champs Qobuz, pour ne pas inventer un second dialecte.
pub const MASQUE: &str = "********";

/// Fragments qui, présents dans un nom de réglage, désignent un secret.
///
/// Comparaison en minuscules, sur le nom entier : `auth_tokens_qobuz` porte
/// `token`, `airplay2_pairing:<id>` ne porte rien mais son objet contient
/// `our_ed25519_seed_hex`, qui porte `seed` — d'où la descente récursive de
/// [`caviarder`].
const FRAGMENTS_SECRETS: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "passphrase",
    "token",
    "api_key",
    "apikey",
    "private_key",
    "signing_key",
    "session_key",
    "credential",
    "seed",
    "fingerprint",
    "pkce",
];

/// Noms exacts qui sont secrets sans porter aucun fragment ci-dessus.
const NOMS_SECRETS: &[&str] = &[
    // La clé de licence : un porteur suffit à activer une autre machine.
    "license_key",
    // Les clés `tunedev_` de l'API développeur, en clair dans un tableau JSON
    // (`tune-server/src/routes/developer_api.rs:19`). L'écran Développeur les
    // lit sur `/developer/api-keys`, qui les tronque ; personne ne les lit ici.
    "developer_api_keys",
    // Les webhooks portent l'URL de rappel, souvent avec son jeton dans le
    // chemin (`developer_api.rs:20`).
    "developer_webhooks",
    // Le « nom d'utilisateur » du pont Philips Hue EST le porteur d'accès.
    "hue_username",
];

/// Noms exacts qui portent un fragment sensible sans être des secrets.
///
/// Chaque ligne cite son lecteur : ce sont les clés dont le masquage casserait
/// un écran en silence.
const EXCEPTIONS: &[&str] = &[
    // Booléen DÉRIVÉ, calculé par `get_config` à partir de `discogs_token`.
    // Lu par l'interface : SettingsView.svelte:2948 et :4799 (badge
    // « Discogs configuré »), déclaré types.ts:551.
    "discogs_token_set",
    // Déjà masquée par `get_config` : seuls les quatre derniers caractères
    // restent. La masquer une seconde fois n'ajouterait rien et effacerait
    // l'indice que l'écran affiche.
    "license_key_masked",
];

/// Ce nom de réglage désigne-t-il une valeur qui ne doit jamais sortir ?
///
/// Le test porte sur le NOM, jamais sur la valeur : une valeur peut être vide
/// aujourd'hui et renseignée demain, et une règle qui ne masque que le
/// non-vide révèle déjà si le secret est posé.
pub fn est_secret(cle: &str) -> bool {
    let cle_min = cle.to_ascii_lowercase();
    if EXCEPTIONS.contains(&cle_min.as_str()) {
        return false;
    }
    if NOMS_SECRETS.contains(&cle_min.as_str()) {
        return true;
    }
    FRAGMENTS_SECRETS.iter().any(|f| cle_min.contains(f))
}

/// Masque, en place, toute valeur secrète d'une carte de réglages.
///
/// Descend dans les objets et les tableaux : un secret imbriqué est aussi lisible
/// qu'un secret de premier niveau. C'est ce qui attrape la graine Ed25519, qui
/// vit sous `airplay2_pairing:<id>` — un nom parfaitement anodin — dans un objet
/// dont le champ s'appelle `our_ed25519_seed_hex`.
///
/// La clé de plus haut niveau n'est pas testée par cette fonction : c'est
/// l'appelant qui itère sur ses propres entrées, parce que lui seul sait s'il
/// doit masquer ou retirer.
pub fn caviarder_valeur(valeur: &mut Value) {
    match valeur {
        Value::Object(carte) => {
            for (cle, v) in carte.iter_mut() {
                if est_secret(cle) {
                    *v = Value::String(MASQUE.to_string());
                } else {
                    caviarder_valeur(v);
                }
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                caviarder_valeur(v);
            }
        }
        _ => {}
    }
}

/// Masque toute entrée secrète d'une carte de réglages, imbrications comprises.
///
/// Pour les routes de LECTURE (`GET /system/config`) : la valeur disparaît,
/// mais le nom reste, donc l'interface sait encore que le réglage est posé —
/// exactement ce que faisait déjà le masquage des sous-champs Qobuz.
pub fn caviarder_carte(carte: &mut serde_json::Map<String, Value>) {
    for (cle, valeur) in carte.iter_mut() {
        if est_secret(cle) {
            *valeur = Value::String(MASQUE.to_string());
        } else {
            caviarder_valeur(valeur);
        }
    }
}

/// Retire toute entrée secrète d'une carte de réglages, imbrications comprises.
///
/// Pour les routes de SAUVEGARDE (`GET /system/config/export`), et pas
/// [`caviarder_carte`] : une sauvegarde se ré-importe. Un export qui
/// remplacerait `jwt_secret` par [`MASQUE`] et qu'on restaurerait sur la même
/// machine écrirait la chaîne `********` par-dessus le vrai secret de
/// signature — toutes les sessions tomberaient. L'absence de la clé, elle, est
/// exactement ce que `import_config` sait ignorer : il n'écrit que les clés
/// présentes dans le corps.
pub fn retirer_les_secrets(carte: &mut serde_json::Map<String, Value>) {
    carte.retain(|cle, _| !est_secret(cle));
    for valeur in carte.values_mut() {
        retirer_les_secrets_valeur(valeur);
    }
}

fn retirer_les_secrets_valeur(valeur: &mut Value) {
    match valeur {
        Value::Object(carte) => {
            carte.retain(|cle, _| !est_secret(cle));
            for v in carte.values_mut() {
                retirer_les_secrets_valeur(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                retirer_les_secrets_valeur(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn les_secrets_nommes_dans_l_issue_2793_sont_reconnus() {
        for cle in [
            "jwt_secret",
            "api_key",
            "license_key",
            "credentials_vault",
            "developer_api_keys",
            "developer_webhooks",
            "listenbrainz_token",
            "lastfm_session_key",
            "auth_tokens_qobuz",
            "auth_tokens_tidal",
            "discogs_token",
            "bridge_token",
            "ha_token",
            "mozaik_access_token",
            "mozaik_refresh_token",
            "mozaik_pkce_pending",
            "hardware_fingerprint",
            "hue_username",
        ] {
            assert!(est_secret(cle), "{cle} devrait etre classe secret");
        }
    }

    /// Le témoin anti-régression : ces clés-là sont servies, et le resteront.
    /// Chacune a un lecteur prouvé dans le client web (cf. #2793).
    #[test]
    fn les_reglages_legitimes_ne_sont_jamais_masques() {
        for cle in [
            "discogs_token_set",
            "license_key_masked",
            "api_port",
            "stream_port",
            "setting_keys",
            "setting_key",
            "supported_audio_backends",
            "local_audio_backend",
            "local_exclusive_mode",
            "server_name",
            "server_urls",
            "music_dirs",
            "theme",
            "ui_preferences",
            "shortcuts",
            "db_engine",
            "db_path",
            "quality_split",
            "premium_features",
            "premium_tier",
            "zone_limit",
            "replaygain_mode",
            "replaygain_analysis_enabled",
            "community_sync_enabled",
            "lyrics_lrclib_enabled",
            "audio_embedding_enabled",
            "squeezebox_enabled",
            "zone_auto_create",
        ] {
            assert!(!est_secret(cle), "{cle} ne doit PAS etre masque");
        }
    }

    #[test]
    fn la_casse_du_nom_ne_permet_pas_de_passer() {
        assert!(est_secret("JWT_SECRET"));
        assert!(est_secret("Auth_Tokens_Qobuz"));
    }

    /// La graine Ed25519 d'AirPlay vit sous un nom de clé anodin
    /// (`airplay2_pairing:<id>`) : seule la descente dans l'objet l'attrape.
    #[test]
    fn la_graine_airplay_est_masquee_sans_effacer_la_cle_publique() {
        let mut carte = serde_json::Map::new();
        carte.insert(
            "airplay2_pairing:airplay2:salon".to_string(),
            json!({
                "our_ed25519_seed_hex": "00112233445566778899aabbccddeeff",
                "accessory_ltpk_hex": "ffeeddccbbaa99887766554433221100",
                "accessory_id": "salon",
            }),
        );
        caviarder_carte(&mut carte);
        let bloc = &carte["airplay2_pairing:airplay2:salon"];
        assert_eq!(bloc["our_ed25519_seed_hex"], json!(MASQUE));
        assert_eq!(
            bloc["accessory_ltpk_hex"],
            json!("ffeeddccbbaa99887766554433221100"),
            "la cle PUBLIQUE de l'accessoire n'est pas un secret"
        );
        assert_eq!(bloc["accessory_id"], json!("salon"));
    }

    #[test]
    fn les_sous_champs_qobuz_restent_masques_comme_avant() {
        let mut carte = serde_json::Map::new();
        carte.insert(
            "auth_tokens_qobuz".to_string(),
            json!({
                "stored_password": "faux-mot-de-passe",
                "user_auth_token": "faux-jeton",
                "app_secret": "faux-secret",
                "email": "essai@example.com",
            }),
        );
        caviarder_carte(&mut carte);
        // Le NOM du bloc porte deja `token` : il part en entier, ce qui couvre
        // aussi les sous-champs que la liste manuelle d'avant ne nommait pas.
        assert_eq!(carte["auth_tokens_qobuz"], json!(MASQUE));
    }

    #[test]
    fn un_secret_dans_un_tableau_est_masque() {
        let mut carte = serde_json::Map::new();
        carte.insert(
            "plugins_configures".to_string(),
            json!([{ "nom": "hue", "api_key": "faux" }]),
        );
        caviarder_carte(&mut carte);
        assert_eq!(carte["plugins_configures"][0]["api_key"], json!(MASQUE));
        assert_eq!(carte["plugins_configures"][0]["nom"], json!("hue"));
    }

    /// Une clé inventée après ce correctif, jamais nommée nulle part.
    #[test]
    fn une_cle_ajoutee_plus_tard_est_couverte_par_son_nom() {
        assert!(est_secret("napster_refresh_token"));
        assert!(est_secret("un_service_client_secret"));
        assert!(!est_secret("napster_enabled"));
    }

    /// Une sauvegarde se ré-importe : elle doit OMETTRE le secret, jamais
    /// porter [`MASQUE`] à sa place — sinon la restauration écrit `********`
    /// par-dessus le vrai secret de signature.
    #[test]
    fn le_retrait_omet_la_cle_au_lieu_d_y_poser_le_masque() {
        let mut carte = serde_json::Map::new();
        carte.insert("jwt_secret".to_string(), json!("faux"));
        carte.insert("theme".to_string(), json!("sombre"));
        carte.insert(
            "airplay2_pairing:salon".to_string(),
            json!({ "our_ed25519_seed_hex": "faux", "accessory_id": "salon" }),
        );
        retirer_les_secrets(&mut carte);
        assert!(!carte.contains_key("jwt_secret"));
        assert_eq!(carte["theme"], json!("sombre"));
        let bloc = &carte["airplay2_pairing:salon"];
        assert!(bloc.get("our_ed25519_seed_hex").is_none());
        assert_eq!(bloc["accessory_id"], json!("salon"));
        let serialise = serde_json::to_string(&Value::Object(carte)).unwrap();
        assert!(
            !serialise.contains(MASQUE),
            "une sauvegarde ne porte jamais le marqueur de masquage"
        );
    }
}
