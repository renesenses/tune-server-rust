//! Témoin de la contre-épreuve #2305/#2306 — à exécuter en profil **release**.
//!
//! `cargo test --release` ne prouve RIEN ici : Cargo ignore le réglage `panic`
//! du profil pour les cibles de test et force toujours l'unwind (JP Robbe). Un
//! test resterait donc vert avec `panic = "abort"` dans `Cargo.toml`, c'est-à-dire
//! exactement dans la configuration qui tue le serveur en production.
//!
//! Cet exemple est un **vrai binaire** construit sous le profil release : il
//! porte donc le réglage réel. `scripts/verifier-panique-release.sh` le lance
//! comme processus enfant et observe ce qu'il en sort.
//!
//! Deux modes :
//!
//! - `decodage <fichier>` — passe un fichier malformé au décodeur de production.
//!   Sous `abort`, le processus meurt (code 134/101, pas de sortie). Sous
//!   `unwind`, le `catch_unwind` de `decode.rs` reprend la main, journalise
//!   `symphonia_decoder_panic` et rend une erreur : le processus SURVIT.
//!
//! - `interception` — panique DANS un `catch_unwind`, comme le fait
//!   `decode.rs`. Sous `abort`, le processus meurt : le `catch_unwind` est
//!   décoratif, ce qui est tout le sujet de #2305. Sous `unwind`, il reprend la
//!   main et le processus survit. C'est la propriété du PROFIL qu'on mesure ici,
//!   celle dont dépendent les `catch_unwind` déjà présents dans `decode.rs`
//!   (lignes 439 et 485), dans l'installateur de mise à jour, et le hook qui
//!   écrit `tune-crash.log`.
//!
//! - `panique` — panique volontairement dans une fonction au nom reconnaissable.
//!   Le backtrace doit nommer `temoin_panique_release::fonction_temoin_de_crash`.
//!   Avec `strip = true` (alias de `"symbols"`), la table des symboles disparaît
//!   et le rapport désigne des fonctions voisines encore exportées — les `onig_*`
//!   de la bibliothèque de regex — donc un coupable faux (#2306).

/// Nom volontairement distinctif : c'est LUI qu'on cherche dans le backtrace.
#[inline(never)]
fn fonction_temoin_de_crash() -> ! {
    panic!("panique volontaire du temoin (#2306)");
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("decodage") => {
            let fichier = args.next().unwrap_or_else(|| {
                eprintln!("usage: temoin_panique_release decodage <fichier>");
                std::process::exit(2);
            });
            match tune_core::audio::decode::decode_to_pcm(&fichier, None, None, 0.0, 0.0) {
                Ok(audio) => {
                    println!("DECODAGE_OK trames={}", audio.samples_i32.len());
                }
                Err(e) => {
                    // C'est le cas attendu sur un fichier malformé : une ERREUR,
                    // pas un cadavre.
                    println!("DECODAGE_ERREUR {e}");
                }
            }
            // Ne rien afficher d'autre : la présence de cette ligne EST la preuve
            // que le processus a survécu au décodage.
            println!("PROCESSUS_VIVANT");
        }
        Some("interception") => {
            // Exactement la forme employée par `decode.rs` : un `catch_unwind`
            // autour d'un appel qui panique. Sous `panic = "abort"`, ce bloc
            // n'existe pour ainsi dire pas — le processus meurt ici.
            let resultat = std::panic::catch_unwind(|| -> i32 {
                panic!("panique interceptee du temoin (#2305)");
            });
            match resultat {
                Ok(_) => println!("INTERCEPTION_INATTENDUE"),
                Err(_) => println!("INTERCEPTION_OK"),
            }
            println!("PROCESSUS_VIVANT");
        }
        Some("panique") => fonction_temoin_de_crash(),
        _ => {
            eprintln!(
                "usage: temoin_panique_release <decodage <fichier> | interception | panique>"
            );
            std::process::exit(2);
        }
    }
}
