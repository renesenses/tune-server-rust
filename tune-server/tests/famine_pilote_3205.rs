//! #3205 — le rappel d'ERREUR du backend doit COMPTER la sous-alimentation du
//! pilote, et pas seulement la journaliser.
//!
//! # Ce que cette garde défend
//!
//! `tune-core/src/outputs/local.rs` reçoit `cpal::StreamError::BufferUnderrun`
//! quand ALSA a sous-alimenté le DAC : le processus n'a pas été ordonnancé à
//! temps. Jusqu'à ce lot, ce signal partait dans un `warn!` **plafonné à une
//! ligne par seconde** et n'était compté nulle part — une heure à 5 000
//! sous-alimentations et une heure à 3 600 laissaient exactement le même
//! journal. Or c'est CE chiffre dont #3205 fait dépendre le sort du noyau
//! `PREEMPT_RT` de Tune OS : « si les xruns sont à zéro sur noyau standard, le
//! noyau RT est un coût sans gain ».
//!
//! La famine de l'anneau, déjà mesurée, ne peut pas le remplacer : sur un
//! XRun, cpal signale l'erreur, recouvre et **saute le rappel de données**
//! (`cpal/src/host/alsa/mod.rs`, branche `PollDescriptorsFlow::XRun`). Le
//! rappel n'est jamais appelé, l'anneau est resté plein, et son compteur ne
//! bouge pas d'un cran pendant le trou.
//!
//! # Pourquoi le TEXTE et pas l'exécution
//!
//! Le même motif que `journal_pcm_alsa_ouvert.rs`, pour la même raison, et
//! elle est structurelle : le code visé est derrière `local-audio`, que le job
//! `Test` de `ci.yml` **n'active pas** (`--no-default-features --features
//! oaat,cloud-relay,bandcamp`). Le seul job qui l'active à l'exécution,
//! `Test (jeu de fonctionnalités livré)`, est gardé par
//! `if: needs.impact.outputs.full == 'true'` — il ne tourne que sous
//! `ci:full`. Une garde qui appellerait `make_stream_error_cb` serait donc
//! compilée par clippy et **exécutée par aucun job de routine**.
//!
//! `include_str!` ignore les `cfg` : la garde lit le fichier tel qu'il est sur
//! le disque, et tourne sur toutes les PR. C'est ce qui la rend utile.
//!
//! Le contrat du COMPTEUR lui-même (la disjonction des deux pannes, la remise
//! à zéro, la sérialisation) est tenu ailleurs, par de vrais tests exécutés :
//! `tune-output-api/src/lib.rs`, module `famine_pilote_3205`.

const SOURCE: &str = include_str!("../../tune-core/src/outputs/local.rs");

/// Le bloc qui suit `ancre` dans `texte`, par APPARIEMENT DES ACCOLADES.
///
/// Volontairement pas une fenêtre de N octets ni un `find` jusqu'au prochain
/// `}` : ces deux raccourcis rendent vert un bloc dont l'accolade fermante a
/// bougé, et c'est exactement la façon dont une garde de ce dépôt est restée
/// verte sur le sabotage qu'elle prétendait tenir.
///
/// `texte` est un paramètre et non `SOURCE` : la seconde recherche doit avoir
/// lieu DANS le rappel d'erreur déjà extrait, pas dans les 10 000 lignes du
/// fichier. Une aiguille qui existerait ailleurs ne doit pas pouvoir répondre
/// à la place de celle qu'on vise.
fn bloc_apres(texte: &str, ancre: &str) -> String {
    let debut = texte
        .find(ancre)
        .unwrap_or_else(|| panic!("ancre introuvable : `{ancre}`"));
    let reste = &texte[debut..];
    let ouvrante = reste
        .find('{')
        .unwrap_or_else(|| panic!("aucune accolade ouvrante après `{ancre}`"));

    let mut profondeur = 0usize;
    for (i, c) in reste[ouvrante..].char_indices() {
        match c {
            '{' => profondeur += 1,
            '}' => {
                profondeur -= 1;
                if profondeur == 0 {
                    return reste[ouvrante..=ouvrante + i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("bloc jamais refermé après `{ancre}` : l'appariement des accolades a échoué");
}

/// Le rappel d'erreur doit compter, et compter SEULEMENT la sous-alimentation.
///
/// Trois assertions, parce que trois régressions distinctes sont possibles :
///
/// * la garde vise le mauvais bloc — un renommage la laisserait verte contre
///   n'importe quoi ;
/// * le comptage disparaît — on revient au `warn!` plafonné, et #3205 redevient
///   immesurable sans démonter la machine du testeur ;
/// * le comptage devient inconditionnel — chaque `StreamError`, y compris un
///   débranchement d'USB, gonflerait le chiffre qui décide du noyau RT.
#[test]
fn le_rappel_d_erreur_compte_la_sous_alimentation_du_pilote() {
    let corps = bloc_apres(SOURCE, "fn make_stream_error_cb(");

    assert!(
        corps.contains("audio_stream_error"),
        "le bloc extrait n'est pas le rappel d'erreur de la sortie locale : \
         `make_stream_error_cb` a été renommé ou déplacé. Réaccorder cette \
         garde fait partie du changement"
    );

    let aiguille = "cpal::StreamError::BufferUnderrun";
    assert!(
        corps.contains(aiguille),
        "`make_stream_error_cb` ne reconnaît plus `BufferUnderrun` : le signal \
         que cpal remonte quand ALSA a sous-alimenté le DAC n'est plus \
         distingué des autres erreurs de flux"
    );

    let arme = bloc_apres(&corps, aiguille);
    assert!(
        arme.contains("record_driver_underrun"),
        "`make_stream_error_cb` reçoit `BufferUnderrun` — ALSA a sous-alimenté \
         le DAC, le processus n'a pas été ordonnancé à temps — et ne le COMPTE \
         pas. Le `warn!` qui suit est plafonné à une ligne par seconde : sans \
         compteur, une heure à 5 000 sous-alimentations et une heure à 3 600 \
         rendent le même journal, et le chiffre dont #3205 fait dépendre le \
         sort du noyau PREEMPT_RT de Tune OS n'existe plus. La famine de \
         l'anneau ne le remplace pas : sur un XRun, cpal saute le rappel de \
         données et l'anneau reste plein"
    );

    // Le comptage vit DANS la branche `BufferUnderrun`, pas au-dessus d'elle.
    let avant_la_branche = &corps[..corps.find(aiguille).expect("déjà vérifié plus haut")];
    assert!(
        !avant_la_branche.contains("record_driver_underrun"),
        "le compteur de sous-alimentation du pilote est incrémenté AVANT d'avoir \
         reconnu `BufferUnderrun` : un débranchement d'USB ou une erreur de \
         backend gonflerait le chiffre qui décide du noyau RT"
    );
}
