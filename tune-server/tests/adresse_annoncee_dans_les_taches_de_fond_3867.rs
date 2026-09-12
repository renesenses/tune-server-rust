//! L'adresse ANNONCÉE gouverne les URL que les tâches de fond remettent à un
//! tiers (#3867).
//!
//! # Le défaut
//!
//! `tune-server/src/background.rs` appelait `get_local_ip()` brut à trois
//! endroits, à deux fonctions d'un annonceur SSDP qui, lui, respecte
//! `advertised_ip`. Sur un hôte multi-domicilié — VPN, NordVPN, plusieurs
//! cartes réseau — Tune s'annonçait donc à la bonne adresse et distribuait en
//! même temps des URL portant l'autre interface : le lecteur recevait une
//! adresse qu'il ne savait pas joindre.
//!
//! # Les trois sites, et pourquoi UN seul reste sur l'autodétection
//!
//! - le proxy Deezer : son URL est consommée par `DeezerService::get_track_url`
//!   (`tune-core/src/streaming/deezer.rs`), qui la rend comme `StreamUrl`. Sur
//!   une zone réseau, c'est le renderer, depuis une AUTRE machine, qui va la
//!   chercher. → corrigé ;
//! - le serveur SlimProto : l'adresse arrive dans `CliState::local_ip`, que
//!   `cli_server.rs` recopie dans le `player_ip:<ip>:3483` remis à un
//!   contrôleur distant (Squeeze-LX, Home Assistant). → corrigé ;
//! - les notifications de bureau : la base ne quitte jamais la machine. Son
//!   unique consommateur est `download_icon` (`tune-core/src/notifications.rs`)
//!   qui va chercher la pochette SUR CE SERVEUR, depuis CE processus, pour
//!   l'afficher localement. `advertised_ip` peut valoir une adresse NAT ou
//!   publique que l'hôte ne sait pas joindre lui-même. → LAISSÉ, délibérément.
//!
//! Le quatrième site du même fichier, `SlimProtoState::server_ip`, est lui
//! aussi laissé : son unique consommateur est `sonder_qui_tient_le_port`, qui
//! sonde le 3483 SUR CETTE MACHINE quand le bind échoue (#2938, #2349).
//!
//! # Ce que ce témoin mesure
//!
//! Deux étages, parce qu'un seul ne suffit pas :
//!
//! 1. les deux décisions corrigées sont APPELÉES (`base_du_proxy_deezer`,
//!    `adresse_slimproto_remise_aux_controleurs`), et non recopiées en ligne —
//!    un correctif « écrit mais pas branché » rougit ici ;
//! 2. ces décisions rendent bien l'adresse annoncée — remettre `get_local_ip()`
//!    dans leur corps rougit là.
//!
//! Le troisième étage garde l'ABSTENTION : la sonde de bind doit rester sur
//! l'autodétection, pour que personne ne l'uniformise « pour faire joli ».
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` déclarée dans `tune-server/Cargo.toml`. Sans elle,
//! il n'est jamais exécuté.

use tune_server::config::TuneConfig;

/// RFC 5737 (TEST-NET-3) : jamais l'adresse d'une carte réelle, donc jamais
/// celle que l'autodétection pourrait rendre par hasard sur le coureur.
const ADRESSE_ANNONCEE: &str = "203.0.113.7";
const PORT: u16 = 8899;

const BACKGROUND: &str = include_str!("../src/background.rs");

fn config_avec_adresse_annoncee() -> TuneConfig {
    TuneConfig {
        advertised_ip: Some(ADRESSE_ANNONCEE.to_string()),
        port: PORT,
        ..TuneConfig::default()
    }
}

/// Le corps d'une fonction de `background.rs`, de son en-tête à la première
/// accolade fermante en colonne 0.
fn corps<'a>(source: &'a str, entete: &str) -> &'a str {
    let debut = source.find(entete).unwrap_or_else(|| {
        panic!("`{entete}` introuvable dans tune-server/src/background.rs — le temoin ne mesure plus rien")
    });
    let reste = &source[debut + 1..];
    let fin = reste.find("\n}\n").unwrap_or_else(|| {
        panic!("fin de `{entete}` introuvable dans tune-server/src/background.rs")
    });
    &reste[..fin]
}

// --- Étage 1 : la décision est BRANCHÉE -------------------------------------

#[test]
fn le_proxy_deezer_est_cable_sur_l_adresse_annoncee() {
    let site = corps(BACKGROUND, "\nasync fn configure_deezer_proxy(");
    assert!(
        !site.contains("get_local_ip"),
        "configure_deezer_proxy lit encore l'autodetection brute `get_local_ip()` : \
         l'URL du proxy remise aux lecteurs du reseau ignore le reglage d'adresse \
         annoncee, alors que l'annonceur SSDP du meme fichier le respecte (#3867)"
    );
    assert!(
        site.contains("base_du_proxy_deezer(config)"),
        "configure_deezer_proxy doit APPELER `base_du_proxy_deezer` — la decision que \
         mesure `l_url_du_proxy_deezer_porte_l_adresse_annoncee` — et non rebatir \
         l'URL en ligne, sans quoi le temoin de comportement garde une fonction que \
         la production n'emprunte pas"
    );
}

#[test]
fn le_serveur_slimproto_remet_l_adresse_annoncee_a_ses_controleurs() {
    let site = corps(BACKGROUND, "\nfn spawn_slimproto_server(");
    assert!(
        site.contains("let adresse_remise = adresse_slimproto_remise_aux_controleurs(config);"),
        "spawn_slimproto_server doit APPELER `adresse_slimproto_remise_aux_controleurs` \
         pour l'adresse qu'il REMET a un tiers (#3867)"
    );
    assert!(
        site.contains("local_ip: adresse_remise,"),
        "le `CliState` du port 9090 doit recevoir l'adresse ANNONCEE : c'est elle que \
         `tune-core/src/slimproto/cli_server.rs` recopie dans le `player_ip:<ip>:3483` \
         de sa reponse d'etat, lue par un controleur distant (Squeeze-LX, Home \
         Assistant)"
    );
}

// --- Étage 2 : la décision rend bien l'adresse annoncée ---------------------

#[test]
fn l_url_du_proxy_deezer_porte_l_adresse_annoncee() {
    let url = tune_server::background::base_du_proxy_deezer(&config_avec_adresse_annoncee());
    assert_eq!(
        url,
        format!("http://{ADRESSE_ANNONCEE}:{PORT}/deezer-proxy"),
        "l'URL du proxy Deezer doit porter l'adresse ANNONCEE : `get_track_url` en \
         derive le `StreamUrl` que le renderer DLNA va chercher depuis une autre \
         machine (#3867)"
    );
}

#[test]
fn l_adresse_slimproto_remise_est_l_adresse_annoncee() {
    let adresse = tune_server::background::adresse_slimproto_remise_aux_controleurs(
        &config_avec_adresse_annoncee(),
    );
    assert_eq!(
        adresse, ADRESSE_ANNONCEE,
        "l'adresse remise au serveur CLI SlimProto doit etre l'adresse ANNONCEE : \
         elle part sur le reseau, vers un controleur tiers (#3867)"
    );
}

/// Non-régression : sans réglage, rien ne change pour personne.
#[test]
fn sans_adresse_annoncee_l_autodetection_reste_en_place() {
    let config = TuneConfig {
        port: PORT,
        ..TuneConfig::default()
    };
    let url = tune_server::background::base_du_proxy_deezer(&config);
    assert!(
        !url.contains(ADRESSE_ANNONCEE),
        "aucune adresse annoncee n'est posee : l'URL ne peut pas en porter une"
    );
    assert!(
        url.starts_with("http://") && url.ends_with(&format!(":{PORT}/deezer-proxy")),
        "sans reglage, l'URL garde exactement sa forme d'avant — mesuree : {url}"
    );
}

// --- Étage 3 : l'ABSTENTION est gardée, elle aussi --------------------------

#[test]
fn la_sonde_de_bind_slimproto_reste_sur_l_autodetection() {
    let site = corps(BACKGROUND, "\nfn spawn_slimproto_server(");
    assert!(
        site.contains("let ip_sondee_localement = tune_core::discovery::ssdp::get_local_ip()"),
        "`SlimProtoState::server_ip` doit rester sur l'autodetection : son unique \
         consommateur est `sonder_qui_tient_le_port`, qui tente une connexion TCP pour \
         nommer qui tient le 3483 SUR CETTE MACHINE (#2938, #2349). L'adresse annoncee \
         y ferait sonder une AUTRE machine, et nommerait « un autre serveur ecoute » un \
         port pourtant libre"
    );
}

#[test]
fn les_notifications_de_bureau_restent_sur_l_autodetection() {
    let site = corps(BACKGROUND, "\nfn spawn_desktop_notifications(");
    assert!(
        site.contains("get_local_ip"),
        "la base des notifications de bureau doit rester sur l'autodetection : elle ne \
         quitte jamais la machine (`download_icon` va chercher la pochette sur CE \
         serveur, depuis CE processus). `advertised_ip` peut valoir une adresse NAT ou \
         publique que l'hote ne sait pas joindre lui-meme — y basculer casserait une \
         pochette qui s'affiche aujourd'hui"
    );
}
