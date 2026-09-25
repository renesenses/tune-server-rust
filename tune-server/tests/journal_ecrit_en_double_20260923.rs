//! Une ligne émise arrive **une fois** dans le journal — même quand la console
//! du processus a été branchée sur ce même fichier.
//!
//! ## Le défaut mesuré
//!
//! Journal joint au rapport de Cyrille Moutia (macOS, 0.9.163) : **1 002
//! lignes pour 585 uniques**, chacune en double, consécutivement et à
//! l'identique. Un seul `tune-server starting (pid 14418)`, une seule séquence
//! de démarrage, les lignes de migration de base dédoublées elles aussi — ce
//! n'étaient donc pas deux serveurs.
//!
//! La cause est le lanceur du `.app` macOS
//! (`.github/workflows/release.yml:915-916`) :
//!
//! ```text
//! tune-server >> ~/Library/Logs/tune-server.log 2>&1 &
//! ```
//!
//! …soit la sortie du processus branchée sur le fichier **que la couche fichier
//! de l'abonné `tracing` ouvre de son côté**. Deux descripteurs, un inode,
//! chaque ligne deux fois.
//!
//! La couche console écrit sur la SORTIE STANDARD — `io::stdout` est
//! l'écrivain par défaut de `fmt::layer()` (`tracing-subscriber-0.3.23`,
//! `fmt/fmt_layer.rs:749`), quoi qu'ait laissé croire le nom `stderr_layer`
//! que portait la variable. C'est donc le descripteur 1 qui est en cause, et
//! `2>&1` du lanceur amène le 2 avec lui.
//!
//! ## Pourquoi ça ne se répare pas « à l'œil »
//!
//! Le rapport de bogue intégré n'embarque que les 200 dernières lignes
//! (`BUG_REPORT_LOG_LINES`, `routes/system/diagnostics.rs`). Dédoublées, elles
//! portent 100 lignes d'information : chaque diagnostic venant d'un Mac nous
//! arrive de moitié, sans que rien ne le signale.
//!
//! ## Ce que ce banc prouve, et comment
//!
//! La propriété, pas la plomberie. On ne regarde pas *quelles couches sont
//! posées* : on reconstitue le lancement macOS dans un **sous-processus** —
//! sortie standard ET flux d'erreur ouverts en ajout sur le fichier de journal,
//! comme `>> … 2>&1` — on lui fait installer le vrai abonné
//! ([`tune_server::bootstrap::installer_le_journal`]) et émettre une ligne
//! témoin, puis on la **compte** dans le fichier.
//!
//! Le second témoin garde le cas légitime : lancé dans un terminal, le serveur
//! doit écrire sur la console **et** dans le fichier. Un correctif qui
//! supprimerait la couche console le ferait rougir.
//!
//! Le sous-processus est indispensable : rediriger le descripteur 1 du binaire
//! de test empoisonnerait tous les autres tests du même processus, et
//! `tracing_subscriber::…::init()` ne s'installe qu'une fois par processus.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// La ligne témoin. Choisie improbable : on la compte par `contains`, et rien
/// d'autre dans le journal ne doit pouvoir la porter.
const SENTINELLE: &str = "sentinelle_journal_double_20260923_une_seule_fois";

/// Dit à l'enfant qu'il est l'enfant. Absent, le test ignoré ne fait rien.
const DRAPEAU_ENFANT: &str = "TUNE_TEST_JOURNAL_ENFANT";

/// Le côté « serveur » du banc : installe le vrai abonné et émet la sentinelle.
///
/// `#[ignore]` : ce n'est pas un témoin, c'est le sous-processus que les deux
/// témoins ci-dessous lancent avec `--ignored --exact`.
#[test]
#[ignore = "sous-processus du banc de non-duplication ; lancé par les témoins"]
fn enfant_emet_la_sentinelle() {
    if std::env::var_os(DRAPEAU_ENFANT).is_none() {
        // Lancé à la main hors du banc : ne rien écrire nulle part.
        return;
    }
    // `TUNE_LOG_FILE`, posé par le parent, décide du chemin du journal
    // (`config::default_log_file_path`). Le reste est le code de production.
    tune_server::bootstrap::installer_le_journal("info");
    // `target:` explicite, et non celui du binaire de test : le filtre du
    // serveur ne laisse passer que le PRÉFIXE `tune` (`poser_les_directives`).
    // Sous le nom de cette cible de test, la sentinelle serait filtrée et le
    // banc mesurerait un fichier vide — donc vert, toujours.
    tracing::info!(target: "tune_server::journal_double", "{SENTINELLE}");
}

/// Lance le sous-processus et rend ce qu'il a écrit sur sa **console**.
///
/// `console` décide de la forme du lancement : `Console::MemeFichier`
/// reconstitue `>> journal 2>&1` (les deux descripteurs sur le fichier),
/// `Console::Tube` reconstitue un terminal (une console ailleurs que dans le
/// journal). Rend le texte de la console dans le second cas, la chaîne vide
/// dans le premier.
fn lancer_enfant(journal: &Path, console: Console) -> String {
    let moi = std::env::current_exe().expect("chemin du binaire de test");
    let mut commande = Command::new(moi);
    commande
        .args([
            "enfant_emet_la_sentinelle",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DRAPEAU_ENFANT, "1")
        .env("TUNE_LOG_FILE", journal)
        // `filtre_du_journal` lit `RUST_LOG` en dernier mot : un `RUST_LOG`
        // hérité de la session pourrait étouffer la sentinelle.
        .env_remove("RUST_LOG")
        .env_remove("TUNE_LOG_LEVEL");

    match console {
        Console::MemeFichier => {
            // Exactement ce que fait `>> fichier 2>&1` : deux ouvertures en
            // ajout, installées sur les descripteurs 1 et 2 de l'enfant.
            let ajout = || {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(journal)
                    .expect("ouverture du journal en ajout")
            };
            commande
                .stdout(Stdio::from(ajout()))
                .stderr(Stdio::from(ajout()));
        }
        Console::Tube => {
            commande.stdout(Stdio::piped()).stderr(Stdio::null());
        }
    }

    let mut enfant = commande.spawn().expect("lancement du sous-processus");
    let mut console_lue = String::new();
    if matches!(console, Console::Tube) {
        enfant
            .stdout
            .take()
            .expect("tube de sortie")
            .read_to_string(&mut console_lue)
            .expect("lecture du tube de sortie");
    }
    let etat = enfant.wait().expect("attente du sous-processus");
    assert!(
        etat.success(),
        "le sous-processus a échoué ({etat}) ; console : {console_lue}"
    );
    console_lue
}

/// La forme du lancement à reconstituer.
#[derive(Clone, Copy)]
enum Console {
    /// Le `.app` macOS : `>> journal 2>&1`.
    MemeFichier,
    /// Un terminal : la console va ailleurs que dans le journal.
    Tube,
}

fn compter(texte: &str) -> usize {
    texte.lines().filter(|l| l.contains(SENTINELLE)).count()
}

fn lire(journal: &Path) -> String {
    std::fs::read_to_string(journal).unwrap_or_default()
}

/// Le lancement macOS reconstitué : `>> journal 2>&1`.
///
/// La sentinelle doit figurer **une seule fois** dans le fichier. Avant le
/// correctif elle y figurait deux fois — une par la couche fichier, une par la
/// couche console dont la sortie pointait sur le même inode.
#[test]
fn console_branchee_sur_le_journal_n_ecrit_qu_une_copie() {
    let d = tune_core::test_scratch::scratch_dir("journal-double-meme-fichier");
    let journal = d.path().join("tune-server.log");

    lancer_enfant(&journal, Console::MemeFichier);

    let contenu = lire(&journal);
    let vues = compter(&contenu);
    assert_eq!(
        vues, 1,
        "console branchée sur le journal : la ligne témoin devrait y figurer \
         UNE fois, vue {vues} fois. Chaque doublon coûte une ligne utile au \
         rapport de bogue, qui n'en embarque que 200.\n--- journal ---\n{contenu}"
    );
}

/// Le cas légitime : lancé dans un terminal, le serveur parle aux DEUX.
///
/// Ce témoin est là pour interdire le « correctif » paresseux — retirer la
/// couche console. La console de l'enfant est un tube, donc un tout autre objet
/// que le fichier : elle doit rester servie.
#[test]
fn console_hors_du_journal_alimente_les_deux_destinations() {
    let d = tune_core::test_scratch::scratch_dir("journal-double-terminal");
    let journal = d.path().join("tune-server.log");

    let console = lancer_enfant(&journal, Console::Tube);

    let contenu = lire(&journal);
    assert_eq!(
        compter(&contenu),
        1,
        "console hors du journal : le fichier doit porter la ligne une fois.\
         \n--- journal ---\n{contenu}"
    );
    assert_eq!(
        compter(&console),
        1,
        "console hors du journal : elle doit porter la ligne une fois — qui \
         lance ./tune-server dans un terminal veut la voir passer.\
         \n--- console ---\n{console}"
    );
}
