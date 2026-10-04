//! Fil 2125 (#5711) — la reprise automatique après décrochage (#4645).
//!
//! Journal du 03/10 : `renderer_cale_reprise_automatique position_ms=194000`
//! écrit sur le seul acquittement du `Seek`, alors que le renderer relisait
//! depuis l'octet 0 ; puis silence, coupure 38 s plus tard, et la même
//! séquence recommence. Deux règles :
//!
//! 1. la reprise n'est « réussie » qu'au vu d'une position MESURÉE après le
//!    saut, à la cible près ;
//! 2. une seule reprise par lecture de piste : le décrochage de la reprise
//!    elle-même coupe la zone.
use super::decisions::{self, ConstatDeRepriseCale};
use super::{
    REPRISE_CALE_DELAI_DE_CONSTAT_MS, REPRISE_CALE_DELAI_MAX_DE_CONSTAT_MS,
    REPRISE_CALE_ECART_TOLERE_MS,
};

const CIBLE_MS: u64 = 194_000;
const GENERATION: u64 = 42;

#[test]
fn le_constat_attend_que_le_saut_ait_pu_prendre() {
    assert_eq!(
        decisions::constat_de_reprise_cale(GENERATION, GENERATION, 500, true, 3_000, CIBLE_MS),
        ConstatDeRepriseCale::Attendre,
        "un échantillon pris pendant la pose ne juge rien"
    );
}

#[test]
fn le_renderer_a_la_cible_est_une_reprise_reussie() {
    assert_eq!(
        decisions::constat_de_reprise_cale(
            GENERATION,
            GENERATION,
            REPRISE_CALE_DELAI_DE_CONSTAT_MS,
            true,
            CIBLE_MS + 2_500,
            CIBLE_MS,
        ),
        ConstatDeRepriseCale::Reussie
    );
    assert_eq!(
        decisions::constat_de_reprise_cale(
            GENERATION,
            GENERATION,
            REPRISE_CALE_DELAI_DE_CONSTAT_MS,
            true,
            CIBLE_MS - REPRISE_CALE_ECART_TOLERE_MS,
            CIBLE_MS,
        ),
        ConstatDeRepriseCale::Reussie,
        "un calage sur trame juste sous la cible reste une réussite"
    );
}

/// LE cas du fil 2125 : Seek acquitté, renderer reparti du début.
#[test]
fn le_renderer_reparti_du_debut_est_un_saut_ignore_2125() {
    assert_eq!(
        decisions::constat_de_reprise_cale(
            GENERATION,
            GENERATION,
            REPRISE_CALE_DELAI_DE_CONSTAT_MS,
            true,
            3_000,
            CIBLE_MS,
        ),
        ConstatDeRepriseCale::SautIgnore,
        "à 0:03 au lieu de 3:14 : le Seek a été acquitté puis oublié"
    );
}

#[test]
fn sans_echantillon_probant_le_saut_n_est_pas_constate() {
    // Arrêté, ou position nulle : rien ne prouve ni l'un ni l'autre.
    assert_eq!(
        decisions::constat_de_reprise_cale(GENERATION, GENERATION, 10_000, false, 0, CIBLE_MS),
        ConstatDeRepriseCale::Attendre
    );
    assert_eq!(
        decisions::constat_de_reprise_cale(GENERATION, GENERATION, 10_000, true, 0, CIBLE_MS),
        ConstatDeRepriseCale::Attendre,
        "une position nulle n'est pas une mesure"
    );
    assert_eq!(
        decisions::constat_de_reprise_cale(
            GENERATION,
            GENERATION,
            REPRISE_CALE_DELAI_MAX_DE_CONSTAT_MS,
            false,
            0,
            CIBLE_MS,
        ),
        ConstatDeRepriseCale::NonConstatee
    );
}

#[test]
fn une_lecture_changee_abandonne_le_constat() {
    assert_eq!(
        decisions::constat_de_reprise_cale(
            GENERATION,
            GENERATION + 1,
            REPRISE_CALE_DELAI_DE_CONSTAT_MS,
            true,
            3_000,
            CIBLE_MS,
        ),
        ConstatDeRepriseCale::Abandonnee,
        "piste suivante ou relance : la position mesurée n'est plus celle de la reprise"
    );
}

#[test]
fn une_seule_reprise_par_lecture_de_piste_2125() {
    assert!(
        decisions::reprise_cale_deja_tentee_sur_cette_piste(Some(GENERATION), GENERATION),
        "la reprise elle-même a décroché : pas de seconde reprise, la zone est coupée"
    );
    assert!(
        !decisions::reprise_cale_deja_tentee_sur_cette_piste(Some(GENERATION), GENERATION + 1),
        "une relance ou une autre piste garde son droit à une reprise"
    );
    assert!(!decisions::reprise_cale_deja_tentee_sur_cette_piste(
        None, GENERATION
    ));
}

/// Le bras de production : saut par l'orchestrateur (qui attend l'ouverture
/// du flux), plus de `seek` nu, et la ligne de réussite n'est plus écrite
/// dans le bras — seulement au constat.
#[test]
fn le_bras_de_reprise_passe_par_le_saut_differe_et_constate_2125() {
    const SOURCE: &str = include_str!("tick.rs");
    let debut = SOURCE
        .find("} else if let Some(position_ms) = reprise_cale {")
        .expect("le bras de reprise a disparu de tick.rs");
    let fin = SOURCE[debut..]
        .find("} else if track_ended {")
        .expect("la branche suivante a disparu de tick.rs");
    let bras = &SOURCE[debut..debut + fin];
    assert!(
        bras.contains(".sauter_apres_reprise_de_renderer_cale("),
        "le saut doit passer par l'orchestrateur, qui attend que le renderer \
         ait ouvert le flux (fil 2125)"
    );
    assert!(
        !bras.contains(".seek(zone_id, position_ms"),
        "plus de Seek nu dans la foulée du Play : le renderer l'acquitte puis \
         relit depuis l'octet 0 (fil 2125)"
    );
    assert!(
        !bras.contains("\"renderer_cale_reprise_automatique\""),
        "la réussite ne s'écrit pas sur l'acquittement"
    );
    assert!(
        bras.contains("r.saut_demande_a = Some(Instant::now());"),
        "le bras doit armer le constat sur la position mesurée"
    );
    assert!(
        SOURCE.contains("decisions::constat_de_reprise_cale("),
        "le constat doit être branché dans le tick"
    );
    let decision = SOURCE
        .find("let reprise_cale = match mesure_renderer_cale {")
        .expect("la décision de reprise a disparu");
    assert!(
        SOURCE[decision..debut].contains("decisions::reprise_cale_deja_tentee_sur_cette_piste("),
        "une seule reprise par piste doit être appliquée à la décision"
    );
}
