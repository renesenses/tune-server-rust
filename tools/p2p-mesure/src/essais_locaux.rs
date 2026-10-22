//! Essais serveur-client en boucle locale : deux endpoints iroh dans le même
//! processus, sur 127.0.0.1, sans relais (`RelayMode::Disabled`), sans
//! portmapper, sans découverte. Aucun réseau extérieur.

use std::collections::HashSet;
use std::time::Duration;

use iroh::{Endpoint, EndpointAddr, RelayMode, SecretKey, TransportAddr};

use crate::client::{jouer, ParamsClient, Scenario};
use crate::journal::{self, contient_une_adresse_ip, InfoClient, InfoServeur};
use crate::protocole::ALPN;
use crate::serveur::{lire_jetons, servir, ConfigServeur};
use crate::{ouvrir_endpoint, protocole};

const JETON: &str = "jeton-de-campagne-t03";

struct Banc {
    cible: EndpointAddr,
    serveur: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for Banc {
    fn drop(&mut self) {
        self.serveur.abort();
    }
}

async fn endpoint_local(cle: SecretKey, alpns: Vec<Vec<u8>>) -> Endpoint {
    ouvrir_endpoint(cle, alpns, RelayMode::Disabled, true, false)
        .await
        .expect("endpoint local")
}

/// Démarre un serveur local qui n'autorise que `autorise`.
async fn banc(autorise: iroh::EndpointId) -> Banc {
    let ep = endpoint_local(SecretKey::generate(), vec![ALPN.to_vec()]).await;
    let cible = EndpointAddr::from_parts(
        ep.id(),
        ep.bound_sockets().into_iter().map(TransportAddr::Ip),
    );
    let cfg = ConfigServeur {
        campagne: "c1".into(),
        jetons: lire_jetons(&format!("t03 {JETON}")).unwrap(),
        autorises: HashSet::from([autorise]),
        info: InfoServeur {
            site: "domicile".into(),
            box_: "bbox".into(),
            famille: "ipv4".into(),
        },
    };
    let serveur = tokio::spawn(servir(ep, cfg, Duration::from_secs(120)));
    Banc { cible, serveur }
}

fn params(jeton: &str, scenario: Scenario) -> ParamsClient {
    ParamsClient {
        campagne: "c1".into(),
        testeur: "t03".into(),
        jeton: jeton.into(),
        client: InfoClient {
            os: "linux".into(),
            plateforme: "cli".into(),
            reseau: "wifi".into(),
            operateur: "aucun".into(),
            partage: false,
        },
        scenario,
    }
}

fn petit_scenario() -> Scenario {
    Scenario {
        pings: Duration::from_millis(1200),
        vrac: Duration::from_millis(500),
        formats: vec!["16/44".into()],
        duree_flux: Duration::from_millis(2500),
    }
}

/// Les clés de premier niveau, dans l'ordre de la ligne d'exemple du § 3.2.
const CLES_DU_CAHIER: [&str; 18] = [
    "v",
    "campagne",
    "testeur",
    "essai",
    "debut",
    "client",
    "serveur",
    "relais",
    "chemin_final",
    "famille_direct",
    "t_connecte_ms",
    "t_premier_octet_ms",
    "t_direct_ms",
    "debit_max_mbps",
    "flux",
    "rtt",
    "batterie",
    "erreur",
];

#[tokio::test(flavor = "multi_thread")]
async fn boucle_locale_ecrit_une_ligne_jsonl_valide_sans_adresse_ip() {
    let cle_client = SecretKey::generate();
    let banc = banc(cle_client.public()).await;
    let client = endpoint_local(cle_client, vec![]).await;
    let essai = jouer(
        &client,
        banc.cible.clone(),
        &params(JETON, petit_scenario()),
    )
    .await;
    client.close().await;
    assert_eq!(essai.erreur, None, "essai en erreur : {essai:?}");

    let dossier = tempfile::tempdir().unwrap();
    let chemin = journal::ajouter(dossier.path(), &essai).unwrap();
    let nom = chemin.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        nom.starts_with("p2p-mesure-c1-t03-") && nom.ends_with(".jsonl"),
        "{nom}"
    );
    let texte = std::fs::read_to_string(&chemin).unwrap();
    assert_eq!(texte.lines().count(), 1);
    let ligne = texte.lines().next().unwrap();

    // Ni IP ni position (§ 3.2 et § 5).
    assert_eq!(
        contient_une_adresse_ip(ligne),
        None,
        "adresse IP dans le journal : {ligne}"
    );
    for interdit in ["latitude", "longitude", "position", "ssid", "127.0.0.1"] {
        assert!(
            !ligne.contains(interdit),
            "« {interdit} » dans le journal : {ligne}"
        );
    }

    // Ligne JSON valide, avec exactement les clés du § 3.2, dans l'ordre.
    let valeur: serde_json::Value = serde_json::from_str(ligne).unwrap();
    let cles: Vec<&str> = valeur
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut attendues = CLES_DU_CAHIER.to_vec();
    attendues.sort();
    let mut vues = cles.clone();
    vues.sort();
    assert_eq!(vues, attendues);
    assert_eq!(&ligne[..6], "{\"v\":1");

    // Boucle locale : le direct est le seul chemin possible.
    assert_eq!(valeur["chemin_final"], "direct", "{ligne}");
    assert_eq!(valeur["famille_direct"], "ipv4");
    assert_eq!(valeur["relais"], "aucun");
    assert_eq!(valeur["serveur"]["box"], "bbox");
    assert!(valeur["t_connecte_ms"].is_u64());
    assert!(valeur["t_premier_octet_ms"].is_u64());
    assert!(valeur["t_direct_ms"].is_u64());
    assert!(valeur["debit_max_mbps"].as_f64().unwrap() > 0.0);
    assert_eq!(valeur["flux"][0]["format"], "16/44");
    assert_eq!(valeur["flux"][0]["sous_debits"], 0, "{ligne}");
    assert!(valeur["rtt"]["direct"]["p50"].is_number(), "{ligne}");
    assert!(valeur["rtt"]["pertes_ping"].as_f64().unwrap() < 0.5);
    assert!(valeur["batterie"].is_null());

    // Et la ligne se relit par l'agrégation.
    let (essais, illisibles) = crate::agreger::lire(&[texte]);
    assert_eq!((essais.len(), illisibles), (1, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn identifiant_inconnu_refuse() {
    let banc = banc(SecretKey::generate().public()).await;
    let intrus = endpoint_local(SecretKey::generate(), vec![]).await;
    let essai = jouer(
        &intrus,
        banc.cible.clone(),
        &params(JETON, petit_scenario()),
    )
    .await;
    intrus.close().await;
    let erreur = essai.erreur.clone().unwrap_or_default();
    assert!(
        erreur.contains("identifiant inconnu"),
        "un identifiant hors liste blanche doit être refusé : {essai:?}"
    );
    assert_eq!(essai.chemin_final, None, "un essai refusé ne compte pas");
    assert!(essai.flux.is_empty() && essai.debit_max_mbps.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn jeton_inconnu_refuse() {
    let cle_client = SecretKey::generate();
    let banc = banc(cle_client.public()).await;
    let client = endpoint_local(cle_client, vec![]).await;
    let essai = jouer(
        &client,
        banc.cible.clone(),
        &params("jeton-de-campagne-faux", petit_scenario()),
    )
    .await;
    client.close().await;
    let erreur = essai.erreur.clone().unwrap_or_default();
    assert!(
        erreur.contains("jeton refusé"),
        "un jeton inconnu doit être refusé : {essai:?}"
    );
    assert_eq!(essai.chemin_final, None);
    assert!(essai.flux.is_empty() && essai.debit_max_mbps.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn alpn_inconnue_refusee() {
    let cle_client = SecretKey::generate();
    let banc = banc(cle_client.public()).await;
    let client = endpoint_local(cle_client, vec![]).await;
    let refus = client.connect(banc.cible.clone(), b"autre/1").await;
    assert!(
        refus.is_err(),
        "une ALPN inconnue doit échouer à la poignée de main TLS"
    );
    client.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn le_serveur_s_arrete_seul() {
    let ep = endpoint_local(SecretKey::generate(), vec![ALPN.to_vec()]).await;
    let cfg = ConfigServeur {
        campagne: "c1".into(),
        jetons: lire_jetons(&format!("t03 {JETON}")).unwrap(),
        autorises: HashSet::from([SecretKey::generate().public()]),
        info: InfoServeur {
            site: "domicile".into(),
            box_: "bbox".into(),
            famille: "ipv4".into(),
        },
    };
    let fin = tokio::time::timeout(
        Duration::from_secs(10),
        servir(ep, cfg, Duration::from_millis(300)),
    )
    .await;
    assert!(
        fin.is_ok(),
        "le serveur doit s'arrêter seul à sa durée de vie"
    );
}

#[test]
fn le_ticket_local_se_decode() {
    let id = SecretKey::generate().public();
    let addr = EndpointAddr::from_parts(id, [TransportAddr::Ip("127.0.0.1:1".parse().unwrap())]);
    let t = protocole::encoder_ticket(&addr).unwrap();
    assert_eq!(protocole::decoder_ticket(&t).unwrap(), addr);
}
