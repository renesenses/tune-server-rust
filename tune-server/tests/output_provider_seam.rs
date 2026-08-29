//! Garde-fou : la couture `OutputProvider` doit rester APPELÉE.
//!
//! `bootstrap::run_with` transmet `RunOptions::output_providers` à
//! `discovery_setup::spawn_output_providers`. C'est le seul point d'entrée des
//! caisses de sortie hors-arbre — des binaires composeurs qui ne peuvent pas
//! apparaître dans le graphe de dépendances public, le premier étant le dépôt
//! privé `tune-diretta`.
//!
//! ## Pourquoi ce fichier existe
//!
//! Cette couture a déjà été supprimée une fois : #1510, entre 0.9.69 et
//! 0.9.70, avec ce motif — « doublon mort, rien ne l'appelait, rien ne le
//! testait ». Le raisonnement était **exact**, et la conclusion **fausse** :
//! l'intégration partenaire est restée cassée deux versions, et son seul
//! recours a été de rester épinglée sur 0.9.68.
//!
//! Un commentaire de documentation ne suffit pas — il y en avait un, il
//! nommait `tune-diretta`, et il n'a pas arrêté le geste. Seul un test qui
//! ÉCHOUE peut le faire.
//!
//! ## Ce que ce test ne prétend pas faire
//!
//! Il ne vérifie pas le comportement du polling — cela demanderait un
//! `AppState` complet, donc un test d'intégration lourd pour une garantie
//! qu'un simple appel suffit à donner. Il vérifie **l'existence de l'appel**,
//! c'est-à-dire précisément ce dont l'absence a fait qualifier ce code de
//! mort.
//!
//! Si vous supprimez cet appel volontairement, supprimez ce test dans le même
//! commit — et prévenez les consommateurs hors-arbre avant, pas après.

use std::path::Path;

/// Le fichier qui doit porter l'appel, et l'appel lui-même.
const BOOTSTRAP: &str = "src/bootstrap.rs";
const REQUIRED_CALL: &str = "spawn_output_providers";

fn bootstrap_source() -> String {
    // CARGO_MANIFEST_DIR = tune-server/ quel que soit le répertoire courant.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(BOOTSTRAP);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("lecture de {} impossible : {e}", path.display()))
}

#[test]
fn bootstrap_appelle_encore_spawn_output_providers() {
    let src = bootstrap_source();
    assert!(
        src.contains(REQUIRED_CALL),
        "`{REQUIRED_CALL}` n'est plus appelé depuis {BOOTSTRAP}.\n\
         \n\
         C'est la couture des sorties hors-arbre (tune-diretta et suivants).\n\
         Sans cet appel, un binaire composeur démarre sans jamais interroger\n\
         ses fournisseurs : pas de découverte réseau dynamique, et pas de\n\
         revérification périodique des habilitations.\n\
         \n\
         Déjà supprimée une fois par #1510 au motif qu'elle était morte.\n\
         Elle ne l'est pas : elle est appelée depuis un dépôt privé."
    );
}

#[test]
fn run_options_expose_toujours_output_providers() {
    // La structure est l'autre moitié du contrat : un binaire composeur écrit
    // `RunOptions { output_providers, ..Default::default() }`. Renommer ou
    // retirer ce champ casse ses appels à la compilation, sans qu'aucun test
    // de ce dépôt ne le signale.
    let opts = tune_server::bootstrap::RunOptions::default();
    assert!(
        opts.output_providers.is_empty(),
        "le binaire standard ne doit embarquer aucun fournisseur par défaut"
    );
}

#[test]
fn le_binaire_standard_reste_inchange() {
    // `run()` doit continuer d'exister pour les appelants historiques : il
    // délègue à `run_with` avec des options vides. Si cette fonction
    // disparaît, ce sont les appelants internes qui cassent — un défaut
    // différent, mais dans le même fichier.
    let src = bootstrap_source();
    assert!(
        src.contains("pub async fn run(") && src.contains("pub async fn run_with("),
        "bootstrap doit exposer À LA FOIS `run` (compatibilité) et `run_with` (couture)"
    );
}
