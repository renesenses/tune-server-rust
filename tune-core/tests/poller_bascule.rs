//! La bascule de `levels_available` doit rester ANNONCÉE, donc appelée.
//!
//! `la_bascule_est_annoncee_une_fois_et_une_seule` (dans `poller.rs`) prouve que
//! `annoncer_bascule_des_niveaux` émet ce qu'il faut — il l'appelle directement.
//! Il resterait donc vert si la boucle du poller cessait de l'appeler, et
//! `levels_available` resterait figé pendant exactement la lecture DSD
//! (JP Robbe, #2285).
//!
//! `spawn()` est une tâche infinie qui interroge des périphériques : elle ne se
//! teste pas en unitaire. On verrouille le point d'appel dans la source, comme
//! `dsp_track_boundary.rs` et `oaat_negociation.rs`.
//!
//! ⚠️ On cherche `self.annoncer…` et non le nom nu : la DÉFINITION de la
//! fonction se trouve elle aussi entre `spawn` et `tick`. Ma première version
//! cherchait le nom, et restait donc verte en retirant l'appel — elle trouvait
//! la définition. Tester l'existence au lieu du branchement est l'erreur que
//! cette revue m'a fait répéter le plus souvent.

use std::path::Path;

#[test]
fn la_boucle_du_poller_annonce_la_bascule() {
    let src = std::fs::read_to_string(Path::new("src/poller.rs"))
        .expect("src/poller.rs doit être lisible depuis la racine du crate");

    let debut = src
        .find("pub fn spawn(self)")
        .expect("spawn doit exister — s'il a été renommé, ce test doit suivre");
    // La méthode suivante, quelle que soit sa visibilité : `tick` a quitté le
    // fichier (REF-1, #2219) et une fenêtre ouverte jusqu'à la fin du fichier
    // ne mesurerait plus rien.
    let fin = [
        "\n    async fn ",
        "\n    pub async fn ",
        "\n    pub(super) async fn ",
        "\n    pub(crate) async fn ",
        "\n    fn ",
        "\n    pub fn ",
        "\n    pub(super) fn ",
        "\n    pub(crate) fn ",
    ]
    .iter()
    .filter_map(|m| src[debut + 1..].find(m))
    .min()
    .map(|i| debut + 1 + i)
    .expect("une méthode doit suivre spawn — sinon ce test ne borne plus rien");
    let boucle = &src[debut..fin];

    assert!(
        boucle.contains("self.annoncer_bascule_des_niveaux("),
        "la boucle du poller n'annonce plus la bascule : `levels_available` \
         restera figé pendant la lecture concernée, et le client ne refetchera \
         jamais (#2285)"
    );
}
