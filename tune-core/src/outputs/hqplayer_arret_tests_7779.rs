//! #7779 — un VRAI arrêt de HQPlayer quand la zone s'arrête.
//!
//! Retour terrain (nurni, rc9, HQPlayer Embedded 5.17, SDM DSD256 et
//! convolution) : en pause, HQPlayer garde tout son traitement prêt et l'hôte
//! reste à ~70 W ; seul `Stop` le ramène au repos (~31 W). L'arrêt de la
//! sortie écrivait `Stop` sans rien vérifier. Il attend désormais que
//! `Status` rapporte l'état 0, et la pause reste une pause : sa reprise est
//! un `Play` sur place.
//!
//! Faux HQPlayer de #4851 (socket locale, XML du protocole) : aucun son.

use super::enchainement_tests_4851::FauxHqplayer;
use super::*;

const COURANTE: &str = "http://192.168.0.20:8888/stream/aaaa.flac";
const AUTRE: &str = "http://192.168.0.20:8888/stream/eeee.flac";

fn media(url: &str) -> PlayMedia<'_> {
    PlayMedia {
        url,
        mime_type: "audio/flac",
        ..Default::default()
    }
}

fn etat(faux: &FauxHqplayer) -> u8 {
    faux.etat.lock().unwrap().etat
}

fn commandes(faux: &FauxHqplayer, nom: &str) -> usize {
    faux.recu()
        .iter()
        .filter(|c| {
            c.strip_prefix('<').is_some_and(|r| {
                r.starts_with(nom) && !r[nom.len()..].starts_with(char::is_alphanumeric)
            })
        })
        .count()
}

/// LE CAS SIGNALÉ : la zone est en pause, puis elle s'arrête. HQPlayer doit
/// finir à l'état 0 (arrêté), pas rester en pause, et l'arrêt n'est rendu
/// qu'une fois rapporté par `Status` (STOPREQ d'abord, comme le vrai).
#[tokio::test]
async fn l_arret_d_une_zone_en_pause_arrete_vraiment_hqplayer_7779() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 60.0, 300.0).await;
    let sortie = faux.sortie();
    sortie.pause().await.unwrap();
    assert_eq!(etat(&faux), 1, "la pause garde HQPlayer en pause");
    faux.regler(|e| e.status_avant_arret = 3);

    sortie.stop().await.expect("#7779 : arrêt confirmé");

    assert_eq!(
        etat(&faux),
        0,
        "#7779 : HQPlayer est resté en STOPREQ/pause après l'arrêt de la zone ; reçu {:?}",
        faux.recu()
    );
    let recu = faux.recu();
    let stop = recu.iter().position(|c| c.starts_with("<Stop")).unwrap();
    assert!(
        recu[stop + 1..].iter().any(|c| c.starts_with("<Status")),
        "#7779 : l'arrêt doit être vérifié par Status : {recu:?}"
    );
    assert_eq!(
        sortie.get_status().await.unwrap().state,
        TransportState::Stopped
    );
}

/// En pleine lecture aussi : `Stop`, puis l'état 0 constaté.
#[tokio::test]
async fn l_arret_en_lecture_est_confirme_par_status_7779() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 60.0, 300.0).await;
    let sortie = faux.sortie();
    faux.regler(|e| e.status_avant_arret = 2);
    sortie.stop().await.unwrap();
    assert_eq!(etat(&faux), 0, "reçu {:?}", faux.recu());
    assert_eq!(commandes(&faux, "Stop"), 1);
}

/// Un HQPlayer qui ne rapporte jamais l'arrêt : l'attente est bornée et
/// l'arrêt est une ERREUR (l'orchestrateur la journalise), pas un succès muet.
#[tokio::test]
async fn un_arret_jamais_rapporte_est_une_erreur_bornee_7779() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 60.0, 300.0).await;
    faux.regler(|e| e.n_arrete_jamais = true);
    let sortie = faux.sortie();

    let debut = std::time::Instant::now();
    let erreur = sortie
        .stop()
        .await
        .expect_err("#7779 : un arrêt non rapporté ne doit pas passer pour réussi");
    let duree = debut.elapsed();
    assert!(erreur.contains("hqplayer_stop_not_confirmed"), "{erreur}");
    assert!(erreur.contains("did not report the stop"), "{erreur}");
    assert!(
        duree < ATTENTE_ARRET + std::time::Duration::from_secs(2),
        "attente {duree:?}"
    );
}

/// La pause n'est PAS un arrêt : aucun `Stop`, et la reprise est un `Play`
/// sur place qui rend la lecture.
#[tokio::test]
async fn la_pause_reste_une_pause_et_reprend_sur_place_7779() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 60.0, 300.0).await;
    let sortie = faux.sortie();
    sortie.pause().await.unwrap();
    assert_eq!(commandes(&faux, "Stop"), 0, "{:?}", faux.recu());
    sortie.resume().await.unwrap();
    assert_eq!(etat(&faux), 2);
    assert_eq!(commandes(&faux, "Stop"), 0, "{:?}", faux.recu());
    assert_eq!(commandes(&faux, "PlaylistAdd"), 0, "reprise sur place");
    assert_eq!(faux.playlist(), vec![COURANTE]);
}

/// Après un vrai arrêt, relancer la zone fonctionne : pas de second `Stop`
/// (HQPlayer est déjà arrêté), le titre est chargé et joue.
#[tokio::test]
async fn apres_l_arret_la_relance_joue_7779() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 60.0, 300.0).await;
    let sortie = faux.sortie();
    sortie.pause().await.unwrap();
    sortie.stop().await.unwrap();

    sortie.play_media(&media(AUTRE)).await.unwrap();
    assert_eq!(etat(&faux), 2);
    assert_eq!(faux.playlist(), vec![AUTRE]);
    assert_eq!(commandes(&faux, "Stop"), 1, "{:?}", faux.recu());
}

/// L'arrêt oublie la suivante posée d'avance (#4851) : la relance ne doit
/// pas croire qu'elle est encore dans la playlist.
#[tokio::test]
async fn l_arret_oublie_la_suivante_posee_7779() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 60.0, 300.0).await;
    let sortie = faux.sortie();
    sortie.set_next_media(&media(AUTRE)).await.unwrap();
    assert!(sortie.enchainement().suivante.is_some());
    sortie.stop().await.unwrap();
    assert!(sortie.enchainement().suivante.is_none());
}
