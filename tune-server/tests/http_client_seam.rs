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

/// Répertoires de sources examinés, relatifs à la racine de l'espace de travail.
const SCANNED: [&str; 2] = ["tune-core/src", "tune-server/src"];

/// Le seul fichier autorisé à nommer ces constructions : celui qui les
/// enveloppe. Chemin relatif à la racine de l'espace de travail.
const ALLOWED: &str = "tune-core/src/http/client.rs";

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

    let mut files = Vec::new();
    for dir in SCANNED {
        let d = root.join(dir);
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
