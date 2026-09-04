use std::fs;
use std::path::Path;

/// Le corps de `patch_zone`, des `async fn patch_zone(` jusqu'au `\n}\n`
/// qui le ferme. Découpé sur la source plutôt que sur des numéros de ligne,
/// qui dérivent à chaque édition.
fn corps_du_handler() -> String {
    let source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes/zones.rs"))
            .expect("lecture de zones.rs");
    let debut = source
        .find("async fn patch_zone(")
        .expect("`patch_zone` a été renommé — ce garde-fou ne garde plus rien");
    let reste = &source[debut..];
    let fin = reste
        .find("\n}\n")
        .expect("fin de `patch_zone` introuvable");
    reste[..fin].to_string()
}

#[test]
fn no_bare_500_survives_in_patch_zone() {
    let corps = corps_du_handler();
    assert!(
        !corps.contains("(StatusCode::INTERNAL_SERVER_ERROR, e)"),
        "un `return (StatusCode::INTERNAL_SERVER_ERROR, e)` nu subsiste dans \
         `patch_zone` : la cause partira sans laisser de trace, et un 500 \
         signalé par un testeur sera de nouveau impossible à instruire \
         (#1964). Utiliser la macro `ecrire!`, qui journalise."
    );
}

#[test]
fn every_write_goes_through_the_logging_macro() {
    let corps = corps_du_handler();
    let ecritures = corps.matches("ecrire!(").count();
    // 22 à la rédaction. Le seuil protège contre l'inverse du test
    // précédent : quelqu'un qui remplacerait les blocs par des `.ok()`
    // silencieux passerait le premier test et perdrait tout autant les
    // causes.
    assert!(
        ecritures >= 20,
        "seulement {ecritures} appels à `ecrire!` dans `patch_zone` — \
         des écritures ont-elles été retirées du chemin journalisé ?"
    );
}

/// Les valeurs jugeables par la route doivent l'être AVANT la première
/// écriture. Un PATCH à moitié appliqué est pire qu'un PATCH refusé : la
/// zone se retrouve dans un état que l'utilisateur n'a pas demandé.
#[test]
fn value_checks_come_before_any_write() {
    let corps = corps_du_handler();
    let premier_refus = corps
        .find("refus_de_valeur(")
        .expect("aucune validation de valeur dans `patch_zone`");
    let premiere_ecriture = corps
        .find("ecrire!(")
        .expect("aucune écriture dans `patch_zone`");
    assert!(
        premier_refus < premiere_ecriture,
        "une validation arrive APRÈS une écriture : un PATCH refusé aurait \
         déjà modifié la zone"
    );
}

#[test]
fn full_volume_refusal_comes_before_any_write() {
    let corps = corps_du_handler();
    let refus = corps
        .find("fixed_volume_confirmation_required(&zone_before, &body)")
        .expect("le PATCH ne protège plus l'armement du volume fixe");
    let premiere_ecriture = corps
        .find("ecrire!(")
        .expect("aucune écriture dans `patch_zone`");
    assert!(
        refus < premiere_ecriture,
        "la confirmation du volume fixe est vérifiée APRÈS une écriture : \
         un PATCH refusé aurait déjà modifié la zone"
    );
}
