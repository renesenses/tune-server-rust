//! #3814 — « Qobuz sur sortie locale (macOS, Topping E30) : la lecture
//! s'arrête en plein morceau » (Dimitri, fil 1747).
//!
//! ## Ce que ce fichier garde
//!
//! Le seul témoin joint par le testeur est une ligne de ce dépôt :
//!
//! ```text
//! WARN tune_core::poller::tick: famine_anneau_fin — l'anneau audio est réalimenté ;
//!   bilan de l'épisode : zone_id=3 device=local:E30 position_ms=23684 flux_ms=976561
//!   rappels_a_court=4 echantillons_manquants=3288 silence_ms=37 duree_ms=128
//! ```
//!
//! Ses chiffres se lisent entièrement. `echantillons_manquants / silence_ms`
//! donne la cadence : 3 288 / 0,037 s = 88 864 échantillons entrelacés par
//! seconde, soit 44,1 kHz en stéréo. À cette cadence `duree_ms = 128` vaut
//! 11 289 échantillons réclamés par le pilote sur TOUT l'épisode — et un
//! épisode couvre au moins deux ticks du sondeur, qui tourne à la seconde
//! (`POLL_INTERVAL_MS`). Le pilote a donc réclamé **moins de 7 % de ce que
//! deux secondes de lecture lui doivent**.
//!
//! Un rappel de pilote en marche réclame 1 000 ms d'audio par seconde de temps
//! réel, anneau plein ou vide : un anneau vide lui rend des zéros, il ne
//! suspend pas ses rappels. Une horloge de pilote qui stagne ne dit donc pas
//! « l'anneau a été réalimenté », elle dit **« le consommateur s'est tu »** —
//! l'exact contraire de ce que la ligne annonçait.
//!
//! Ce n'est pas un défaut de rédaction : `SuiviFamine` fermait un épisode dès
//! que `events` cessait d'augmenter, et `events` cesse d'augmenter des DEUX
//! côtés du diagnostic. Sans le temps réel pour dénominateur, les deux états
//! sont indiscernables — et quatre dossiers (#3814, #3801, #3318, #2369) ont
//! cherché la panne chez le producteur sur la foi de cette phrase.
//!
//! ## Le banc
//!
//! Rien n'est simulé : l'anneau est le `RingBuf` de production, les compteurs
//! sont ceux que le rappel du pilote incrémente vraiment
//! (`RingStarvation::record`, appelé depuis `RingBuf::pop`), et les relevés
//! sont ceux que le sondeur lit (`RingStarvation::snapshot`). Seul le temps
//! réel est versé à la main — c'est exactement ce que fait le sondeur, le seul
//! qui ait le droit de lire l'heure.

use super::decisions::{FamineAnneau, SuiviFamine, rappel_en_retard};
use crate::outputs::traits::OutputRingStarvation;
use std::sync::Arc;

// Le banc matériel ne se compile qu'avec la sortie locale : `outputs::local`
// (et donc `RingBuf`) est derrière `local-audio`, et la porte `Test` de la CI
// tourne sans cette caractéristique. Toute la comptabilité et le témoin de
// branchement, eux, restent compilés partout.
#[cfg(feature = "local-audio")]
use crate::outputs::local::RingBuf;
#[cfg(feature = "local-audio")]
use crate::outputs::traits::RingStarvation;

/// La cadence du sondeur (`POLL_INTERVAL_MS`).
const TICK_MS: u64 = 1_000;

/// 44,1 kHz en stéréo — la cadence que la ligne du fil 1747 porte.
const CADENCE: u64 = 88_200;

/// Le rappel du pilote : 512 trames stéréo, soit 1 024 échantillons
/// entrelacés — 11,6 ms à 44,1 kHz.
#[cfg(feature = "local-audio")]
const RAPPEL: usize = 1_024;

/// Un banc : l'anneau de production et ses compteurs de production.
#[cfg(feature = "local-audio")]
struct Banc {
    anneau: RingBuf,
    compteurs: Arc<RingStarvation>,
}

#[cfg(feature = "local-audio")]
impl Banc {
    /// Un anneau de 2 s à 44,1 kHz stéréo — le dimensionnement de production
    /// (`ring_cap = taux × canaux × 2`, `outputs/local.rs`).
    fn neuf() -> Self {
        let compteurs = Arc::new(RingStarvation::new());
        compteurs.begin_stream(44_100, 2);
        let anneau = RingBuf::new_metered((CADENCE * 2) as usize, compteurs.clone());
        Self { anneau, compteurs }
    }

    /// Le producteur pousse `echantillons` échantillons entrelacés.
    fn produire(&self, echantillons: usize) -> usize {
        let bloc = vec![0.25f32; echantillons];
        self.anneau.push(&bloc)
    }

    /// UN rappel du pilote : il réclame `echantillons`, il prend ce que
    /// l'anneau a, et le reste part en zéros vers le DAC.
    fn rappel(&self, echantillons: usize) -> usize {
        let mut tampon = vec![0.0f32; echantillons];
        self.anneau.pop(&mut tampon)
    }

    /// Ce que le sondeur lit.
    fn releve(&self) -> OutputRingStarvation {
        self.compteurs.snapshot()
    }

    /// Une seconde de lecture saine : le producteur tient la cadence, le
    /// pilote est servi en entier à chaque rappel.
    fn une_seconde_saine(&self) {
        for _ in 0..(CADENCE as usize / RAPPEL) {
            self.produire(RAPPEL);
            assert_eq!(
                self.rappel(RAPPEL),
                RAPPEL,
                "une lecture saine sert le pilote en entier"
            );
        }
    }
}

// ───────────────── 1. le banc : une vraie famine, puis un vrai arrêt ────────

/// LE BANC. Un producteur bridé affame l'anneau de production — les compteurs
/// de production le voient —, puis le CONSOMMATEUR se tait pendant qu'une
/// seconde de temps réel passe.
///
/// Avant #3814, ce second tick fermait l'épisode sur « l'anneau audio est
/// réalimenté » alors que rien n'avait été réalimenté : le pilote avait
/// simplement cessé de réclamer.
#[cfg(feature = "local-audio")]
#[test]
fn un_rappel_qui_se_tait_ne_se_lit_pas_comme_un_anneau_realimente() {
    let banc = Banc::neuf();
    let mut suivi = SuiviFamine::default();

    // Tick 1 — une seconde de lecture saine. Le repère.
    banc.une_seconde_saine();
    assert!(
        suivi.observer(banc.releve(), TICK_MS).is_none(),
        "le premier relevé sert de repère"
    );

    // Tick 2 — le producteur ne fournit plus que le quart de ce que le pilote
    // réclame. L'anneau se vide POUR DE VRAI et le rappel reçoit des zéros.
    let mut a_court = 0;
    for _ in 0..(CADENCE as usize / RAPPEL) {
        banc.produire(RAPPEL / 4);
        if banc.rappel(RAPPEL) < RAPPEL {
            a_court += 1;
        }
    }
    assert!(
        a_court > 0,
        "le banc doit PROVOQUER une vraie famine, pas la décrire"
    );
    let Some(FamineAnneau::Debut(ouverture)) = suivi.observer(banc.releve(), TICK_MS) else {
        panic!("un anneau qui se vide doit ouvrir un épisode");
    };
    assert_eq!(
        ouverture.rappels_a_court, a_court,
        "le bilan compte les rappels réellement servis à court"
    );

    // Tick 3 — le rappel du pilote S'ARRÊTE. Le producteur, lui, continue :
    // c'est le cas qui départage, et c'est celui qu'on lisait à l'envers.
    banc.produire(CADENCE as usize / 2);
    for _ in 0..11 {
        // ~128 ms de rappels, le chiffre du fil 1747
        banc.rappel(RAPPEL);
    }
    let constat = suivi.observer(banc.releve(), TICK_MS);

    match constat {
        Some(FamineAnneau::RappelArrete(bilan)) => {
            assert!(
                bilan.duree_ms < bilan.ecoule_ms,
                "l'horloge du pilote ({} ms) doit être en dessous du temps réel ({} ms)",
                bilan.duree_ms,
                bilan.ecoule_ms
            );
            assert_eq!(
                bilan.echantillons_manquants, ouverture.echantillons_manquants,
                "le bilan cumule l'épisode entier"
            );
        }
        autre => panic!(
            "un rappel qui se tait doit être NOMMÉ, pas annoncé comme une réalimentation : {autre:?}"
        ),
    }
}

/// TÉMOIN VERT du banc : le producteur rattrape VRAIMENT son retard et le
/// pilote continue de réclamer son dû. C'est là, et seulement là, que
/// « l'anneau est réalimenté » est vrai — et le suivi doit toujours le dire.
#[cfg(feature = "local-audio")]
#[test]
fn un_anneau_vraiment_realimente_se_ferme_toujours_sur_fin() {
    let banc = Banc::neuf();
    let mut suivi = SuiviFamine::default();

    banc.une_seconde_saine();
    assert!(suivi.observer(banc.releve(), TICK_MS).is_none());

    for _ in 0..(CADENCE as usize / RAPPEL) {
        banc.produire(RAPPEL / 4);
        banc.rappel(RAPPEL);
    }
    assert!(matches!(
        suivi.observer(banc.releve(), TICK_MS),
        Some(FamineAnneau::Debut(_))
    ));

    // Le producteur rattrape : plus un seul rappel servi à court, et le pilote
    // réclame bien sa seconde.
    banc.une_seconde_saine();
    let Some(FamineAnneau::Fin(bilan)) = suivi.observer(banc.releve(), TICK_MS) else {
        panic!("un anneau réellement réalimenté doit fermer l'épisode sur Fin");
    };
    assert!(
        !rappel_en_retard(bilan.duree_ms, bilan.ecoule_ms),
        "le pilote a réclamé son dû : {} ms sur {} ms",
        bilan.duree_ms,
        bilan.ecoule_ms
    );
}

// ─────────────────── 2. la ligne de Dimitri, telle qu'elle ──────────────────

/// Un relevé bâti comme `RingStarvation::snapshot` le bâtit : `stream_ms` se
/// DÉDUIT des échantillons servis, il ne se pose pas.
fn releve(servis: u64, evenements: u64, manquants: u64) -> OutputRingStarvation {
    OutputRingStarvation {
        events: evenements,
        missing_samples: manquants,
        served_samples: servis,
        driver_underruns: 0,
        stream_ms: servis * 1_000 / CADENCE,
    }
}

/// Les trois relevés qui rendent EXACTEMENT la ligne du fil 1747.
///
/// `86 132 681` échantillons servis rendent `flux_ms = 976 561` (la division
/// entière de `snapshot`), et `11 289` échantillons en arrière rendent les
/// `128 ms` de l'épisode. Le relevé du milieu est celui qui ouvre l'épisode :
/// c'est lui qui porte les quatre rappels à court.
fn les_trois_releves_du_fil_1747() -> (
    OutputRingStarvation,
    OutputRingStarvation,
    OutputRingStarvation,
) {
    let fin = 86_132_681;
    let debut = fin - 11_289;
    let milieu = fin - 5_645;
    (
        releve(debut, 17, 40_000),
        releve(milieu, 17 + 4, 40_000 + 3_288),
        releve(fin, 17 + 4, 40_000 + 3_288),
    )
}

/// La ligne du testeur, rejouée. Les cinq chiffres qu'il a collés doivent se
/// retrouver au bit près — sinon le banc ne parle pas de son cas — et le
/// verdict doit être « le rappel s'est arrêté », pas « l'anneau est
/// réalimenté ».
#[test]
fn la_ligne_du_fil_1747_est_un_rappel_arrete_pas_une_realimentation() {
    let (avant, ouverture, fermeture) = les_trois_releves_du_fil_1747();
    let mut suivi = SuiviFamine::default();

    assert!(suivi.observer(avant, TICK_MS).is_none(), "le repère");
    assert!(
        matches!(
            suivi.observer(ouverture, TICK_MS),
            Some(FamineAnneau::Debut(_))
        ),
        "les quatre rappels à court ouvrent l'épisode"
    );

    let Some(FamineAnneau::RappelArrete(bilan)) = suivi.observer(fermeture, TICK_MS) else {
        panic!("128 ms d'horloge de pilote sur deux secondes de temps réel est un rappel arrêté");
    };

    assert_eq!(bilan.rappels_a_court, 4, "rappels_a_court du fil 1747");
    assert_eq!(
        bilan.echantillons_manquants, 3_288,
        "echantillons_manquants du fil 1747"
    );
    assert_eq!(bilan.silence_ms, 37, "silence_ms du fil 1747");
    assert_eq!(bilan.duree_ms, 128, "duree_ms du fil 1747");
    assert_eq!(bilan.flux_ms, 976_561, "flux_ms du fil 1747");
    assert_eq!(
        bilan.ecoule_ms,
        2 * TICK_MS,
        "l'épisode couvre les deux intervalles du sondeur"
    );
}

/// Le même épisode, fermé par un SEEK plutôt que par un tick — l'hypothèse la
/// plus probable de la ligne de Dimitri (« si je déplace la barre un peu plus
/// loin, ça repart »). Un seek recrée le flux, donc `begin_stream` remet les
/// compteurs à zéro et le bilan part par la branche « flux neuf ». C'était le
/// dernier endroit d'où la phrase mensongère pouvait encore sortir.
#[test]
fn la_branche_flux_neuf_nomme_aussi_le_rappel_arrete() {
    let (avant, ouverture, _) = les_trois_releves_du_fil_1747();
    let mut suivi = SuiviFamine::default();
    suivi.observer(avant, TICK_MS);
    assert!(matches!(
        suivi.observer(ouverture, TICK_MS),
        Some(FamineAnneau::Debut(_))
    ));

    // Le seek : compteurs neufs, très en dessous des précédents.
    let neuf = releve(CADENCE / 2, 0, 0);
    let Some(FamineAnneau::RappelArrete(bilan)) = suivi.observer(neuf, TICK_MS) else {
        panic!("le flux mourant dont le rappel s'était tu ne doit pas se dire « réalimenté »");
    };
    assert_eq!(
        bilan.duree_ms, 64,
        "l'épisode s'arrête au dernier relevé vu"
    );

    // Et le flux neuf, lui, repart d'un repère muet.
    assert!(
        suivi
            .observer(releve(CADENCE + CADENCE / 2, 0, 0), TICK_MS)
            .is_none()
    );
}

/// TÉMOIN VERT de la branche « flux neuf » : un seek qui interrompt une
/// famine dont le pilote tournait NORMALEMENT doit toujours rendre `Fin`.
#[test]
fn un_flux_neuf_apres_une_vraie_famine_dit_toujours_fin() {
    let mut suivi = SuiviFamine::default();
    suivi.observer(releve(CADENCE, 0, 0), TICK_MS);
    assert!(matches!(
        suivi.observer(releve(2 * CADENCE, 40, 19_200), TICK_MS),
        Some(FamineAnneau::Debut(_))
    ));
    let Some(FamineAnneau::Fin(bilan)) = suivi.observer(releve(CADENCE / 2, 0, 0), TICK_MS) else {
        panic!("un pilote qui a réclamé sa seconde a bien vu son anneau réalimenté");
    };
    assert_eq!(bilan.duree_ms, 1_000);
}

// ──────────── 3. l'arrêt SANS famine préalable — l'instrument neuf ──────────

/// Le cas de Didier (#3801) : `wasapi_exclusive_stopped … underruns=1` sur une
/// piste entière, et pourtant des sauts par dizaines. Un anneau qui ne se vide
/// pas ne produit AUCUN rappel à court — donc, jusqu'ici, aucune ligne du
/// tout. Un pilote qui cesse de réclamer doit être nommé même quand l'anneau
/// est plein.
#[test]
fn un_pilote_qui_se_tait_sans_famine_est_nomme_puis_sa_reprise_aussi() {
    let mut suivi = SuiviFamine::default();
    let mut servis = CADENCE;

    assert!(suivi.observer(releve(servis, 0, 0), TICK_MS).is_none());

    // Le pilote ne réclame plus que 100 ms par seconde de temps réel.
    let mut lignes = Vec::new();
    for _ in 0..4 {
        servis += CADENCE / 10;
        lignes.push(suivi.observer(releve(servis, 0, 0), TICK_MS));
    }
    assert!(
        lignes[0].is_none() && lignes[1].is_none(),
        "deux ticks ne suffisent pas : un changement de piste en vaut autant"
    );
    assert!(
        matches!(lignes[2], Some(FamineAnneau::RappelArrete(_))),
        "au troisième tick consécutif, l'arrêt doit être nommé : {:?}",
        lignes[2]
    );
    assert!(
        lignes[3].is_none(),
        "un arrêt qui dure n'écrit pas une ligne par seconde"
    );

    // Le pilote repart : une seule ligne de reprise, avec le bilan de tout
    // l'arrêt.
    servis += CADENCE;
    let Some(FamineAnneau::RappelRepris(bilan)) = suivi.observer(releve(servis, 0, 0), TICK_MS)
    else {
        panic!("la reprise du rappel doit fermer l'arrêt");
    };
    assert_eq!(
        bilan.ecoule_ms,
        5 * TICK_MS,
        "le bilan couvre tout l'arrêt, du dernier relevé sain à la reprise"
    );
    assert!(
        suivi
            .observer(releve(servis + CADENCE, 0, 0), TICK_MS)
            .is_none(),
        "et la lecture reprise ne dit plus rien"
    );
}

/// TÉMOIN VERT, et c'est celui qui coûte : entre deux pistes sans
/// enchaînement continu, le flux de sortie se referme et ses compteurs restent
/// FIGÉS jusqu'au `begin_stream` suivant — pendant que la zone se dit encore
/// en lecture. Ce trou-là dure quelques centaines de millisecondes
/// (`playback_timing … total_ms=219`, relevé de Didier du 10/09). Il ne doit
/// JAMAIS produire une ligne : un instrument qui crie à chaque changement de
/// piste ne sert plus à rien.
#[test]
fn un_changement_de_piste_ne_fabrique_pas_un_rappel_arrete() {
    let mut suivi = SuiviFamine::default();

    for piste in 0..5u64 {
        let mut servis = 0;
        for _ in 0..4 {
            servis += CADENCE;
            assert!(
                suivi.observer(releve(servis, 0, 0), TICK_MS).is_none(),
                "piste {piste} : une lecture saine ne dit rien"
            );
        }
        // Un tick, puis deux, où les compteurs de la piste finie ne bougent
        // plus : le flux suivant n'est pas encore armé.
        assert!(
            suivi.observer(releve(servis, 0, 0), TICK_MS).is_none(),
            "piste {piste} : le premier tick figé ne doit rien écrire"
        );
        assert!(
            suivi.observer(releve(servis, 0, 0), TICK_MS).is_none(),
            "piste {piste} : le deuxième non plus"
        );
        // `begin_stream` : compteurs neufs.
        assert!(
            suivi.observer(releve(CADENCE / 2, 0, 0), TICK_MS).is_none(),
            "piste {piste} : un flux neuf se recale sans rien dire"
        );
    }
}

/// Un intervalle de sondage trop court ne dit rien : le rapport « horloge du
/// pilote / temps réel » tombe dans le bruit d'un tampon de période, et on
/// n'en tire pas un verdict.
#[test]
fn un_intervalle_trop_court_ne_conclut_rien() {
    assert!(!rappel_en_retard(0, 100), "100 ms ne décident de rien");
    assert!(!rappel_en_retard(0, 499));
    assert!(
        rappel_en_retard(0, 500),
        "une demi-seconde sans un seul échantillon réclamé est un arrêt"
    );
    assert!(
        !rappel_en_retard(600, 1_000),
        "un pilote qui sert 60 % du temps réel n'est pas à l'arrêt"
    );
    assert!(rappel_en_retard(128, 1_000), "le cas du fil 1747");
}

/// Une PAUSE n'est pas un arrêt de rappel. Le sondeur remet le suivi à zéro
/// dès que la zone cesse d'être en lecture (`tick.rs`) : sans ça, le relèvé
/// pris avant la pause servirait de repère à celui pris après, et la reprise
/// annoncerait la fin d'un arrêt qui n'a jamais eu lieu.
#[test]
fn une_pause_nannonce_pas_la_reprise_dun_arret_qui_nen_etait_pas_un() {
    let mut suivi = SuiviFamine::default();
    let mut servis = CADENCE;
    assert!(suivi.observer(releve(servis, 0, 0), TICK_MS).is_none());
    // Le pilote se tait : la zone part en pause, ses compteurs se figent.
    for _ in 0..3 {
        suivi.observer(releve(servis, 0, 0), TICK_MS);
    }

    // Le sondeur voit la zone en pause et remet le suivi à zéro.
    suivi.reinitialiser();

    // Reprise : le premier relèvé n'est qu'un repère, le second une lecture
    // saine. Aucun des deux n'a de reprise d'arrêt à annoncer.
    servis += CADENCE;
    assert!(suivi.observer(releve(servis, 0, 0), TICK_MS).is_none());
    servis += CADENCE;
    assert!(
        suivi.observer(releve(servis, 0, 0), TICK_MS).is_none(),
        "une pause n'est pas un arrêt de rappel : sa reprise n'a rien à annoncer"
    );
}

// ───────────────────────── 4. le BRANCHEMENT dans tick() ────────────────────

/// Récupère la sortie `tracing` d'un futur : c'est le journal, et lui seul,
/// que l'on aura entre les mains la prochaine fois qu'un testeur écrira.
#[derive(Clone, Default)]
struct JournalCapture(Arc<std::sync::Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// LE BRANCHEMENT. Toute la comptabilité ci-dessus ne vaut rien si `tick()`
/// ne la lit pas : un verdict juste que personne n'appelle ne change pas une
/// ligne du journal du testeur.
///
/// Un vrai `tick()` de sondeur, sur une vraie zone locale en lecture, doit
/// écrire `rappel_pilote_arrete` quand l'horloge du pilote cesse d'avancer —
/// et surtout PAS `famine_anneau_fin`, la phrase qui a envoyé quatre dossiers
/// chercher la panne chez le producteur.
///
/// Le temps réel est ici le vrai : c'est le sondeur qui le mesure
/// (`ZonePollState::famine_releve_at`), et rien ne le simule. D'où les
/// attentes — elles ne peuvent que RENFORCER le déficit mesuré, jamais
/// l'effacer.
#[tokio::test]
async fn un_rappel_arrete_atteint_le_journal_par_un_vrai_tick() {
    let journal = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _garde = tracing::subscriber::set_default(abonne);

    let repere = releve(CADENCE, 0, 0);
    let mut banc = super::famine_anneau_i3318::Banc::monter(Some(repere)).await;
    banc.tick_avec(Some(repere)).await;

    // Une seconde d'anneau vide : l'épisode s'ouvre.
    let vide = releve(2 * CADENCE, 40, 19_200);
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    banc.tick_avec(Some(vide)).await;

    // Puis le pilote SE TAIT : mêmes compteurs, même horloge de flux, et du
    // temps réel qui passe.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    banc.tick_avec(Some(vide)).await;

    let texte = journal.texte();
    let attendu = ["rappel", "pilote", "arrete"].join("_");
    let mensonge = ["famine", "anneau", "fin"].join("_");
    assert!(
        texte.contains(&attendu),
        "un vrai tick doit NOMMER le rappel arrêté ; journal :\n{texte}"
    );
    assert!(
        !texte.contains(&mensonge),
        "et il ne doit surtout pas dire que l'anneau a été réalimenté ; journal :\n{texte}"
    );
}
