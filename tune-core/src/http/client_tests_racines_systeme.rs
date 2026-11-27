//! rc14.1 (fil 3136) — les certificats du magasin du système s'ajoutent aux
//! racines webpki, sans jamais les remplacer.
//!
//! Le banc : un serveur HTTPS local dont le certificat est signé par une AC
//! PRIVÉE (`tests/fixtures/tls_ac_privee/`, clés de TEST publiées, jamais
//! dans un binaire livré), comme celle qu'un antivirus installe dans le
//! magasin de Windows pour inspecter le HTTPS.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{configuration_tls_avec, racines_de_confiance};

const AC_PRIVEE: &[u8] = include_bytes!("../../tests/fixtures/tls_ac_privee/ac.pem");
const CERT_SERVEUR: &[u8] = include_bytes!("../../tests/fixtures/tls_ac_privee/serveur.pem");
const CLE_SERVEUR: &[u8] = include_bytes!("../../tests/fixtures/tls_ac_privee/serveur.key");

fn ac_privee() -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(AC_PRIVEE).expect("ac.pem lisible")
}

/// Un serveur HTTPS sur 127.0.0.1, signé par l'AC privée : répond `200 ok`
/// à chaque connexion. Rend son port.
async fn serveur_signe_par_l_ac_privee() -> u16 {
    let certs = vec![CertificateDer::from_pem_slice(CERT_SERVEUR).expect("serveur.pem")];
    let cle = PrivateKeyDer::from_pem_slice(CLE_SERVEUR).expect("serveur.key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("versions")
    .with_no_client_auth()
    .with_single_cert(certs, cle)
    .expect("certificat du serveur");
    let accepteur = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = ecoute.accept().await {
            let accepteur = accepteur.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = accepteur.accept(tcp).await else {
                    return;
                };
                let mut lu = Vec::new();
                let mut tampon = [0u8; 1024];
                while !lu.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut tampon).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => lu.extend_from_slice(&tampon[..n]),
                    }
                }
                let _ = tls
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

async fn requete(magasin: Vec<CertificateDer<'static>>, port: u16) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(configuration_tls_avec(racines_de_confiance(magasin)))
        .build()
        .map_err(|e| e.to_string())?;
    let reponse = client
        .get(format!("https://localhost:{port}/"))
        .send()
        .await
        .map_err(|e| super::decrire_erreur_http(&e))?;
    reponse.text().await.map_err(|e| e.to_string())
}

/// L'AC privée présente dans le magasin (simulé) : la connexion passe.
#[tokio::test]
async fn une_ac_privee_du_magasin_rend_la_connexion_possible() {
    let port = serveur_signe_par_l_ac_privee().await;
    assert_eq!(requete(vec![ac_privee()], port).await.as_deref(), Ok("ok"));
}

/// Témoin : sans elle (webpki seul, le comportement de la rc14), la même
/// connexion est refusée pour son certificat. C'est le symptôme du fil 3136.
#[tokio::test]
async fn sans_l_ac_privee_webpki_seul_refuse_la_connexion() {
    let port = serveur_signe_par_l_ac_privee().await;
    let erreur = requete(Vec::new(), port).await.expect_err("refus attendu");
    assert!(
        erreur.to_lowercase().contains("certificate") || erreur.contains("UnknownIssuer"),
        "refus pour le certificat attendu : {erreur}"
    );
}

/// Les racines webpki sont TOUJOURS là : magasin vide, magasin illisible
/// (octets quelconques), ou magasin réel ajouté par-dessus.
#[test]
fn les_racines_webpki_restent_quel_que_soit_le_magasin() {
    let webpki = webpki_roots::TLS_SERVER_ROOTS.len();
    assert_eq!(racines_de_confiance(Vec::new()).len(), webpki);

    let illisibles = vec![
        CertificateDer::from(vec![0u8; 3]),
        CertificateDer::from(b"pas un certificat".to_vec()),
    ];
    assert_eq!(racines_de_confiance(illisibles).len(), webpki);

    let avec_l_ac = racines_de_confiance(vec![CertificateDer::from(vec![1u8, 2, 3]), ac_privee()]);
    assert_eq!(
        avec_l_ac.len(),
        webpki + 1,
        "l'AC lisible s'ajoute, l'illisible est ignorée"
    );
}

/// La configuration partagée (`builder`) se construit même avec le magasin
/// réel de la machine, et garde au moins les racines webpki.
#[test]
fn la_configuration_partagee_se_construit_avec_le_magasin_reel() {
    let racines = racines_de_confiance(super::magasin_du_systeme());
    assert!(racines.len() >= webpki_roots::TLS_SERVER_ROOTS.len());
    let _ = super::configuration_tls();
}

/// Linux : l'AC posée par `SSL_CERT_FILE` est lue par le VRAI chargeur
/// (`magasin_du_systeme`), puis rend la connexion possible. La variable
/// d'environnement est posée dans un processus enfant (ce même binaire de
/// test, filtré sur ce test) : la poser ici courrait sur les autres tests.
#[cfg(target_os = "linux")]
#[test]
fn ssl_cert_file_est_lu_par_le_magasin_du_systeme() {
    const ENFANT: &str = "TUNE_TEST_RACINES_SYSTEME_ENFANT";
    if std::env::var_os(ENFANT).is_some() {
        let magasin = super::magasin_du_systeme();
        assert!(
            magasin.iter().any(|c| c.as_ref() == ac_privee().as_ref()),
            "l'AC de SSL_CERT_FILE n'est pas dans le magasin lu ({} certificats)",
            magasin.len()
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let port = serveur_signe_par_l_ac_privee().await;
            assert_eq!(requete(magasin, port).await.as_deref(), Ok("ok"));
        });
        return;
    }
    let fichier = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tls_ac_privee/ac.pem"
    );
    let sortie = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "http::client::tests_racines_systeme::ssl_cert_file_est_lu_par_le_magasin_du_systeme",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENFANT, "1")
        .env("SSL_CERT_FILE", fichier)
        .env_remove("SSL_CERT_DIR")
        .output()
        .expect("processus enfant");
    let texte = format!(
        "{}{}",
        String::from_utf8_lossy(&sortie.stdout),
        String::from_utf8_lossy(&sortie.stderr)
    );
    assert!(sortie.status.success(), "enfant en échec :\n{texte}");
    assert!(
        texte.contains("1 passed"),
        "le test enfant n'a pas tourné :\n{texte}"
    );
}

/// Android n'est pas touché : `tune-core` n'y déclare pas `rustls-native-certs`,
/// et son `magasin_du_systeme` ne rend rien.
#[test]
fn android_reste_aux_seules_racines_webpki() {
    let manifeste = include_str!("../../Cargo.toml");
    let section = manifeste
        .split("\n[")
        .find(|s| s.contains("rustls-native-certs"))
        .expect("rustls-native-certs déclaré");
    assert!(
        section.starts_with("target.'cfg(not(target_os = \"android\"))'.dependencies]"),
        "rustls-native-certs doit rester hors Android : [{}",
        section.lines().next().unwrap_or("")
    );
    let source = include_str!("client.rs");
    let android = source
        .split("#[cfg(target_os = \"android\")]\nfn magasin_du_systeme()")
        .nth(1)
        .expect("le magasin d'Android est déclaré à part");
    let corps = android.split("\n}").next().unwrap();
    assert!(corps.contains("Vec::new()") && !corps.contains("rustls_native_certs"));
}
