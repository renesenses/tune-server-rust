//! Ce que le journal dit vraiment quand un descriptif UPnP est illisible (#2665).
//!
//! Journal de Jean Valjean (fil forum 1585, v0.9.116, 28/08/2026) — la seule
//! trace de l'incident, et tout ce qu'elle disait :
//!
//! ```text
//! DEBUG tune_core::discovery::xml_parser: xml_parse_error
//!     error=ill-formed document: expected `</meta>`, but `</head>` was found
//! ```
//!
//! On savait qu'une adresse du réseau rendait du HTML. On ne savait pas
//! laquelle. Impossible de l'ouvrir, de chercher qui l'annonce, ou de dire au
//! testeur quel équipement est en cause. Ce fichier verrouille l'inverse.
//!
//! ## Pourquoi un binaire de test à lui seul
//!
//! `tracing` met en cache, **pour tout le processus**, la décision « ce point
//! d'appel intéresse-t-il quelqu'un ? » ainsi que le niveau maximal utile. Un
//! abonné posé le temps d'un `await`, dans un binaire qui en crée des dizaines
//! en parallèle, se voit priver d'évènements de façon imprévisible : mesuré,
//! capture **vide 1 exécution sur 6** de la suite complète du crate, quand le
//! même test seul passait 8 fois sur 8.
//!
//! Ici l'abonné est **global** et le binaire ne contient **que ce test** : il
//! est installé avant toute autre chose, rien d'autre n'enregistre d'abonné, et
//! le résultat ne dépend plus d'un ordonnancement.

use std::sync::{Arc, Mutex};

use tune_core::discovery::xml_parser::fetch_device_description;

/// Page d'accueil type d'une console d'équipement : des balises vides non
/// refermées dans un `<head>`, ce qui produit la famille d'erreurs du journal
/// de Jean Valjean.
///
/// Le nom de réseau est délibérément placé **au-delà du 200e octet** : c'est ce
/// qui rend vérifiable que la troncature protège quelque chose.
const PAGE_HTML: &str = "<!DOCTYPE html>\n<html lang=\"fr\">\n<head>\n\
<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<link rel=\"stylesheet\" href=\"/static/admin.css\">\n\
<title>Console d'administration</title>\n</head>\n\
<body><h1>Bienvenue</h1><p>Réseau : Livebox-4F2A</p>\
<p>Adresse MAC : 00:1A:2B:3C:4D:5E</p></body></html>\n";

const NOM_DE_RESEAU: &str = "Livebox-4F2A";

/// Un serveur web ordinaire à l'adresse annoncée : il rend `PAGE_HTML` sur
/// `chemin`, et 404 partout ailleurs.
async fn serveur_web(chemin: &'static str) -> std::net::SocketAddr {
    // Port éphémère sur la boucle locale : pas d'IPv6, aucun chemin fixe, deux
    // exécutions concurrentes ne se disputent rien.
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = ecoute.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let tete = String::from_utf8_lossy(&req);
                let demande = tete
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let resp = if demande == chemin {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE_HTML}",
                        PAGE_HTML.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qu'on aura
/// entre les mains la prochaine fois.
#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn le_journal_dit_l_adresse_le_genre_la_taille_et_borne_l_extrait() {
    let capture = JournalCapture::default();
    // Niveau WARN : c'est ce qu'un journal ORDINAIRE laisse passer.
    // `log_level` vaut `info` par défaut (`tune-core/src/config.rs`), si bien
    // que l'ancien `debug!(… "xml_parse_error")` n'apparaissait même pas —
    // l'échec n'était pas seulement anonyme, il était invisible.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    const CHEMIN: &str = "/rootDesc.xml";
    let addr = serveur_web(CHEMIN).await;
    // L'adresse exacte qu'on doit pouvoir relire, puis coller dans un
    // navigateur pour identifier l'équipement.
    let location = format!("http://{addr}{CHEMIN}");

    let err = fetch_device_description(&location)
        .await
        .expect_err("une page HTML ne peut pas produire un descriptif UPnP");

    let texte = capture.texte();
    let ligne = texte
        .lines()
        .find(|l| l.contains("upnp_description_unreadable"))
        .unwrap_or_else(|| {
            panic!(
                "aucune trace de descriptif illisible au niveau WARN.\n\
                 erreur rendue : {err}\njournal capturé :\n{texte}"
            )
        });

    assert!(
        ligne.contains(&location),
        "sans l'adresse, la trace ne permet pas d'agir — c'est tout le défaut \
         de #2665 : {ligne}"
    );
    assert!(
        ligne.contains("page HTML"),
        "la trace doit dire que le corps était une page web, pas un descriptif \
         mal formé : les gestes ne sont pas les mêmes : {ligne}"
    );
    assert!(
        ligne.contains(&format!(" octets={}", PAGE_HTML.len())),
        "la trace doit annoncer la taille RÉELLE du corps reçu : {ligne}"
    );
    assert!(
        ligne.contains("extrait_octets=200") && ligne.contains("tronque=true"),
        "la trace doit dire combien elle a montré et qu'elle a coupé : {ligne}"
    );
    assert!(
        !ligne.contains(NOM_DE_RESEAU),
        "le corps ne doit JAMAIS être journalisé en entier : la page servie est \
         celle d'un équipement du foyer, et un journal de testeur part sur un \
         forum public : {ligne}"
    );
    assert!(
        ligne.contains("occurrences_tues=0"),
        "la première occurrence doit s'annoncer comme telle : {ligne}"
    );
}
