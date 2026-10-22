//! p2p-mesure : banc de mesure pair-à-pair pour l'écoute à distance.
//!
//! Hors produit, pour la rc8 (cahier des charges du 19/10/2026). Trois
//! sous-commandes : `serveur`, `client <ticket>`, `agreger <fichiers…>`.
//! Voir LISEZMOI.md.

mod agreger;
mod chemins;
mod client;
mod journal;
mod protocole;
mod serveur;
mod stats;
mod tampon;

#[cfg(test)]
mod essais_locaux;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use data_encoding::HEXLOWER;
use iroh::endpoint::{presets, PortmapperConfig};
use iroh::{Endpoint, RelayMode, SecretKey};

use crate::journal::{identifiant_valide, InfoClient, InfoServeur};

#[derive(Parser)]
#[command(
    name = "p2p-mesure",
    version,
    about = "Banc de mesure pair-à-pair (iroh) — hors produit"
)]
struct Cli {
    #[command(subcommand)]
    commande: Commande,
}

#[derive(Subcommand)]
enum Commande {
    /// Ouvre un endpoint iroh (ALPN tune-mesure/1), affiche le ticket et attend les essais.
    Serveur {
        #[arg(long)]
        campagne: String,
        /// Fichier « testeur jeton », une ligne par testeur.
        #[arg(long)]
        jetons: PathBuf,
        /// Fichier des identifiants d'endpoint autorisés, un par ligne.
        #[arg(long)]
        autorises: PathBuf,
        /// `n0` (relais publics, phase 0 seulement), `aucun`, ou l'URL du relais auto-hébergé.
        #[arg(long)]
        relais: String,
        /// Clé du serveur (créée si absente) : le ticket reste le même d'un lancement à l'autre.
        #[arg(long)]
        cle: PathBuf,
        #[arg(long, value_parser = ["domicile", "vps", "autre"])]
        site: String,
        #[arg(long = "box", value_parser = ["livebox", "freebox", "freebox-ipv4-partagee", "sfrbox", "bbox", "vps", "autre"])]
        box_: String,
        /// Durée de vie en heures, 24 au plus (et par défaut).
        #[arg(long, default_value_t = 24.0)]
        duree_max_h: f64,
        /// Désactive UPnP / NAT-PMP / PCP.
        #[arg(long)]
        sans_portmapper: bool,
    },
    /// Joue un essai contre le serveur du ticket et ajoute une ligne au journal.
    Client {
        /// Ticket affiché par `p2p-mesure serveur`.
        #[arg(required_unless_present = "identite")]
        ticket: Option<String>,
        /// Clé du client pour la campagne (créée si absente).
        #[arg(long)]
        cle: PathBuf,
        /// Affiche l'identifiant d'endpoint de la clé (à envoyer à Bertrand) et sort.
        #[arg(long)]
        identite: bool,
        #[arg(long, required_unless_present = "identite")]
        campagne: Option<String>,
        /// Pseudonyme (t01, t02…).
        #[arg(long, required_unless_present = "identite")]
        testeur: Option<String>,
        #[arg(
            long,
            env = "P2P_MESURE_JETON",
            hide_env_values = true,
            required_unless_present = "identite"
        )]
        jeton: Option<String>,
        #[arg(long, value_parser = ["wifi", "5g", "4g", "filaire", "wifi-public", "entreprise"], required_unless_present = "identite")]
        reseau: Option<String>,
        #[arg(long, value_parser = ["orange", "sfr", "bouygues", "free", "autre", "aucun"], required_unless_present = "identite")]
        operateur: Option<String>,
        /// Le client passe par le partage de connexion du téléphone.
        #[arg(long)]
        partage: bool,
        /// Essai long : 20 s en vrac, puis 5 min par format (16/44, 24/96, 24/192).
        #[arg(long)]
        long: bool,
        /// Durée de la phase de pings seuls, en secondes (essai court : 60).
        #[arg(long)]
        pings_s: Option<f64>,
        #[arg(long)]
        vrac_s: Option<f64>,
        /// Formats cadencés, séparés par des virgules.
        #[arg(long, value_delimiter = ',')]
        formats: Option<Vec<String>>,
        #[arg(long)]
        flux_s: Option<f64>,
        /// Dossier du journal.
        #[arg(long, default_value = ".")]
        dossier: PathBuf,
        #[arg(long)]
        sans_portmapper: bool,
    },
    /// Agrège des journaux JSONL : tableau Markdown (sortie standard) et CSV.
    Agreger {
        #[arg(required = true)]
        fichiers: Vec<PathBuf>,
        #[arg(long)]
        csv: Option<PathBuf>,
        #[arg(long)]
        md: Option<PathBuf>,
    },
}

pub fn lire_ou_creer_cle(chemin: &Path) -> Result<SecretKey> {
    if chemin.exists() {
        let texte = std::fs::read_to_string(chemin)?;
        let octets = HEXLOWER
            .decode(texte.trim().as_bytes())
            .context("clé illisible (64 caractères hexadécimaux attendus)")?;
        let octets: [u8; 32] = octets
            .try_into()
            .map_err(|_| anyhow::anyhow!("clé de 32 octets attendue"))?;
        return Ok(SecretKey::from_bytes(&octets));
    }
    let cle = SecretKey::generate();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    writeln!(
        options.open(chemin)?,
        "{}",
        HEXLOWER.encode(&cle.to_bytes())
    )?;
    Ok(cle)
}

pub fn mode_relais(relais: &str) -> Result<RelayMode> {
    Ok(match relais {
        "n0" => RelayMode::Default,
        "aucun" => RelayMode::Disabled,
        url => RelayMode::custom([url.parse().context("URL de relais illisible")?]),
    })
}

/// Endpoint sans découverte publique : preset `Minimal`, donc ni pkarr ni DNS
/// d'n0 (cahier § 2.2). `local` : 127.0.0.1 seulement, sans portmapper (tests).
pub async fn ouvrir_endpoint(
    cle: SecretKey,
    alpns: Vec<Vec<u8>>,
    relais: RelayMode,
    local: bool,
    portmapper: bool,
) -> Result<Endpoint> {
    let mut b = Endpoint::builder(presets::Minimal)
        .secret_key(cle)
        .alpns(alpns)
        .relay_mode(relais);
    if local {
        b = b
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if local || !portmapper {
        b = b.portmapper_config(PortmapperConfig::Disabled);
    }
    b.bind()
        .await
        .map_err(|e| anyhow::anyhow!("ouverture de l'endpoint : {e:#}"))
}

fn secondes(s: f64) -> Duration {
    Duration::from_secs_f64(s.max(0.0))
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().commande {
        Commande::Serveur {
            campagne,
            jetons,
            autorises,
            relais,
            cle,
            site,
            box_,
            duree_max_h,
            sans_portmapper,
        } => {
            if !identifiant_valide(&campagne) {
                bail!("campagne : a-z, 0-9, - et _ seulement");
            }
            let jetons = serveur::lire_jetons(&std::fs::read_to_string(&jetons)?)?;
            let autorises = serveur::lire_autorises(&std::fs::read_to_string(&autorises)?)?;
            let mode = mode_relais(&relais)?;
            let avec_relais = !matches!(mode, RelayMode::Disabled);
            let endpoint = ouvrir_endpoint(
                lire_ou_creer_cle(&cle)?,
                vec![protocole::ALPN.to_vec()],
                mode,
                false,
                !sans_portmapper,
            )
            .await?;
            if avec_relais
                && tokio::time::timeout(Duration::from_secs(30), endpoint.online())
                    .await
                    .is_err()
            {
                eprintln!("attention : relais injoignable après 30 s, le ticket n'en porte pas");
            }
            let addr = endpoint.addr();
            let ticket = protocole::encoder_ticket(&addr)?;
            println!("Identifiant : {}", endpoint.id());
            for ip in addr.ip_addrs() {
                println!("Adresse    : {ip}");
            }
            for url in addr.relay_urls() {
                println!("Relais     : {url}");
            }
            println!("\nTicket :\n{ticket}\n");
            match qrcode::QrCode::new(ticket.as_bytes()) {
                Ok(qr) => println!(
                    "{}",
                    qr.render::<qrcode::render::unicode::Dense1x2>()
                        .quiet_zone(true)
                        .build()
                ),
                Err(e) => eprintln!("QR code impossible : {e}"),
            }
            let info = InfoServeur {
                site,
                box_,
                famille: serveur::familles(&endpoint),
            };
            let duree = serveur::duree_de_vie(Some(Duration::from_secs_f64(
                duree_max_h.clamp(0.0, 24.0) * 3600.0,
            )));
            eprintln!(
                "En attente des essais (arrêt automatique dans {:.1} h)…",
                duree.as_secs_f64() / 3600.0
            );
            serveur::servir(
                endpoint,
                serveur::ConfigServeur {
                    campagne,
                    jetons,
                    autorises,
                    info,
                },
                duree,
            )
            .await
        }
        Commande::Client {
            ticket,
            cle,
            identite,
            campagne,
            testeur,
            jeton,
            reseau,
            operateur,
            partage,
            long,
            pings_s,
            vrac_s,
            formats,
            flux_s,
            dossier,
            sans_portmapper,
        } => {
            let cle = lire_ou_creer_cle(&cle)?;
            if identite {
                println!("{}", cle.public());
                return Ok(());
            }
            // clap garantit leur présence hors `--identite`.
            let (
                Some(ticket),
                Some(campagne),
                Some(testeur),
                Some(jeton),
                Some(reseau),
                Some(operateur),
            ) = (ticket, campagne, testeur, jeton, reseau, operateur)
            else {
                bail!("arguments manquants");
            };
            if !identifiant_valide(&campagne) || !identifiant_valide(&testeur) {
                bail!("campagne et testeur : a-z, 0-9, - et _ seulement (pseudonyme, pas de nom)");
            }
            let mut scenario = if long {
                client::Scenario::long()
            } else {
                client::Scenario::court()
            };
            if let Some(s) = pings_s {
                scenario.pings = secondes(s);
            }
            if let Some(s) = vrac_s {
                scenario.vrac = secondes(s);
            }
            if let Some(f) = formats {
                for format in &f {
                    if protocole::debit_cible(format).is_none() {
                        bail!("format inconnu : {format} (16/44, 24/96, 24/192)");
                    }
                }
                scenario.formats = f;
            }
            if let Some(s) = flux_s {
                scenario.duree_flux = secondes(s);
            }
            let cible = protocole::decoder_ticket(&ticket)?;
            let relais: Vec<_> = cible.relay_urls().cloned().collect();
            let mode = if relais.is_empty() {
                RelayMode::Disabled
            } else {
                RelayMode::custom(relais)
            };
            let endpoint = ouvrir_endpoint(cle, vec![], mode, false, !sans_portmapper).await?;
            let params = client::ParamsClient {
                campagne,
                testeur,
                jeton,
                client: InfoClient {
                    os: std::env::consts::OS.to_string(),
                    plateforme: "cli".into(),
                    reseau,
                    operateur,
                    partage,
                },
                scenario,
            };
            let essai = client::jouer(&endpoint, cible, &params).await;
            endpoint.close().await;
            let chemin = journal::ajouter(&dossier, &essai)?;
            println!(
                "Essai {} : chemin {}, t_direct {} ms{}",
                essai.essai,
                essai.chemin_final.as_deref().unwrap_or("–"),
                essai
                    .t_direct_ms
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "–".into()),
                essai
                    .erreur
                    .as_deref()
                    .map(|e| format!(", erreur : {e}"))
                    .unwrap_or_default()
            );
            println!("Journal : {}", chemin.display());
            Ok(())
        }
        Commande::Agreger { fichiers, csv, md } => {
            let (essais, illisibles) = agreger::lire_fichiers(&fichiers)?;
            let table = agreger::par_cellule(&essais);
            let texte = agreger::markdown(&table, illisibles);
            print!("{texte}");
            if let Some(md) = md {
                std::fs::write(md, &texte)?;
            }
            if let Some(chemin) = csv {
                std::fs::write(chemin, agreger::csv(&table))?;
            }
            Ok(())
        }
    }
}
