//! Garde #2211 : **aucune rampe sur le volume de la sortie ne tient lieu de
//! fondu enchaîné.**
//!
//! Ce que la mesure du 11/09/2026 a établi, au tag publié v0.9.145 :
//!
//! - `tune-core/src/playback/crossfade.rs` portait exactement **156 lignes**
//!   en v0.9.129, v0.9.141, v0.9.144 et v0.9.145 — quinze versions sans une
//!   ligne de différence. Le correctif `09be1df6` que l'issue cite n'a jamais
//!   eu de PR et n'est ancêtre d'aucun tag ;
//! - hors de son propre fichier, `git grep CrossfadeHandler` ne rendait
//!   **rien** : ni le sondeur, ni la fin de piste, ni un bras de
//!   l'orchestrateur ne l'instanciait. Les « deux fondus séquentiels de
//!   volume » que l'intitulé de #2211 dénonce ne s'exécutaient donc jamais ;
//! - le seul chemin que l'utilisateur atteint est la route
//!   `POST /zones/{id}/crossfade`, fermée par #2689 : 501
//!   `crossfade_unavailable`, préférence persistée forcée à `false`.
//!
//! L'arbitrage de Bertrand du 02/09/2026 est explicite : le vrai fondu
//! enchaîné mélangera deux flux décodés dans le moteur audio, sur la sortie
//! locale seulement, et **le volume matériel ne doit plus être touché** —
//! c'est lui qui rend l'implémentation d'origine audible comme un trou et qui
//! la fait interférer avec le volume persistant de la zone.
//!
//! Ce garde ferme la porte par laquelle la rampe reviendrait. Il ne prétend
//! pas qu'un fondu enchaîné existe : il interdit qu'on en simule un avec le
//! volume de la sortie, dans `tune-core` comme dans `tune-server`.
//!
//! ⚠️ Les aiguilles sont **assemblées à l'exécution**. Écrites en clair, elles
//! figureraient dans ce fichier, et un garde qui se trouve lui-même reste vert
//! sous sabotage (#2082). Il ne balaie de toute façon que les arbres `src/`,
//! jamais `tests/`.
//!
//! Aucun réseau, aucune base : le test relit des fichiers source sur le
//! disque.

use std::path::{Path, PathBuf};

/// Le nom de la structure retirée par #2211, en morceaux.
fn structure_de_fondu() -> String {
    format!("Cross{}Handler", "fade")
}

/// La signature de l'aide qui descendait puis remontait le volume de la
/// sortie, en morceaux. Cherchée comme **définition** (`fn fade_volume(`) et
/// non comme mot : `sleep_timer.rs` porte légitimement le marqueur de journal
/// `sleep_timer_fade_volume_failed`, et l'endormissement a le droit de baisser
/// le volume — c'est ce qu'on lui demande.
fn definition_de_rampe() -> String {
    format!("fn {}_volume(", "fade")
}

/// Le CODE du fichier, ses lignes de commentaire retirées.
///
/// Sans cette découpe le garde serait rouge sur les deux fichiers qui
/// **racontent** la suppression — `playback/mod.rs` et `config.rs` — et la
/// seule façon de le rendre vert serait d'effacer l'explication : un garde
/// qui punit la documentation de ce qu'il garde. Nommer la structure retirée
/// est permis ; l'employer ne l'est pas.
///
/// Seule la ligne ENTIÈREMENT commentaire est écartée, jamais la fin d'une
/// ligne de code : tronquer au premier `//` couperait aussi une URL dans une
/// chaîne et pourrait cacher du vrai code derrière — un faux vert, l'erreur
/// qu'on ne veut pas.
fn code_seul(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
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
            && code_seul(&src).contains(aiguille)
        {
            trouves.push(chemin);
        }
    }
}

/// Les deux arbres de code du moteur de lecture. `plugins/` est hors champ :
/// le greffon DJ a un vrai crossfader, à deux platines et à sa propre horloge,
/// et c'est un autre sujet que la transition entre deux pistes de la file.
fn arbres_du_moteur() -> Vec<PathBuf> {
    let caisse = Path::new(env!("CARGO_MANIFEST_DIR"));
    vec![
        caisse.join("src"),
        caisse
            .parent()
            .expect("tune-core doit vivre dans l'espace de travail")
            .join("tune-server")
            .join("src"),
    ]
}

fn chercher(aiguille: &str) -> Vec<String> {
    let mut trouves = Vec::new();
    for arbre in arbres_du_moteur() {
        assert!(
            arbre.is_dir(),
            "arbre de source introuvable : {} — ce garde ne balaie plus rien, \
             le corriger plutôt que le supprimer (#2211)",
            arbre.display()
        );
        parcourir(&arbre, aiguille, &mut trouves);
    }
    trouves.iter().map(|c| c.display().to_string()).collect()
}

#[test]
fn aucun_gestionnaire_de_fondu_par_le_volume_ne_revient() {
    let intrus = chercher(&structure_de_fondu());
    assert!(
        intrus.is_empty(),
        "le gestionnaire de fondu retiré par #2211 est de retour dans le \
         moteur de lecture : {intrus:?}. Il baissait le volume de \
         l'`OutputTarget` à zéro puis le remontait — sur une sortie \
         matérielle, c'est le volume PERSISTANT de la zone, et jamais deux \
         flux mélangés. L'arbitrage du 02/09/2026 est que le volume matériel \
         ne doit plus être touché : un vrai fondu enchaîné superpose deux flux \
         PCM décodés sur la sortie locale. Tant qu'il n'existe pas, la route \
         reste fermée (501 `crossfade_unavailable`, #2689) — mieux vaut une \
         option absente qu'une option qui ment."
    );
}

#[test]
fn aucune_rampe_de_volume_ne_se_redefinit_dans_le_moteur() {
    let intrus = chercher(&definition_de_rampe());
    assert!(
        intrus.is_empty(),
        "une aide qui fait glisser le volume de la sortie d'une valeur à une \
         autre est redéfinie dans le moteur de lecture : {intrus:?}. C'était \
         la mécanique exacte du faux fondu enchaîné de #2211 — dix commandes \
         de volume par seconde, avec le risque de zipper noise et \
         l'altération du volume de zone qui va avec. Si le besoin est un \
         endormissement, il existe déjà (`sleep_timer`) ; si c'est un fondu \
         entre deux pistes, il passe par un mélange PCM, pas par le volume."
    );
}
