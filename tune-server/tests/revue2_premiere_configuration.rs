//! Première configuration dans un conteneur : sans autorisation explicite de
//! l'exploitant, la voie sans code est fermée ; détection du conteneur ;
//! lecture des passerelles.
//!
//! Toutes posent la même valeur de `TUNE_TEST_PASSERELLE_CONTENEUR` (une seule
//! passerelle par défaut, 172.17.0.1) : l'ordre d'exécution est indifférent.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::auth::{ouvrir_la_premiere_configuration, passerelles_par_defaut};
use tune_server::state::AppState;

fn conteneur_en_pont() {
    // SAFETY: même valeur partout dans ce binaire.
    unsafe { std::env::set_var("TUNE_TEST_PASSERELLE_CONTENEUR", "172.17.0.1") };
}

fn installation_neuve() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    assert!(ouvrir_la_premiere_configuration(&state));
    state
}

fn active(state: &AppState) -> bool {
    tune_server::auth::authentification_active(state)
}

async fn creer(state: &AppState, adresse: &str, entetes: &[(&str, &str)]) -> (StatusCode, Value) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/activation")
        .header(header::CONTENT_TYPE, "application/json");
    for (n, v) in entetes {
        req = req.header(*n, *v);
    }
    let resp = app
        .oneshot(
            req.body(Body::from(r#"{"password":"intrus-1234"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let o = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&o)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&o).to_string()));
    (st, v)
}

/// conteneur relié à DEUX réseaux (compose), connexion relayée
/// par le mandataire du moteur (Docker Desktop, docker-proxy en IPv6 ou en
/// épingle) sur le réseau qui ne porte pas la route par défaut : la source est
/// la passerelle de CE réseau (172.18.0.1), absente de la table des routes
/// par défaut. Même mécanisme : Kubernetes (SNAT vers l'adresse du nœud),
/// mandataire userland d'un autre réseau.
#[tokio::test]
async fn contournement_passerelle_d_un_second_reseau() {
    conteneur_en_pont();
    let state = installation_neuve();
    let (st, v) = creer(&state, "172.18.0.1:50000", &[]).await;
    assert_ne!(
        st,
        StatusCode::OK,
        "passerelle_non_par_defaut_vue_comme_lan {v}"
    );
    assert!(!active(&state));
}

/// mandataire inverse dans un AUTRE conteneur du même réseau
/// (nginx `proxy_pass http://tune:8888; proxy_http_version 1.1;` sans
/// `X-Forwarded-For`, ou relais TCP), exposé à Internet. La source est
/// l'adresse du conteneur mandataire (172.17.0.5), pas la passerelle ; l'hôte
/// `tune:8888` (un seul label) est accepté.
#[tokio::test]
async fn contournement_mandataire_conteneur_sans_en_tete() {
    conteneur_en_pont();
    let state = installation_neuve();
    let (st, v) = creer(&state, "172.17.0.5:50000", &[("host", "tune:8888")]).await;
    assert_ne!(st, StatusCode::OK, "mandataire_du_pont_vu_comme_lan {v}");
    assert!(!active(&state));
}

/// Détection : containerd/nerdctl, LXC/Incus, systemd-nspawn
/// en cgroup v2 avec espace de noms cgroup privé : ni `/.dockerenv`, ni
/// `/run/.containerenv`, et `/proc/1/cgroup` vaut `0::/`. Le serveur se croit
/// hors conteneur et ne fait AUCUNE vérification de passerelle.
#[test]
fn contournement_conteneur_cgroup_v2_non_detecte() {
    let i = tune_server::conteneur::Indices {
        cgroup: "0::/\n",
        ..Default::default()
    };
    // Le reste du serveur garde sa détection : pas de conteneur ici.
    assert_eq!(tune_server::conteneur::moteur(&i), None);
    assert!(
        tune_server::conteneur::conteneur_probable(&i, false, false),
        "conteneur_cgroup_v2_sans_marqueur_non_detecte"
    );
}

/// IPv6 : passerelle en lien local et plusieurs routes par défaut (RA de deux
/// routeurs) : toutes sont lues.
#[test]
fn ipv6_lien_local_et_plusieurs_passerelles() {
    let v4 = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
eth1\t00000000\t0100A8C0\t0003\t0\t0\t200\t00000000\t0\t0\t0\n\
eth0\t0001A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
    let v6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00450003 eth0\n\
00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000211223344556677 00000400 00000001 00000000 00450003 eth0\n\
00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200 lo\n";
    let p = passerelles_par_defaut(v4, v6);
    for a in [
        "192.168.1.1",
        "192.168.0.1",
        "fe80::1",
        "fe80::211:2233:4455:6677",
    ] {
        assert!(p.contains(&a.parse().unwrap()), "{a} absente de {p:?}");
    }
    assert_eq!(p.len(), 4, "{p:?}");
}

// Le cas `network_mode: host` AUTORISÉ par l'exploitant pose une autre
// variable : il est dans `premiere_configuration_environnement.rs`.

/// Limite connue (pas une faille) : un appareil du LAN qui joint Tune par son
/// adresse IPv6 GLOBALE n'est pas « réseau local » : il lui faut le code.
#[tokio::test]
async fn limite_ipv6_globale_du_lan_demande_le_code() {
    conteneur_en_pont();
    let state = installation_neuve();
    let (st, v) = creer(&state, "[2a01:e0a:1:2::50]:50000", &[]).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(!active(&state));
}
