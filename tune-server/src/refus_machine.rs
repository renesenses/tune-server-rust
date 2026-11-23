//! rc14 — le refus d'une action réservée à la machine du serveur ou à un
//! administrateur (`auth::AdminOuCetteMachine`), et celui de l'activation de
//! l'authentification depuis un autre appareil, en phrases COURTES et justes.
//!
//! ## Le défaut (capture d'Yves, rc13.1)
//!
//! « Redémarrer le serveur », cliqué depuis un Mac du réseau, rendait un
//! paragraphe illisible pour un utilisateur ordinaire, qui citait des
//! commandes (`docker restart`, `journalctl -u tune`) et disait « Tune OS :
//! SSH » — faux : le SSH par mot de passe y est fermé, on passe par Cockpit
//! (`https://<adresse>:9443`), et le service s'appelle `tune.service`.
//!
//! ## Le contrat
//!
//! * `reason` : le motif machine (`not_local`, `port80_relay`), inchangé, que
//!   le client lit ;
//! * `message` (+ `message_key`) : une phrase dit ce qui est refusé et
//!   pourquoi, une autre dit quoi faire — « sur la machine du serveur, ou en
//!   vous connectant en administrateur ». Aucune commande ;
//! * `help_url` : le fil épinglé du forum qui explique comment faire.
//!
//! Les commandes, quand elles servent (où lire le code d'activation), vivent
//! dans un champ technique séparé, justes par plateforme : [`OU_LIRE_LE_CODE`].

use serde_json::{Value, json};

/// Le fil épinglé du forum : « rc13 : mise à jour bloquée ou actions
/// refusées ? Activez l'authentification ».
pub const AIDE_ACTIONS_MACHINE: &str = "https://mozaiklabs.fr/forum/threads/rc13-mise-a-jour-bloquee-ou-actions-refusees-activez-lauthentification-update-blocked-turn-on-authentication-3eAFTN";

/// Où lire le code d'activation, par plateforme. Jamais « SSH » pour Tune OS.
pub const OU_LIRE_LE_CODE: [(&str, &str); 4] = [
    ("docker", "docker logs <container>"),
    ("deb", "journalctl -u tune-server"),
    (
        "tune_os",
        "Cockpit https://<address>:9443 > Logs (unit tune.service)",
    ),
    ("windows", "tune-server.log"),
];

/// La clé de la phrase pour un motif de refus « hors de cette machine ».
pub fn cle_du_motif(motif: &str) -> &'static str {
    if motif == "port80_relay" {
        "api.machine.refusedPort80Relay"
    } else {
        "api.machine.refusedNotLocal"
    }
}

/// Le corps du refus de `AdminOuCetteMachine`, dans la langue de la requête.
pub fn corps_refus_hors_machine(motif: &str, appelant: &str, relaye: bool) -> Value {
    crate::message_api::avec_message(
        json!({
            "error": "auth_required",
            "reason": motif,
            "help_url": AIDE_ACTIONS_MACHINE,
            "caller": appelant,
            "relayed": relaye,
        }),
        cle_du_motif(motif),
        &[],
    )
}

/// Le corps du refus d'activation de l'authentification depuis un autre
/// appareil sans code, dans la langue de la requête.
pub fn corps_refus_activation_hors_machine() -> Value {
    let ou: serde_json::Map<String, Value> = OU_LIRE_LE_CODE
        .iter()
        .map(|(plateforme, ou)| ((*plateforme).to_string(), json!(ou)))
        .collect();
    crate::message_api::avec_message(
        json!({
            "error": "activation_code_required",
            "reason": "not_local",
            "help_url": AIDE_ACTIONS_MACHINE,
            "code_location": ou,
        }),
        "api.machine.activationCodeRequired",
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLES: [&str; 3] = [
        "api.machine.refusedNotLocal",
        "api.machine.refusedPort80Relay",
        "api.machine.activationCodeRequired",
    ];

    /// Ce que le message ne doit JAMAIS contenir : des commandes, et « SSH »
    /// (fermé sur Tune OS).
    const INTERDITS: [&str; 7] = [
        "SSH",
        "docker",
        "journalctl",
        "localhost",
        "<container>",
        "tune-server.log",
        "POST /",
    ];

    #[test]
    fn refus_hors_machine_court_sans_commande_motif_garde() {
        for motif in ["not_local", "port80_relay"] {
            let corps = corps_refus_hors_machine(motif, "192.168.1.20", false);
            assert_eq!(corps["error"], json!("auth_required"));
            assert_eq!(corps["reason"], json!(motif), "le motif machine reste lu par le client");
            assert_eq!(corps["help_url"], json!(AIDE_ACTIONS_MACHINE));
            assert_eq!(corps["caller"], json!("192.168.1.20"));
            assert_eq!(corps["message_key"], json!(cle_du_motif(motif)));
            let m = corps["message"].as_str().unwrap();
            for interdit in INTERDITS {
                assert!(!m.contains(interdit), "{motif} : « {interdit} » dans {m}");
            }
        }
        let m = corps_refus_hors_machine("not_local", "x", false)["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(m.contains("server machine"), "{m}");
        assert!(m.contains("administrator"), "{m}");
    }

    #[test]
    fn chaque_langue_courte_et_sans_commande() {
        for cle in CLES {
            for lang in crate::i18n::SUPPORTED {
                let m = crate::message_api::texte_en(lang, cle, &[]);
                assert_ne!(m, cle, "{lang} : {cle} absente");
                assert!(
                    m.chars().count() <= 260,
                    "{lang} {cle} : trop long ({} car.) : {m}",
                    m.chars().count()
                );
                for interdit in INTERDITS {
                    assert!(!m.contains(interdit), "{lang} {cle} : « {interdit} » dans {m}");
                }
            }
        }
        let fr = crate::message_api::texte_en("fr", "api.machine.refusedNotLocal", &[]);
        assert!(fr.contains("machine du serveur"), "{fr}");
        assert!(fr.contains("administrateur"), "{fr}");
    }

    /// 🔴 Témoin de bout en bout : le VRAI routeur, un Mac du réseau
    /// (192.168.1.20), authentification coupée, interface en français.
    /// Avant rc14 : « This action requires authentication to be enabled, or a
    /// call from this machine (localhost). » en anglais, sans lien d'aide ; et
    /// l'activation citait « on Tune OS over SSH ».
    #[tokio::test]
    async fn temoin_routeur_redemarrer_arreter_et_activer_depuis_le_reseau() {
        use axum::body::{Body, to_bytes};
        use axum::extract::ConnectInfo;
        use axum::http::{Request, StatusCode, header};
        use tower::ServiceExt;

        let dossier = tempfile::tempdir().unwrap();
        let base = dossier.path().join("tune.db");
        let state =
            crate::state::AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
        let appel = |uri: &'static str, corps: Option<Value>| {
            let state = state.clone();
            async move {
                let adresse: std::net::SocketAddr = "192.168.1.20:51000".parse().unwrap();
                let mut requete = Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("accept-language", "fr-FR,fr;q=0.9")
                    .extension(ConnectInfo(adresse));
                let body = match corps {
                    Some(c) => {
                        requete = requete.header(header::CONTENT_TYPE, "application/json");
                        Body::from(c.to_string())
                    }
                    None => Body::empty(),
                };
                let reponse = crate::routes::router(state)
                    .oneshot(requete.body(body).unwrap())
                    .await
                    .unwrap();
                let statut = reponse.status();
                let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
                (
                    statut,
                    serde_json::from_slice::<Value>(&octets).unwrap_or(Value::Null),
                )
            }
        };
        for uri in ["/api/v1/system/restart", "/api/v1/system/stop"] {
            let (statut, corps) = appel(uri, None).await;
            assert_eq!(statut, StatusCode::FORBIDDEN, "{uri} : {corps}");
            assert_eq!(corps["error"], json!("auth_required"), "{uri} : {corps}");
            assert_eq!(corps["reason"], json!("not_local"), "{uri} : {corps}");
            assert_eq!(corps["help_url"], json!(AIDE_ACTIONS_MACHINE), "{uri} : {corps}");
            let m = corps["message"].as_str().unwrap_or_default();
            assert!(m.contains("machine du serveur"), "{uri} : {m}");
            assert!(m.contains("administrateur"), "{uri} : {m}");
            for interdit in INTERDITS {
                assert!(!m.contains(interdit), "{uri} : « {interdit} » dans {m}");
            }
        }
        let (statut, corps) = appel(
            "/api/v1/auth/config",
            Some(json!({ "auth_enabled": true })),
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{corps}");
        assert_eq!(corps["error"], json!("activation_code_required"), "{corps}");
        assert!(!corps.to_string().contains("SSH"), "{corps}");
        assert!(
            corps["code_location"]["tune_os"]
                .as_str()
                .is_some_and(|t| t.contains("Cockpit")),
            "{corps}"
        );
    }

    #[test]
    fn activation_tune_os_par_cockpit_jamais_ssh() {
        let corps = corps_refus_activation_hors_machine();
        assert_eq!(corps["error"], json!("activation_code_required"));
        assert_eq!(corps["reason"], json!("not_local"));
        let texte = corps.to_string();
        assert!(!texte.contains("SSH"), "{texte}");
        let tune_os = corps["code_location"]["tune_os"].as_str().unwrap();
        assert!(tune_os.contains("Cockpit"), "{tune_os}");
        assert!(tune_os.contains(":9443"), "{tune_os}");
        assert!(tune_os.contains("tune.service"), "{tune_os}");
        assert_eq!(
            corps["code_location"]["deb"],
            json!("journalctl -u tune-server")
        );
    }
}
