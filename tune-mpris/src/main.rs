//! `tune-mpris` : publie une zone Tune sur MPRIS (#8824).

use clap::Parser;

use tune_mpris::client::{ClientTune, JETON_REQUIS, SERVEUR_PAR_DEFAUT, choisir_zone};

#[derive(Parser)]
#[command(
    name = "tune-mpris",
    version,
    about = "Compagnon MPRIS de Tune : touches multimédia, applets du bureau et playerctl pilotent une zone Tune"
)]
struct Options {
    /// Adresse du serveur Tune.
    #[arg(long, env = "TUNE_SERVER", default_value = SERVEUR_PAR_DEFAUT)]
    server: String,
    /// Zone à suivre : identifiant ou nom. Par défaut : celle qui joue,
    /// sinon une en pause, sinon la première.
    #[arg(long, env = "TUNE_ZONE")]
    zone: Option<String>,
    /// Jeton des clients web/remote (JWT), ou « ApiKey <clé> ». Seulement si
    /// l'authentification est active sur le serveur.
    #[arg(long, env = "TUNE_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Liste les zones du serveur et sort.
    #[arg(long)]
    list_zones: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let options = Options::parse();
    let mut client = ClientTune::new(&options.server, options.token.as_deref());

    // Lancé avec la session, le compagnon peut partir avant le réseau ou le
    // serveur : il attend la liste des zones.
    let mut attente = std::time::Duration::from_secs(1);
    let zones = loop {
        match client.zones().await {
            Ok(z) => break z,
            Err(e) if options.list_zones || e == JETON_REQUIS => {
                eprintln!("{} : {e}", client.serveur);
                std::process::exit(1);
            }
            Err(e) => {
                tracing::warn!(
                    "{} injoignable ({e}), nouvel essai dans {attente:?}",
                    client.serveur
                );
                tokio::time::sleep(attente).await;
                attente = tune_mpris::synchro::attente_suivante(attente);
            }
        }
    };

    if options.list_zones {
        for z in &zones {
            println!(
                "{}\t{}\t{}",
                z.get("id").and_then(|v| v.as_i64()).unwrap_or_default(),
                z.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                z.get("state").and_then(|v| v.as_str()).unwrap_or(""),
            );
        }
        return;
    }

    let zone = match choisir_zone(&zones, options.zone.as_deref()) {
        Ok(z) => z,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    client.zone_id = zone.get("id").and_then(|v| v.as_i64()).unwrap_or_default();
    let nom = zone
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    lancer(client, nom).await;
}

#[cfg(target_os = "linux")]
async fn lancer(client: ClientTune, nom_de_zone: String) {
    use std::sync::{Arc, Mutex};
    use tune_mpris::bus::{Lecteur, NotifieurBus, Racine, publier};
    use tune_mpris::etat::Etat;

    let zone_id = client.zone_id;
    let etat = Arc::new(Mutex::new(Etat::new(
        zone_id,
        nom_de_zone.clone(),
        std::time::Instant::now(),
    )));
    let connexion = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bus D-Bus de la session injoignable : {e}");
            std::process::exit(1);
        }
    };
    let racine = Racine {
        etat: etat.clone(),
        serveur: client.serveur.clone(),
    };
    let lecteur = Lecteur {
        etat: etat.clone(),
        commandes: Arc::new(client.clone()),
    };
    match publier(&connexion, racine, lecteur, zone_id).await {
        Ok(nom) => tracing::info!("{nom} publié pour la zone {zone_id} ({nom_de_zone})"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    let notifieur = Arc::new(NotifieurBus { connexion });
    tune_mpris::synchro::suivre(client, etat, notifieur).await;
}

#[cfg(not(target_os = "linux"))]
async fn lancer(_client: ClientTune, _nom_de_zone: String) {
    eprintln!("MPRIS n'existe que sur le bus D-Bus d'un bureau Linux.");
    std::process::exit(1);
}
