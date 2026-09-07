use super::{
    FinDeTranscodage, PAS_SONDAGE_BUDGET, Supersession, ecrire_sauf_si_abandonne,
    transcoder_sous_budget,
};
use crate::audio::decode_progress::DecodeProgress;
use crate::playback::PlaybackManager;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Le couple du ticket, ramené à ses deux grandeurs.
///
/// `Aac -> Flac` de 102,2 s mesuré sur le .18 en 0.9.136, et un budget qui
/// laisse largement le temps de finir : `transcode_budget_for` accorde 120 s au
/// plancher et le budget adaptatif monte jusqu'à `PLAFOND_BUDGET_TRANSCODAGE`,
/// soit 1 800 s. C'est cette borne-là, pas la durée du format, qui dit combien
/// de temps la zone peut rester prise.
const TRANSCODAGE_S: f64 = 102.2;
/// La demande de lecture qui arrive PENDANT le transcodage.
const DEMANDE_A_S: f64 = 3.0;
const ZONE: i64 = 10;

/// Un pré-transcodage feint : il consomme `duree_s` d'horloge tokio et publie
/// son avancement sur la même balise que la vraie boucle de décodage.
async fn transcodage_feint(
    progres: Arc<DecodeProgress>,
    duree_s: f64,
) -> Result<&'static str, String> {
    let pas_ms = 250u64;
    let total_ms = (duree_s * 1000.0) as u64;
    let mut ecoule_ms = 0u64;
    while ecoule_ms < total_ms {
        let tranche = pas_ms.min(total_ms - ecoule_ms);
        tokio::time::sleep(Duration::from_millis(tranche)).await;
        ecoule_ms += tranche;
        // Décodage à × 1 temps réel : la valeur importe peu ici, seul compte
        // que la balise PARLE — sans quoi le budget ne s'étendrait jamais et
        // le test mesurerait autre chose que la préemption.
        progres.publier(ecoule_ms);
    }
    Ok("transcodé")
}

/// La zone, et la demande de lecture qui la prend `a_s` secondes plus tard.
/// C'est `bump_generation` — le geste EXACT de `play_inner` — qui joue le
/// deuxième clic.
fn une_demande_de_lecture_a(playback: Arc<PlaybackManager>, a_s: f64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64(a_s)).await;
        playback.bump_generation(ZONE).await;
    });
}

async fn surveille(
    playback: Arc<PlaybackManager>,
    seq: u64,
    budget: Duration,
    supersede: bool,
) -> (
    Result<Result<&'static str, String>, FinDeTranscodage>,
    Duration,
) {
    let debut = tokio::time::Instant::now();
    let progres = DecodeProgress::new();
    let politique = super::BudgetAdaptatif::new(TRANSCODAGE_S, budget);
    let sup = Supersession {
        playback,
        zone_id: ZONE,
        seq,
    };
    let r = transcoder_sous_budget(
        transcodage_feint(progres.clone(), TRANSCODAGE_S),
        progres,
        politique,
        PAS_SONDAGE_BUDGET,
        None,
        supersede.then_some(&sup),
    )
    .await;
    (r, debut.elapsed())
}

// ------------------------------------------------------------------
// Le témoin du ticket, en deux moitiés.
// ------------------------------------------------------------------

/// ROUGE AVANT — et pas sur un calcul : sur l'ANCIEN comportement, exécuté.
///
/// `supersede = false` reproduit mot pour mot ce que faisait le chien de garde
/// avant ce correctif : aucun point de contrôle pendant le travail. La demande
/// de lecture tombe à 3 s, elle est bien enregistrée par la zone — et le
/// transcodage court malgré tout ses 102,2 s, exactement comme le journal du
/// .18 le montre (`transcode_to_temp_file_complete elapsed_ms=102163`, puis
/// `superseded_skipping_output resolve_ms=102170`). Sans cette moitié-là, le
/// test vert d'à côté ne prouverait rien.
#[tokio::test(start_paused = true)]
async fn rouge_avant_le_transcodage_ignore_la_demande_et_va_au_bout() {
    let playback = Arc::new(PlaybackManager::new());
    let seq = playback.bump_generation(ZONE).await;
    une_demande_de_lecture_a(playback.clone(), DEMANDE_A_S);
    let (r, ecoule) = surveille(playback.clone(), seq, Duration::from_secs(120), false).await;
    assert!(
        matches!(r, Ok(Ok("transcodé"))),
        "l'ancien comportement va AU BOUT : {r:?}"
    );
    assert!(
        ecoule.as_secs_f64() > TRANSCODAGE_S - 1.0,
        "la zone devait rester prise les {TRANSCODAGE_S} s entières, \
         elle n'a tenu que {:.1} s",
        ecoule.as_secs_f64()
    );
    // Et la supersession était pourtant DÉTECTABLE depuis la 3e seconde.
    assert_ne!(
        playback.current_play_seq(ZONE).await,
        seq,
        "la demande concurrente doit bien avoir bumpé la génération"
    );
}

/// VERT APRÈS : la même demande, au même instant, prend la main au pas de
/// sondage près — et le journal sait dire ce qui a été jeté.
#[tokio::test(start_paused = true)]
async fn la_demande_emise_pendant_le_transcodage_prend_la_main() {
    let playback = Arc::new(PlaybackManager::new());
    let seq = playback.bump_generation(ZONE).await;
    une_demande_de_lecture_a(playback.clone(), DEMANDE_A_S);
    let (r, ecoule) = surveille(playback.clone(), seq, Duration::from_secs(120), true).await;
    let Err(FinDeTranscodage::Preempte { gagnant, perdu }) = r else {
        panic!("le transcodage devait être ABANDONNÉ, il a rendu {r:?}");
    };
    assert_eq!(
        gagnant,
        playback.current_play_seq(ZONE).await,
        "le journal doit nommer la demande QUI PREND LA MAIN"
    );
    assert_ne!(
        gagnant, seq,
        "et elle doit différer de celle qu'on abandonne"
    );
    // Le temps perdu est celui qu'on annonce, et il est BORNÉ par le pas.
    assert!(
        (perdu.as_secs_f64() - DEMANDE_A_S).abs() <= PAS_SONDAGE_BUDGET.as_secs_f64() * 2.0,
        "temps perdu annoncé {:.2} s, la demande est tombée à {DEMANDE_A_S} s",
        perdu.as_secs_f64()
    );
    assert!(
        ecoule.as_secs_f64() < DEMANDE_A_S + PAS_SONDAGE_BUDGET.as_secs_f64() * 2.0,
        "la zone devait être rendue vers {DEMANDE_A_S} s, elle l'a été à {:.2} s",
        ecoule.as_secs_f64()
    );
}

/// La conséquence qui compte pour l'auditeur : le verrou par fichier
/// (`TRANSCODE_GATE`, tenu par `transcoder_vers_fichier` pendant tout le
/// pré-transcodage) est rendu, donc la demande émise pendant le transcodage est
/// SERVIE au lieu d'être mise en attente derrière un travail déjà condamné.
///
/// Le verrou est reproduit ici sous sa forme exacte — un `Arc<Mutex<()>>` par
/// fichier — plutôt que d'être pris sur le statique de production, que les
/// essais voisins partageraient.
#[tokio::test(start_paused = true)]
async fn la_demande_est_servie_et_non_mise_en_attente_derriere_le_verrou() {
    let playback = Arc::new(PlaybackManager::new());
    let verrou: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));
    let seq = playback.bump_generation(ZONE).await;

    // La demande perdante : elle tient le verrou et transcode.
    let perdante = {
        let playback = playback.clone();
        let verrou = verrou.clone();
        tokio::spawn(async move {
            let _tenu = verrou.lock().await;
            surveille(playback, seq, Duration::from_secs(120), true).await
        })
    };
    // Laisser la perdante prendre le verrou avant que la gagnante n'arrive.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // La demande gagnante : elle bumpe la génération (comme `play_inner`),
    // puis attend son tour sur le verrou du même fichier.
    let debut = tokio::time::Instant::now();
    playback.bump_generation(ZONE).await;
    let _servie = verrou.lock().await;
    let attente = debut.elapsed();

    assert!(
        attente.as_secs_f64() < PAS_SONDAGE_BUDGET.as_secs_f64() * 3.0,
        "la demande gagnante devait être servie au pas de sondage près, \
         elle a attendu {:.2} s (le ticket en mesure 102)",
        attente.as_secs_f64()
    );
    let (r, _) = perdante.await.expect("la perdante ne doit pas paniquer");
    assert!(
        matches!(r, Err(FinDeTranscodage::Preempte { .. })),
        "la perdante devait abandonner : {r:?}"
    );
}

/// Non-régression : sans demande concurrente, RIEN ne change — le transcodage
/// va au bout, à la milliseconde près.
#[tokio::test(start_paused = true)]
async fn sans_demande_concurrente_le_transcodage_va_au_bout() {
    let playback = Arc::new(PlaybackManager::new());
    let seq = playback.bump_generation(ZONE).await;
    let (r, ecoule) = surveille(playback, seq, Duration::from_secs(120), true).await;
    assert!(
        matches!(r, Ok(Ok("transcodé"))),
        "sans concurrence le transcodage doit ABOUTIR : {r:?}"
    );
    assert!(
        (ecoule.as_secs_f64() - TRANSCODAGE_S).abs() < 1.0,
        "il devait finir vers {TRANSCODAGE_S} s, il a mis {:.1} s",
        ecoule.as_secs_f64()
    );
}

// ------------------------------------------------------------------
// Le fichier temporaire abandonné.
// ------------------------------------------------------------------

/// Abandon posé : aucun `tune-transcode-*` ne subsiste, que l'écriture ait eu
/// le temps de commencer ou non. Sans cette garde, le balayage
/// `cleanup_leftover_transcode_files` ne ramasserait le fichier qu'au prochain
/// démarrage du serveur.
#[test]
fn abandon_pose_aucun_temporaire_ne_subsiste() {
    // `scratch_dir` et non un chemin composé à la main : le dossier part par
    // `Drop`, y compris quand l'essai ÉCHOUE (#3030).
    let d = crate::test_scratch::scratch_dir("preemption-abandon");
    let dest = d.join("tune-transcode-abandonne.flac");
    let dest_s = dest.to_string_lossy().to_string();
    let abandon = AtomicBool::new(true);
    ecrire_sauf_si_abandonne(&dest_s, &[0u8; 4096], Some(&abandon)).expect("écriture");
    assert!(
        !dest.exists(),
        "un pré-transcodage abandonné ne doit laisser AUCUN fichier : {dest_s}"
    );
}

/// Le témoin d'à côté : sans abandon, le fichier est écrit tel quel. C'est le
/// chemin de toutes les lectures qui aboutissent.
#[test]
fn sans_abandon_le_temporaire_est_ecrit() {
    let d = crate::test_scratch::scratch_dir("preemption-nominal");
    let dest = d.join("tune-transcode-nominal.flac");
    let dest_s = dest.to_string_lossy().to_string();
    ecrire_sauf_si_abandonne(&dest_s, &[7u8; 4096], None).expect("écriture");
    assert_eq!(
        std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
        4096,
        "le fichier nominal doit être écrit en entier"
    );
    let abandon = AtomicBool::new(false);
    let dest2 = d.join("tune-transcode-nominal2.flac");
    let dest2_s = dest2.to_string_lossy().to_string();
    ecrire_sauf_si_abandonne(&dest2_s, &[7u8; 2048], Some(&abandon)).expect("écriture");
    assert!(
        dest2.exists(),
        "un drapeau d'abandon BAISSÉ ne doit rien empêcher"
    );
    assert!(!abandon.load(Ordering::Relaxed));
}
