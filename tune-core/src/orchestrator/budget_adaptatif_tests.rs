use super::{
    BudgetAdaptatif, MARGE_BUDGET_TRANSCODAGE, PAS_SONDAGE_BUDGET, PLAFOND_BUDGET_TRANSCODAGE,
    SONDAGES_AVANT_MESURE, VerdictBudget, transcoder_sous_budget,
};
use crate::audio::decode_progress::DecodeProgress;
use std::sync::Arc;
use std::time::Duration;

/// Le budget historique, tel qu'il était calculé — reproduit ici sur la
/// taille SANS toucher au disque, pour que la contre-épreuve puisse dire
/// « rouge avant » sur le même nombre que la production.
fn budget_historique_pour(octets: u64) -> Duration {
    let gib = octets as f64 / (1024.0 * 1024.0 * 1024.0);
    Duration::from_secs((120 + (gib * 120.0).round() as u64).min(30 * 60))
}

/// Octets d'un DSD256 stéréo de `piste_s` secondes : 11 289 600 bits/s par
/// canal, deux canaux. C'est le format du ticket.
fn octets_dsd256(piste_s: f64) -> u64 {
    (piste_s * 11_289_600.0 / 8.0 * 2.0) as u64
}

/// Un transcodage FEINT : il consomme `piste_s / facteur` secondes
/// d'horloge tokio et publie son avancement sur la balise exactement comme
/// la boucle de décodage réelle (valeur cumulée, en millisecondes d'audio).
///
/// Le pas est de 250 ms d'audio, du même ordre qu'un paquet FLAC ; le vrai
/// DSF publie plus fin encore (un super-bloc ≈ 3 ms en DSD256), ce qui ne
/// peut que rendre l'estimation plus rapide, jamais plus lente.
async fn transcodage_feint(
    progres: Arc<DecodeProgress>,
    piste_s: f64,
    facteur: f64,
) -> Result<&'static str, String> {
    let total_ms = (piste_s * 1000.0) as u64;
    let mut decode_ms = 0u64;
    while decode_ms < total_ms {
        let tranche_ms = 250u64.min(total_ms - decode_ms);
        tokio::time::sleep(Duration::from_secs_f64(
            tranche_ms as f64 / 1000.0 / facteur,
        ))
        .await;
        decode_ms += tranche_ms;
        progres.publier(decode_ms);
    }
    Ok("transcodé")
}

async fn jouer(
    piste_s: f64,
    facteur: f64,
    budget_taille: Duration,
) -> (
    Result<Result<&'static str, String>, super::DepassementBudget>,
    Duration,
) {
    let debut = tokio::time::Instant::now();
    let progres = DecodeProgress::new();
    let politique = BudgetAdaptatif::new(piste_s, budget_taille);
    let r = transcoder_sous_budget(
        transcodage_feint(progres.clone(), piste_s, facteur),
        progres,
        politique,
        PAS_SONDAGE_BUDGET,
        None,
    )
    .await;
    (r, debut.elapsed())
}

// ------------------------------------------------------------------
// Couple 1 — le silence du ticket : DSD256 de 20 min sur un hôte à × 2,2
// (le facteur MESURÉ de Shrek, banc `dsd_to_pcm`).
// ------------------------------------------------------------------

/// ROUGE AVANT — et pas sur un calcul : sur l'ANCIEN code, exécuté.
///
/// `tokio::time::timeout(budget_de_taille, …)` est mot pour mot ce que
/// faisait `resolve_local_track` avant #3140. Le même décodeur feint, le
/// même couple, et il EXPIRE. Sans cette moitié-là, le test vert d'à côté
/// ne prouverait rien.
#[tokio::test(start_paused = true)]
async fn couple_1_rouge_avant_le_budget_de_taille_ne_tient_pas() {
    let piste_s = 20.0 * 60.0;
    let facteur = 2.2;
    let budget = budget_historique_pour(octets_dsd256(piste_s));
    let besoin_s = piste_s / facteur;
    assert!(
        besoin_s > budget.as_secs_f64(),
        "ce couple doit ÊTRE rouge avant : besoin {besoin_s:.0} s, \
         budget de taille {} s",
        budget.as_secs()
    );
    let progres = DecodeProgress::new();
    let avant = tokio::time::timeout(budget, transcodage_feint(progres, piste_s, facteur)).await;
    assert!(
        avant.is_err(),
        "l'ANCIEN budget devait EXPIRER sur ce couple, il a rendu {avant:?}"
    );
}

/// VERT APRÈS : le même couple aboutit, et il aboutit AU MOMENT où le
/// décodage finit — le budget ne l'a pas retenu.
#[tokio::test(start_paused = true)]
async fn couple_1_vert_apres_un_dsd256_de_20_min_a_x2_2_aboutit() {
    let piste_s = 20.0 * 60.0;
    let facteur = 2.2;
    let budget = budget_historique_pour(octets_dsd256(piste_s));
    let (r, ecoule) = jouer(piste_s, facteur, budget).await;
    assert!(
        matches!(r, Ok(Ok("transcodé"))),
        "le transcodage devait ABOUTIR, il a rendu {r:?}"
    );
    let attendu = piste_s / facteur;
    assert!(
        (ecoule.as_secs_f64() - attendu).abs() < 5.0,
        "il devait finir vers {attendu:.0} s, il a mis {:.0} s",
        ecoule.as_secs_f64()
    );
}

// ------------------------------------------------------------------
// Couple 2 — celui qui doit CONTINUER d'échouer : un budget infini n'est
// pas la réponse. Hôte à × 0,5 (deux fois plus lent que le temps réel),
// piste de 40 min : 4 800 s de décodage, contre un plafond de 1 800 s.
// ------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn couple_2_un_hote_deux_fois_plus_lent_que_le_temps_reel_echoue_toujours() {
    let piste_s = 40.0 * 60.0;
    let facteur = 0.5;
    let budget = budget_historique_pour(octets_dsd256(piste_s));
    let (r, ecoule) = jouer(piste_s, facteur, budget).await;
    let Err(d) = r else {
        panic!("un hôte à × 0,5 ne peut PAS transcoder 40 min : {r:?}");
    };
    assert_eq!(
        d.budget, PLAFOND_BUDGET_TRANSCODAGE,
        "l'échec doit survenir AU PLAFOND, pas plus tard"
    );
    assert!(
        ecoule < PLAFOND_BUDGET_TRANSCODAGE + PAS_SONDAGE_BUDGET * 2,
        "il doit rendre la main au plafond, il a mis {ecoule:?}"
    );
    // Et il doit NOMMER l'hôte : facteur mesuré, facteur qu'il aurait fallu.
    let mesure = d.facteur.expect("le facteur de l'hôte doit être mesuré");
    assert!(
        (mesure - facteur).abs() < 0.1,
        "facteur mesuré {mesure:.2}, attendu ~{facteur:.2}"
    );
    let requis = d.facteur_requis().expect("le facteur requis doit être dit");
    assert!(
        (requis - piste_s / PLAFOND_BUDGET_TRANSCODAGE.as_secs_f64()).abs() < 0.01,
        "facteur requis {requis:.2}"
    );
}

// ------------------------------------------------------------------
// Témoins verts des deux côtés.
// ------------------------------------------------------------------

/// Une piste courte sur une machine rapide ne change NI de budget NI de
/// comportement : c'est la garantie que ce correctif est invisible pour
/// tous ceux qui ne rencontraient pas le silence.
#[tokio::test(start_paused = true)]
async fn temoin_piste_courte_machine_rapide_ne_change_rien() {
    let piste_s = 4.0 * 60.0;
    let facteur = 6.0; // le DSD64 de Shrek
    let budget = budget_historique_pour(octets_dsd256(piste_s));
    let (r, ecoule) = jouer(piste_s, facteur, budget).await;
    assert!(matches!(r, Ok(Ok("transcodé"))), "{r:?}");
    // Le budget de taille suffisait déjà : la politique ne l'étend pas.
    let politique = BudgetAdaptatif::new(piste_s, budget);
    let besoin = Duration::from_secs_f64(piste_s / facteur);
    let v = politique.observer(besoin, Duration::from_secs_f64(piste_s), 99);
    match v {
        VerdictBudget::Mesure { budget: b, .. } => assert_eq!(
            b, budget,
            "le budget d'une machine rapide ne doit PAS bouger"
        ),
        autre => panic!("verdict inattendu : {autre:?}"),
    }
    assert!((ecoule.as_secs_f64() - piste_s / facteur).abs() < 2.0);
}

/// Un fichier réellement illisible échoue TOUJOURS, et VITE : l'erreur du
/// décodeur remonte telle quelle, sans consommer une seconde de budget.
#[tokio::test(start_paused = true)]
async fn temoin_un_fichier_illisible_echoue_toujours_et_vite() {
    let debut = tokio::time::Instant::now();
    let progres = DecodeProgress::new();
    let politique = BudgetAdaptatif::new(1200.0, Duration::from_secs(500));
    let r: Result<Result<&str, String>, _> = transcoder_sous_budget(
        async { Err::<&str, String>("decode failed: corrupt".to_string()) },
        progres,
        politique,
        PAS_SONDAGE_BUDGET,
        None,
    )
    .await;
    assert!(
        matches!(&r, Ok(Err(e)) if e.contains("corrupt")),
        "l'échec du décodeur doit remonter tel quel : {r:?}"
    );
    assert!(
        debut.elapsed() < Duration::from_secs(1),
        "il doit échouer VITE, il a mis {:?}",
        debut.elapsed()
    );
}

/// Un décodeur qui ne publie RIEN (AIFF, WavPack, APE, Opus : ils n'ont pas
/// de balise) garde le budget historique, au quart de seconde près, et son
/// échec porte l'ancien message — `facteur` reste `None`.
///
/// C'est la propriété qui empêche une absence de mesure de RACCOURCIR un
/// budget.
#[tokio::test(start_paused = true)]
async fn temoin_un_decodeur_muet_garde_le_budget_historique() {
    let budget = Duration::from_secs(200);
    let progres = DecodeProgress::new();
    let politique = BudgetAdaptatif::new(1200.0, budget);
    let debut = tokio::time::Instant::now();
    let r: Result<Result<&str, String>, _> = transcoder_sous_budget(
        async {
            // Ne publie jamais, et ne finit jamais dans le budget.
            tokio::time::sleep(Duration::from_secs(10_000)).await;
            Ok("jamais")
        },
        progres,
        politique,
        PAS_SONDAGE_BUDGET,
        None,
    )
    .await;
    let Err(d) = r else {
        panic!("il devait expirer : {r:?}")
    };
    assert_eq!(d.budget, budget, "le budget ne doit pas avoir bougé");
    assert!(d.facteur.is_none(), "rien n'a été mesuré, rien à annoncer");
    assert!(d.facteur_requis().is_some(), "la durée de piste est connue");
    let ecoule = debut.elapsed();
    assert!(
        ecoule >= budget && ecoule < budget + PAS_SONDAGE_BUDGET * 2,
        "il devait rendre la main à {budget:?}, il a mis {ecoule:?}"
    );
}

/// Durée de piste inconnue (`duration_ms` à zéro) : rien à extrapoler, le
/// budget historique s'applique inchangé.
#[test]
fn sans_duree_de_piste_aucune_extension() {
    let politique = BudgetAdaptatif::new(0.0, Duration::from_secs(120));
    assert_eq!(
        politique.observer(Duration::from_secs(10), Duration::from_secs(5), 99),
        VerdictBudget::PasEncore
    );
}

/// Avant `SONDAGES_AVANT_MESURE` fenêtres, on ne conclut RIEN : les
/// premières portent encore le coût fixe du démarrage (ouverture, recopie
/// locale, conception du filtre FIR) et sous-estiment le débit.
#[test]
fn pas_de_verdict_avant_assez_de_fenetres() {
    let politique = BudgetAdaptatif::new(1200.0, Duration::from_secs(120));
    assert_eq!(
        politique.observer(
            Duration::from_secs(1),
            Duration::from_millis(500),
            SONDAGES_AVANT_MESURE - 1
        ),
        VerdictBudget::PasEncore
    );
    assert!(matches!(
        politique.observer(
            Duration::from_secs(1),
            Duration::from_millis(500),
            SONDAGES_AVANT_MESURE
        ),
        VerdictBudget::Mesure { .. }
    ));
}

/// Le budget ne se RESSERRE jamais : quel que soit le débit mesuré, il
/// reste au moins celui de la taille. Sans cette borne, un débit
/// sous-estimé sur un cache froid ferait échouer une lecture qui marchait.
#[test]
fn le_budget_ne_se_resserre_jamais() {
    let budget = Duration::from_secs(600);
    let politique = BudgetAdaptatif::new(60.0, budget);
    // Hôte foudroyant : le besoin réel est de 2 s.
    let VerdictBudget::Mesure { budget: b, facteur } =
        politique.observer(Duration::from_secs(2), Duration::from_secs(60), 99)
    else {
        panic!("verdict attendu")
    };
    assert!(facteur > 20.0);
    assert_eq!(b, budget, "le budget de taille est un PLANCHER");
}

/// La marge est appliquée au temps restant, pas au temps déjà écoulé —
/// sinon un transcodage long verrait son budget enfler à chaque sondage.
#[test]
fn la_marge_porte_sur_le_restant() {
    let politique = BudgetAdaptatif::new(1200.0, Duration::from_secs(1));
    // À mi-course : 600 s d'audio décodées en 300 s → × 2,0.
    let VerdictBudget::Mesure { budget: b, facteur } =
        politique.observer(Duration::from_secs(300), Duration::from_secs(600), 99)
    else {
        panic!("verdict attendu")
    };
    assert!((facteur - 2.0).abs() < 1e-9);
    let attendu = 300.0 + 600.0 / 2.0 * MARGE_BUDGET_TRANSCODAGE;
    assert!(
        (b.as_secs_f64() - attendu).abs() < 0.5,
        "budget {b:?}, attendu {attendu:.1} s"
    );
}
