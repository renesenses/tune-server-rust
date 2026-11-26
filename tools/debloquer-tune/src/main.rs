//! Point d'entrée : un double-clic ouvre le navigateur sur la page locale.

use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use debloquer_tune::web::{self, Etat, Requete};

/// La page envoie un signe de vie toutes les 30 s ; onglet fermé depuis plus
/// longtemps que ça, l'outil s'arrête de lui-même (une app macOS n'a pas de
/// fenêtre à fermer).
const INACTIVITE_MAX: Duration = Duration::from_secs(5 * 60);

fn ouvrir_navigateur(url: &str) {
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if r.is_err() {
        println!(
            "Ouvrez cette adresse dans votre navigateur / Open this address in your browser :"
        );
    }
}

fn entete<'a>(req: &'a tiny_http::Request, nom: &str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(nom))
        .map(|h| h.value.as_str())
}

fn servir(etat: &Etat, mut req: tiny_http::Request) {
    let mut corps = String::new();
    // 64 Kio suffisent largement à un mot de passe et une adresse.
    let _ = req.as_reader().take(64 * 1024).read_to_string(&mut corps);
    let methode = req.method().as_str().to_string();
    let chemin = req.url().to_string();
    let reponse = web::traiter(
        etat,
        &Requete {
            methode: &methode,
            chemin: &chemin,
            hote: entete(&req, "Host"),
            origine: entete(&req, "Origin"),
            type_contenu: entete(&req, "Content-Type"),
            corps: &corps,
        },
    );
    let mut r = tiny_http::Response::from_string(reponse.corps).with_status_code(reponse.code);
    for (nom, valeur) in [
        ("Content-Type", reponse.type_contenu),
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
        ("Content-Security-Policy", "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'self'; frame-ancestors 'none'"),
    ] {
        if let Ok(h) = tiny_http::Header::from_bytes(nom.as_bytes(), valeur.as_bytes()) {
            r = r.with_header(h);
        }
    }
    let _ = req.respond(r);
}

fn main() {
    let serveur = match tiny_http::Server::http("127.0.0.1:0") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Impossible d'ouvrir le serveur local : {e}");
            std::thread::sleep(Duration::from_secs(15));
            std::process::exit(1);
        }
    };
    let port = serveur.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    let etat = Arc::new(Etat::new(web::nouveau_jeton(), port));
    let url = format!("http://127.0.0.1:{port}{}", etat.chemin_page());

    println!("Débloquer Tune {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("La page s'ouvre dans votre navigateur. Laissez cette fenêtre ouverte");
    println!("jusqu'à la fin ; fermez-la ensuite pour quitter.");
    println!("The page opens in your browser. Keep this window open until the end,");
    println!("then close it to quit.");
    println!();
    println!("{url}");
    ouvrir_navigateur(&url);

    let serveur = Arc::new(serveur);
    loop {
        if etat.quitter.load(Ordering::SeqCst) || etat.inactif_depuis() > INACTIVITE_MAX {
            break;
        }
        match serveur.recv_timeout(Duration::from_secs(1)) {
            Ok(Some(req)) => {
                let etat = Arc::clone(&etat);
                std::thread::spawn(move || servir(&etat, req));
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    // Laisser partir la réponse à « quitter ».
    std::thread::sleep(Duration::from_millis(300));
}
