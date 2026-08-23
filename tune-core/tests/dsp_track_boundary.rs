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

/// Et le drainage doit rester BRANCHÉ, sur les quatre chemins.
///
/// `flush_local_dsp` a existé pendant une PR entière sans un seul appel de
/// production — le compilateur le signalait, et je ne l'ai pas lu (JP Robbe,
/// revue de #2277). Un test qui appelle le helper directement ne peut pas voir
/// ça : c'est le NOMBRE de points d'appel qu'il faut tenir.
///
/// Quatre chemins de lecture locale appliquent le DSP, donc quatre doivent
/// drainer : le chemin d'un seul tenant, les deux chemins en continu, et le
/// chemin exclusif.
#[test]
fn les_quatre_chemins_drainent_le_convolveur() {
    let src = std::fs::read_to_string(Path::new("src/outputs/local.rs"))
        .expect("src/outputs/local.rs doit être lisible depuis la racine du crate");

    // La définition ne compte pas, ni l'appel du test unitaire voisin.
    let appels = src.matches("flush_local_dsp(").count();
    let definition = 1;
    let dans_les_tests = src
        .split("mod tests")
        .nth(1)
        .map(|t| t.matches("flush_local_dsp(").count())
        .unwrap_or(0);
    let production = appels - definition - dans_les_tests;

    let applications = src.matches("apply_local_dsp(").count()
        - 1
        - src
            .split("mod tests")
            .nth(1)
            .map(|t| t.matches("apply_local_dsp(").count())
            .unwrap_or(0);

    assert_eq!(
        production, applications,
        "{production} drainage(s) pour {applications} application(s) du DSP : \
         un chemin applique le convolveur sans jamais rendre ce qu'il retient, \
         donc tronque la fin de sa piste (#2209)"
    );
    assert!(
        production >= 4,
        "les quatre chemins de lecture locale doivent drainer, {production} trouvé(s)"
    );
}
