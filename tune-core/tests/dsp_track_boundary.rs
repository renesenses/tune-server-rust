//! La frontière de piste doit rester BRANCHÉE, pas seulement exister.
//!
//! `reset_local_dsp` remet le convolveur à zéro entre deux pistes. Un test
//! unitaire qui appelle ce helper directement prouve qu'il fonctionne — il ne
//! prouve pas qu'on l'appelle. C'est précisément l'écart que JP Robbe a relevé
//! sur #2268 : onze tests verts sur le moteur isolé, et la chaîne réelle qui ne
//! drainait rien.
//!
//! `play_url` est async et pilote un périphérique : il ne se teste pas en
//! unitaire. On verrouille donc le point d'appel dans la source, comme le fait
//! déjà `no_blind_ffmpeg.rs` pour une autre invariante de ce dépôt.

use std::path::Path;

#[test]
fn play_url_remet_le_convolveur_a_zero() {
    let src = std::fs::read_to_string(Path::new("src/outputs/local.rs"))
        .expect("src/outputs/local.rs doit être lisible depuis la racine du crate");

    let debut = src
        .find("async fn play_url(")
        .expect("play_url doit exister — s'il a été renommé, ce test doit suivre");
    // Une fenêtre large : l'appel est en tête de fonction, juste après `stop()`.
    let fin = (debut + 4000).min(src.len());
    let corps = &src[debut..fin];

    assert!(
        corps.contains("reset_local_dsp(&self.convolver)"),
        "play_url n'appelle plus reset_local_dsp : la queue d'une piste \
         repartira dans la suivante (#2268, revue JP Robbe)"
    );
}
