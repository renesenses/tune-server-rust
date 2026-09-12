//! #3205 — le compteur de famine de l'anneau audio mesure-t-il vraiment ?
//!
//! Ce que ce fichier tient, et pourquoi il existe : Tune OS paie le Secure
//! Boot et un dépôt COPR non signé pour un noyau `PREEMPT_RT` dont personne
//! n'a jamais mesuré le bénéfice. Le seul chiffre capable de trancher est le
//! nombre de fois où le rappel audio a manqué de données — pas la latence
//! d'ordonnancement, invisible derrière un anneau de deux secondes et une
//! garde de 500 ms. Un compteur qui compterait n'importe quoi ferait passer
//! pour une mesure ce qui n'en est pas une ; d'où le TÉMOIN
//! (`un_flux_correctement_alimente_laisse_le_compteur_a_zero`) autant que
//! l'épreuve de famine.
//!
//! Les tests appellent le drain de PRODUCTION (`RingBuf::pop`, celui-là même
//! que les six rappels audio invoquent) : ils ne rejouent pas l'arithmétique
//! du compteur, ils l'exercent.

#![cfg(feature = "local-audio")]

use std::sync::Arc;
use tune_core::outputs::local::RingBuf;
use tune_core::outputs::traits::RingStarvation;

/// Un anneau instrumenté, armé pour un flux stéréo 48 kHz.
fn anneau(capacite: usize) -> (Arc<RingStarvation>, RingBuf) {
    let compteur = Arc::new(RingStarvation::new());
    compteur.begin_stream(48_000, 2);
    let anneau = RingBuf::new_metered(capacite, compteur.clone());
    (compteur, anneau)
}

/// Le premier rappel servi EN ENTIER arme le compteur : c'est le « démarrage
/// du flux ». Sans lui, l'anneau vide d'avant la première note ferait passer
/// chaque début de piste pour un incident.
fn demarrer(anneau: &RingBuf) {
    let mut sortie = [0.0f32; 8];
    assert_eq!(anneau.push(&[0.5; 8]), 8);
    assert_eq!(anneau.pop(&mut sortie), 8, "le flux doit démarrer plein");
}

#[test]
fn un_anneau_sous_alimente_fait_monter_le_compteur() {
    let (compteur, anneau) = anneau(64);
    demarrer(&anneau);

    // Famine délibérée : le producteur n'a écrit que 3 échantillons sur les 8
    // que le pilote réclame. Les 5 autres partent en zéros vers le DAC.
    let mut sortie = [0.0f32; 8];
    assert_eq!(anneau.push(&[0.25; 3]), 3);
    assert_eq!(anneau.pop(&mut sortie), 3);

    let releve = compteur.snapshot();
    assert_eq!(releve.events, 1, "une famine, un événement");
    assert_eq!(
        releve.missing_samples, 5,
        "5 échantillons comblés par des zéros"
    );
    assert_eq!(
        releve.served_samples, 16,
        "8 échantillons au démarrage + 8 réclamés pendant la famine"
    );
}

/// Un compteur d'événements SEUL ne distingue pas un micro-trou d'une coupure
/// d'une seconde. C'est ce que ce test épingle : deux famines de gravités très
/// différentes, deux événements, mais un cumul d'échantillons qui les sépare.
#[test]
fn le_compteur_separe_un_micro_trou_d_une_coupure() {
    let (compteur, anneau) = anneau(64);
    demarrer(&anneau);

    let mut sortie = [0.0f32; 8];
    assert_eq!(anneau.push(&[0.25; 7]), 7);
    assert_eq!(anneau.pop(&mut sortie), 7); // micro-trou : 1 manquant
    assert_eq!(anneau.pop(&mut sortie), 0); // coupure franche : 8 manquants

    let releve = compteur.snapshot();
    assert_eq!(releve.events, 2);
    assert_eq!(releve.missing_samples, 9, "1 + 8");
}

/// LE TÉMOIN. Sans lui, un compteur qui compterait n'importe quel rappel
/// passerait pour un succès dans le test ci-dessus.
#[test]
fn un_flux_correctement_alimente_laisse_le_compteur_a_zero() {
    let (compteur, anneau) = anneau(4096);
    demarrer(&anneau);

    let mut sortie = [0.0f32; 8];
    for _ in 0..200 {
        assert_eq!(anneau.push(&[0.5; 8]), 8);
        assert_eq!(anneau.pop(&mut sortie), 8);
    }

    let releve = compteur.snapshot();
    assert_eq!(releve.events, 0, "un flux nourri n'affame personne");
    assert_eq!(releve.missing_samples, 0);
    assert_eq!(releve.served_samples, 8 + 200 * 8);
}

/// Rien n'est compté tant que le flux n'a pas démarré : au tout début l'anneau
/// est vide par construction, et le rappel rend du silence sans que ce soit un
/// incident. Contre-épreuve du garde d'armement.
#[test]
fn le_silence_d_avant_le_demarrage_n_est_pas_une_famine() {
    let (compteur, anneau) = anneau(64);

    let mut sortie = [0.0f32; 8];
    for _ in 0..10 {
        assert_eq!(anneau.pop(&mut sortie), 0, "anneau encore vide");
    }

    let releve = compteur.snapshot();
    assert_eq!(releve.events, 0);
    assert_eq!(releve.served_samples, 0);
    assert_eq!(releve.stream_ms, 0);
}

/// Le dénominateur qui rend le chiffre exploitable après une heure de lecture :
/// un taux d'événements par heure ne se calcule pas sans savoir depuis combien
/// de temps le flux tourne. La durée est déduite du COMPTE d'échantillons —
/// le rappel audio n'a pas le droit de lire l'heure.
#[test]
fn la_duree_de_flux_se_deduit_de_la_cadence() {
    let compteur = Arc::new(RingStarvation::new());
    compteur.begin_stream(48_000, 2); // 96 000 échantillons entrelacés / seconde
    let anneau = RingBuf::new_metered(192_000, compteur.clone());

    // Une seconde pleine d'audio stéréo 48 kHz, servie en blocs de 960.
    let bloc = vec![0.1f32; 960];
    let mut sortie = [0.0f32; 960];
    for _ in 0..100 {
        assert_eq!(anneau.push(&bloc), 960);
        assert_eq!(anneau.pop(&mut sortie), 960);
    }

    let releve = compteur.snapshot();
    assert_eq!(releve.served_samples, 96_000);
    assert_eq!(releve.stream_ms, 1_000, "une seconde d'audio, une seconde");
    assert_eq!(releve.events, 0);
}

/// Un flux neuf repart de zéro : sans cette remise à zéro, le rapport d'un
/// testeur cumulerait les incidents de toutes les pistes de la session et le
/// taux par heure serait faux.
#[test]
fn un_nouveau_flux_remet_le_compteur_a_zero() {
    let (compteur, anneau) = anneau(64);
    demarrer(&anneau);
    let mut sortie = [0.0f32; 8];
    assert_eq!(anneau.pop(&mut sortie), 0);
    assert_eq!(compteur.snapshot().events, 1);

    compteur.begin_stream(44_100, 2);
    let releve = compteur.snapshot();
    assert_eq!(releve.events, 0);
    assert_eq!(releve.missing_samples, 0);
    assert_eq!(releve.served_samples, 0);
}

/// La sortie locale expose bien le relevé par le contrat de sortie — c'est ce
/// que lit `/api/v1/system/diagnostics`. Une sortie qui compte sans être lue
/// ne décide de rien.
#[test]
fn la_sortie_locale_expose_son_releve_par_le_contrat() {
    use tune_core::outputs::local::LocalOutput;
    use tune_core::outputs::traits::OutputTarget;

    let sortie = LocalOutput::new("default".to_string());
    let releve = OutputTarget::ring_starvation(&sortie)
        .expect("la sortie locale rend l'audio elle-même : elle doit répondre");
    assert_eq!(releve.events, 0);
    assert_eq!(releve.missing_samples, 0);
    assert_eq!(releve.served_samples, 0);
}

/// Une sortie qui ne rend pas l'audio elle-même n'a aucun anneau à affamer :
/// elle doit répondre `None` plutôt qu'un zéro qu'on lirait comme « aucun
/// incident ». Un zéro fabriqué serait exactement le faux témoin que ce
/// chantier doit éviter.
#[test]
fn une_sortie_sans_anneau_ne_fabrique_pas_un_zero() {
    use tune_core::outputs::mock::MockOutput;
    use tune_core::outputs::traits::OutputTarget;

    let sortie = MockOutput::new("mock", "Mock");
    assert!(OutputTarget::ring_starvation(&sortie).is_none());
}
