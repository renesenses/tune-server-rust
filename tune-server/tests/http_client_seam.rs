//! Garde-fou : tout client HTTP passe par `tune_core::http::client`.
//!
//! ## Pourquoi
//!
//! La fonctionnalité `rustls` de reqwest confie la racine de confiance à
//! `rustls-platform-verifier`. Sur Android ce vérificateur réclame une
//! initialisation JNI (`Context` / `JavaVM`) que la build FFI
//! (`libtuneserver.so`) n'effectue jamais : la **première requête HTTPS avorte
//! le processus** sur `Expect rustls-platform-verifier to be initialized`.
//!
//! `tune_core::http::client::builder()` neutralise cela en imposant les racines
//! webpki via `use_preconfigured_tls`. Son commentaire de doc le disait déjà —
//! « Every client MUST be built via `builder` » — et sept sites l'ignoraient
//! quand même. Un commentaire n'arrête pas un geste ; un test qui échoue, si.
//!
//! Bénéfice secondaire : les clients nus n'ont **aucun délai d'attente total**.
//! Les accesseurs partagés en ont un (30 s, ou 600 s pour les téléchargements
//! longs), ce qui empêche un pair muet de bloquer une requête indéfiniment.
//!
//! ## Portée
//!
//! Seules les caisses qui dépendent de `tune-core` sont examinées — elles
//! seules peuvent atteindre le constructeur partagé. `tune-cli` et
//! `tune-widget` en sont volontairement absents : hors du graphe de `tune-core`
//! et hors Android.
//!
//! Cette règle était énoncée ici, mais la liste examinée, elle, était écrite en
//! dur : `tune-core/src` et `tune-server/src`. Or **quatre autres caisses
//! dépendent de `tune-core`** — `tune-ffi`, qui *est* la bibliothèque Android où
//! le processus avorte, et les trois greffons, dont deux tirent déjà `reqwest`
//! en direct. Elles étaient hors de portée sans que rien ne le dise. Aucune ne
//! construit de client aujourd'hui : c'est de la chance, pas un garde-fou.
//!
//! La liste n'est donc plus écrite : elle est **déduite de l'espace de travail**
//! à chaque exécution. Toute caisse déclarant `tune-core` entre dans le
//! périmètre le jour où elle est créée, sans que personne ait à y penser — c'est
//! précisément le geste qu'on oublie. Le test affiche les caisses couvertes.
//!
//! `tune-cli` reste dehors, et pour la bonne raison, vérifiée : son manifeste ne
//! déclare pas `tune-core`, donc son `reqwest::Client::new()` n'a aucun moyen
//! d'atteindre le constructeur partagé. Le jour où il le déclarera, il sera
//! examiné tout seul — et ce client-là devra changer.
//!
//! Si vous devez vraiment un client sur mesure, construisez-le depuis
//! `tune_core::http::client::builder()` et enchaînez vos réglages — c'est ce
//! que fait `routes/playlists.rs`, qui garde son `redirect(none)` et son délai
//! de 10 s.

use std::path::{Path, PathBuf};

/// Constructions interdites : elles produisent un client au TLS par défaut.
const FORBIDDEN: [&str; 6] = [
    "reqwest::get(",
    "reqwest::blocking::get(",
    "reqwest::Client::new()",
    "reqwest::blocking::Client::new()",
    "reqwest::Client::builder()",
    "reqwest::blocking::Client::builder()",
];

/// Le seul fichier autorisé à nommer ces constructions : celui qui les
/// enveloppe. Chemin relatif à la racine de l'espace de travail.
const ALLOWED: &str = "tune-core/src/http/client.rs";

/// Caisses dont la présence dans le périmètre est garantie. Si la déduction
/// cesse de les trouver — membres reformatés dans le manifeste racine, caisse
/// renommée — c'est que le test a cessé de lire l'espace de travail, et il doit
/// le dire au lieu d'examiner le vide en affichant vert.
const EXPECTED: [&str; 3] = ["tune-core", "tune-server", "tune-ffi"];

/// Membres de l'espace de travail, lus dans le manifeste racine.
///
/// Analyse volontairement littérale : le contenu entre `members = [` et le `]`
/// qui suit. Pas de dépendance TOML pour un test, et une erreur de lecture se
/// solde par une liste vide — que [`EXPECTED`] transforme en échec bruyant.
fn workspace_members(root: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .expect("le manifeste racine de l'espace de travail est lisible");
    let after = match manifest.split_once("members") {
        Some((_, rest)) => rest,
        None => return Vec::new(),
    };
    let inside = match after.split_once('[') {
        Some((_, rest)) => match rest.split_once(']') {
            Some((list, _)) => list,
            None => return Vec::new(),
        },
        None => return Vec::new(),
    };
    inside
        .split(',')
        .map(|m| m.trim().trim_matches('"').trim())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect()
}

/// La caisse déclare-t-elle `tune-core` parmi ses dépendances ?
///
/// `tune-core` elle-même répond oui : c'est la caisse qui détient le
/// constructeur partagé, et donc la première à devoir s'y tenir.
fn depends_on_tune_core(root: &Path, member: &str) -> bool {
    if member == "tune-core" {
        return true;
    }
    let manifest = match std::fs::read_to_string(root.join(member).join("Cargo.toml")) {
        Ok(m) => m,
        Err(e) => panic!("manifeste de {member} illisible : {e}"),
    };
    // Une déclaration de dépendance commence la ligne ; `name = "tune-core"` ou
    // une mention en commentaire ne doivent pas faire entrer une caisse dans le
    // périmètre par accident.
    manifest
        .lines()
        .any(|l| l.trim_start().starts_with("tune-core"))
}

/// Répertoires de sources examinés, déduits de l'espace de travail.
fn scanned_crates(root: &Path) -> Vec<String> {
    let mut crates: Vec<String> = workspace_members(root)
        .into_iter()
        .filter(|m| depends_on_tune_core(root, m))
        .collect();
    crates.sort();
    crates
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <racine>/tune-server
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tune-server a un parent")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => panic!("lecture de {} impossible : {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn aucun_client_http_ne_contourne_le_constructeur_partage() {
    let root = workspace_root();
    let allowed = root.join(ALLOWED);
    assert!(
        allowed.exists(),
        "{} a disparu ou changé de place — ce test ne protège plus rien, \
         corrigez ALLOWED",
        allowed.display()
    );

    let crates = scanned_crates(&root);
    for expected in EXPECTED {
        assert!(
            crates.iter().any(|c| c == expected),
            "{expected} n'est plus dans le périmètre déduit ({crates:?}) — la \
             lecture de l'espace de travail a cessé de fonctionner, et le test \
             s'apprêtait à examiner moins que prévu en affichant vert"
        );
    }
    // Affiché avec `--nocapture` : la couverture doit être lisible, pas devinée.
    println!("caisses examinées : {}", crates.join(", "));

    let mut files = Vec::new();
    for krate in &crates {
        let d = root.join(krate).join("src");
        assert!(
            d.is_dir(),
            "répertoire examiné introuvable : {}",
            d.display()
        );
        rust_files(&d, &mut files);
    }
    assert!(
        files.len() > 100,
        "moisson suspecte : {} fichiers",
        files.len()
    );

    let mut faults = Vec::new();
    for file in &files {
        if *file == allowed {
            continue;
        }
        let src = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("lecture de {} impossible : {e}", file.display()));
        for (n, line) in src.lines().enumerate() {
            for pattern in FORBIDDEN {
                if line.contains(pattern) {
                    let rel = file.strip_prefix(&root).unwrap_or(file);
                    faults.push(format!("{}:{} → {}", rel.display(), n + 1, pattern));
                }
            }
        }
    }

    assert!(
        faults.is_empty(),
        "client(s) HTTP construits hors de `tune_core::http::client` :\n  {}\n\n\
         Ces clients utilisent le vérificateur TLS de plateforme, que la build \
         FFI Android n'initialise pas : la première requête HTTPS avorte le \
         processus. Ils n'ont pas non plus de délai d'attente.\n\n\
         Remplacez par `tune_core::http::client::shared()` (30 s), \
         `long_timeout()` (600 s, gros téléchargements), ou construisez depuis \
         `builder()` si vous avez besoin de réglages particuliers.",
        faults.join("\n  ")
    );
}
