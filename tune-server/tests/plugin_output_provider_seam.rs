//! Garde-fou : la couture `OutputProvider` doit rester APPELÉE — côté PLUGINS.
//!
//! Sœur de `output_provider_seam.rs`, qui garde le chemin HORS-ARBRE
//! (`bootstrap::run_with` → `RunOptions::output_providers`). Celle-ci garde le
//! chemin PLUGIN : `PluginContext::register_output_provider` collecte, et
//! `plugins::install` transmet à `spawn_output_providers`.
//!
//! ## Pourquoi une SECONDE garde
//!
//! Les deux chemins aboutissent à la même fonction, mais ils partent de deux
//! fichiers différents. Supprimer l'appel dans `plugins.rs` laisserait
//! `output_provider_seam.rs` VERT — le binaire composeur continuerait de
//! fonctionner, et seuls les plugins perdraient leur découverte, EN SILENCE :
//! `register_output_provider` compilerait toujours, ne rendrait aucune erreur,
//! et n'aurait plus aucun effet. Un puits.
//!
//! C'est exactement le mode d'échec de #1510, où la couture hors-arbre a été
//! supprimée comme « morte » parce que rien dans l'arbre ne l'appelait. Le
//! raisonnement était exact, la conclusion fausse, et l'intégration partenaire
//! est restée cassée deux versions.
//!
//! ## Ce que ce test ne prétend pas faire
//!
//! Il ne vérifie pas le comportement du polling — cela demanderait un
//! `AppState` complet. La preuve fonctionnelle vit dans `tune-core`
//! (`plugin_sdk` : un plugin qui enregistre un fournisseur le retrouve dans
//! `take_registrations`). Ici on garde le CÂBLAGE, c'est-à-dire précisément ce
//! dont l'absence ne se voit pas.
//!
//! Si vous supprimez cet appel volontairement, supprimez ce test dans le même
//! commit — et prévenez les plugins concernés avant, pas après.

use std::path::Path;

const PLUGINS: &str = "src/plugins.rs";
const REQUIRED_CALL: &str = "spawn_output_providers";

fn plugins_source() -> String {
    // CARGO_MANIFEST_DIR = tune-server/ quel que soit le répertoire courant.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(PLUGINS);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("lecture de {} impossible : {e}", path.display()))
}

#[test]
fn install_transmet_encore_les_fournisseurs_de_plugins() {
    let src = plugins_source();
    assert!(
        src.contains(REQUIRED_CALL),
        "`{REQUIRED_CALL}` n'est plus appelé depuis {PLUGINS}.\n\
         \n\
         C'est la couture des fournisseurs de sorties déclarés par un PLUGIN\n\
         (`PluginContext::register_output_provider`). Sans cet appel, un plugin\n\
         qui découvre ses sorties sur le réseau enregistre dans le vide : la\n\
         méthode compile, ne rend aucune erreur, et n'a plus aucun effet.\n\
         \n\
         `output_provider_seam.rs` resterait VERT — il garde l'autre chemin,\n\
         celui des binaires composeurs hors-arbre."
    );
}

#[test]
fn install_draine_bien_le_champ_output_providers() {
    // La garde ci-dessus verrait encore un appel qui ne recevrait plus rien :
    // `spawn_output_providers` peut rester présent pour une autre raison.
    // Celle-ci vérifie que le champ est bien extrait de la structure drainée.
    let src = plugins_source();
    assert!(
        src.contains("output_providers,"),
        "`plugins::install` ne déstructure plus `output_providers` : les\n\
         fournisseurs déclarés par un plugin ne sont plus lus, même si\n\
         `spawn_output_providers` reste appelé plus loin dans le fichier."
    );
}
