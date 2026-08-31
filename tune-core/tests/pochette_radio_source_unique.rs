//! Garde #3018 : UNE seule lecture des métadonnées radio dans `tune-core`.
//!
//! Le 30/08/2026 à 18h29, une réponse publiée au testeur Reivax66 (fil forum
//! 104, réponse 6020) affirmait — **en se présentant explicitement comme une
//! vérification de code** — que « la récupération des métadonnées radio ne
//! rapporte que le titre et l'interprète » et que « rien n'est allé chercher la
//! pochette du disque ». C'était faux depuis huit jours : `74677e35` (#2109,
//! 22/08) livre la pochette du titre, et la v0.9.127 qui la contient était
//! publiée depuis quatre heures. Le testeur a répondu le lendemain qu'il voyait
//! bien une image.
//!
//! Le dépôt portait alors DEUX lectures des métadonnées radio :
//!
//! - la vivante, `tune-core/src/radio_metadata.rs` : `visual` chez Radio
//!   France, `cover` chez Radio Paradise, `StreamUrl` dans le bloc ICY ; c'est
//!   `poller::vignette_du_pas_radio` qui tranche ensuite entre la pochette du
//!   titre et le logo de la station ;
//! - une morte, `tune-core/src/playback/radio_handler.rs` : un
//!   `RadioMetadataHandler` complet, **sans un seul appelant** sur toute la
//!   ligne de release, portant sa propre structure `IcyMetadata` homonyme de la
//!   vraie, et dont `fetch_icy_metadata` rendait `cover_url: None` **sans
//!   condition**.
//!
//! Autrement dit : le fichier mort disait, en Rust, exactement l'affirmation
//! fausse qui a été publiée. On ne peut pas prouver que c'est lui qui a été lu,
//! et ce test ne le prétend pas. Ce qu'il ferme est plus simple et suffit : il
//! n'y a plus qu'un seul endroit à lire, donc plus qu'une seule réponse
//! possible à la question « Tune va-t-il chercher la pochette d'un morceau en
//! radio ? ».
//!
//! Aucun réseau : le test relit des fichiers source sur le disque.

use std::path::{Path, PathBuf};

/// Les deux marqueurs d'une lecture de métadonnées radio, écrits en morceaux
/// pour que ce fichier-ci ne se signale pas lui-même.
fn en_tete_icy() -> String {
    format!("Icy{}MetaData", '-')
}

fn declaration_de_structure() -> String {
    format!("{} IcyMetadata", "struct")
}

fn parcourir(dir: &Path, aiguille: &str, trouves: &mut Vec<PathBuf>) {
    let entrees = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entree in entrees.flatten() {
        let chemin = entree.path();
        if chemin.is_dir() {
            parcourir(&chemin, aiguille, trouves);
        } else if chemin.extension().is_some_and(|e| e == "rs")
            && let Ok(src) = std::fs::read_to_string(&chemin)
            && src.contains(aiguille)
        {
            trouves.push(chemin);
        }
    }
}

/// Le seul fichier autorisé à porter ces marqueurs.
fn est_la_lecture_vivante(chemin: &Path) -> bool {
    chemin.file_name().is_some_and(|n| n == "radio_metadata.rs")
}

fn source_de_tune_core() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

#[test]
fn une_seule_lecture_des_metadonnees_radio() {
    let mut trouves = Vec::new();
    parcourir(&source_de_tune_core(), &en_tete_icy(), &mut trouves);

    let intrus: Vec<String> = trouves
        .iter()
        .filter(|c| !est_la_lecture_vivante(c))
        .map(|c| c.display().to_string())
        .collect();

    assert!(
        intrus.is_empty(),
        "seconde lecture des métadonnées radio dans tune-core (#3018) : {intrus:?}. \
         La lecture vivante est src/radio_metadata.rs — elle relit `visual` \
         (Radio France), `cover` (Radio Paradise) et `StreamUrl` (ICY), et \
         poller::vignette_du_pas_radio arbitre pochette du titre contre logo de \
         station. Un deuxième chemin, même sans appelant, se lit comme la \
         réponse à « Tune cherche-t-il la pochette ? » — c'est ce qui a fait \
         publier une affirmation fausse au fil forum 104 le 30/08/2026."
    );

    assert!(
        !trouves.is_empty(),
        "plus aucune lecture des métadonnées radio dans tune-core/src (#3018) : \
         src/radio_metadata.rs devait porter la requête ICY. Si le fichier a été \
         renommé, corriger ce garde plutôt que le supprimer — sans lui, la \
         pochette du morceau en radio redevient invérifiable."
    );
}

#[test]
fn une_seule_structure_de_metadonnees_icy() {
    let mut trouves = Vec::new();
    parcourir(
        &source_de_tune_core(),
        &declaration_de_structure(),
        &mut trouves,
    );

    let intrus: Vec<String> = trouves
        .iter()
        .filter(|c| !est_la_lecture_vivante(c))
        .map(|c| c.display().to_string())
        .collect();

    assert!(
        intrus.is_empty(),
        "structure `IcyMetadata` déclarée hors de src/radio_metadata.rs (#3018) : \
         {intrus:?}. Deux structures homonymes dont une seule porte réellement \
         une pochette : celle de playback/radio_handler.rs rendait `cover_url: \
         None` sans condition, et c'est le genre d'homonyme qu'une recherche \
         dans le code trouve avant la vraie."
    );

    assert_eq!(
        trouves.len(),
        1,
        "src/radio_metadata.rs doit déclarer exactement une structure \
         `IcyMetadata` (#3018) ; trouvé : {trouves:?}"
    );
}
