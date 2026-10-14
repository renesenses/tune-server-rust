//! #6115 — lancer un AUTRE titre alors que HQPlayer joue ou est en pause.
//!
//! Retour terrain (hifi25nl, rc4, fil 2209) : même après Stop puis un autre
//! titre, HQPlayer continue de jouer le précédent. Le faux HQPlayer de #4851
//! reproduit ce comportement : tant que la lecture n'est pas arrêtée (état 0),
//! il ignore `PlaylistAdd clear="1"`. Après un `Stop`, il passe par l'état 3
//! (STOPREQ de la source HQPlayer Control 6.0.1) avant l'état 0.
//!
//! Ce faux NE PROUVE PAS que le vrai HQPlayer se comporte ainsi : il fixe le
//! comportement signalé, et vérifie que Tune y répond par l'enchaînement
//! de l'intégration Audiolinux (`--stop` avant `--play`).

use super::enchainement_tests_4851::FauxHqplayer;
use super::*;

const COURANTE: &str = "http://192.168.0.20:8888/stream/aaaa.flac";
const AUTRE: &str = "http://192.168.0.20:8888/stream/eeee.flac";
const SUIVANTE: &str = "http://192.168.0.20:8888/stream/bbbb.flac";

fn media(url: &str) -> PlayMedia<'_> {
    PlayMedia {
        url,
        mime_type: "audio/flac",
        ..Default::default()
    }
}

/// Un faux HQPlayer qui ignore `clear="1"` hors arrêt, en lecture de
/// `COURANTE`, déjà sondé une fois par la sortie (comme le fait le sondeur).
async fn faux_qui_ignore_clear(
    etat: u8,
    status_avant_arret: u32,
) -> (FauxHqplayer, HqplayerOutput) {
    let faux = FauxHqplayer::en_lecture(COURANTE, 60.0, 300.0).await;
    faux.regler(|e| {
        e.ignore_clear_hors_arret = true;
        e.etat = etat;
        e.status_avant_arret = status_avant_arret;
    });
    let sortie = faux.sortie();
    let _ = sortie.get_status().await.unwrap();
    (faux, sortie)
}

/// LE DÉFAUT SIGNALÉ : en pleine lecture, un autre titre doit remplacer le
/// précédent. Sans `Stop` d'abord, le faux reste sur `COURANTE`.
#[tokio::test]
async fn un_autre_titre_en_pleine_lecture_remplace_le_precedent() {
    let (faux, sortie) = faux_qui_ignore_clear(2, 0).await;

    sortie.play_media(&media(AUTRE)).await.unwrap();

    assert_eq!(
        faux.joue(),
        (Some(AUTRE.to_string()), 2),
        "#6115 : HQPlayer joue encore le titre précédent ; `PlaylistAdd clear=\"1\"` \
         envoyé sans arrêt préalable est ignoré. Reçu {:?}",
        faux.recu()
    );
    assert_eq!(faux.playlist(), vec![AUTRE]);
}

/// En pause aussi, la piste en cours n'est pas arrêtée : même chemin.
#[tokio::test]
async fn un_autre_titre_en_pause_remplace_le_precedent() {
    let (faux, sortie) = faux_qui_ignore_clear(1, 0).await;

    sortie.play_media(&media(AUTRE)).await.unwrap();

    assert_eq!(
        faux.joue(),
        (Some(AUTRE.to_string()), 2),
        "#6115 en pause : reçu {:?}",
        faux.recu()
    );
}

/// L'arrêt n'est pas instantané (état 3, STOPREQ) : `PlaylistAdd` ne part
/// qu'une fois l'état 0 rapporté par `Status`, et l'ordre sur le fil est
/// `Stop`, `Status`…, `PlaylistAdd clear="1"`, `Play`.
#[tokio::test]
async fn le_lancement_attend_que_status_rapporte_l_arret() {
    let (faux, sortie) = faux_qui_ignore_clear(2, 3).await;

    sortie.play_media(&media(AUTRE)).await.unwrap();

    assert_eq!(
        faux.joue(),
        (Some(AUTRE.to_string()), 2),
        "{:?}",
        faux.recu()
    );
    let recu = faux.recu();
    let apres_le_premier_status: Vec<&str> = recu.iter().skip(1).map(String::as_str).collect();
    let pos = |prefixe: &str| {
        apres_le_premier_status
            .iter()
            .position(|c| c.starts_with(prefixe))
            .unwrap_or_else(|| panic!("pas de {prefixe} : {apres_le_premier_status:?}"))
    };
    assert_eq!(pos("<Stop"), 0, "{apres_le_premier_status:?}");
    let statuts = apres_le_premier_status
        .iter()
        .filter(|c| c.starts_with("<Status"))
        .count();
    assert_eq!(
        statuts, 4,
        "trois Status en STOPREQ puis un à l'arrêt : {apres_le_premier_status:?}"
    );
    assert!(pos("<Status") < pos("<PlaylistAdd"));
    assert!(pos("<PlaylistAdd") < pos("<Play "));
}

/// Un HQPlayer qui ne rapporte jamais l'arrêt : l'attente est bornée (2 s),
/// et le lancement part quand même, sans erreur.
#[tokio::test]
async fn l_attente_de_l_arret_est_bornee() {
    let (faux, sortie) = faux_qui_ignore_clear(2, 0).await;
    faux.regler(|e| e.n_arrete_jamais = true);

    let debut = std::time::Instant::now();
    sortie.play_media(&media(AUTRE)).await.unwrap();
    let duree = debut.elapsed();

    assert!(
        duree >= ATTENTE_ARRET && duree < ATTENTE_ARRET + std::time::Duration::from_secs(2),
        "attente {duree:?}, borne {ATTENTE_ARRET:?}"
    );
    let recu = faux.recu();
    assert!(
        recu.iter().any(|c| c.starts_with("<PlaylistAdd"))
            && recu.iter().any(|c| c.starts_with("<Play ")),
        "le lancement part après la borne : {recu:?}"
    );
}

/// Un HQPlayer à l'arrêt (fin de piste, file qui avance) : rien ne change,
/// pas de `Stop`, pas d'attente.
#[tokio::test]
async fn a_l_arret_aucun_stop_n_est_envoye() {
    let (faux, sortie) = faux_qui_ignore_clear(0, 0).await;

    sortie.play_media(&media(AUTRE)).await.unwrap();

    assert!(
        !faux.recu().iter().any(|c| c.starts_with("<Stop")),
        "{:?}",
        faux.recu()
    );
    assert_eq!(faux.joue(), (Some(AUTRE.to_string()), 2));
}

/// #4851 intact : la suivante posée d'avance (`clear="0"`) ne passe pas par
/// l'arrêt, même sur ce faux qui ignore `clear="1"` en lecture.
#[tokio::test]
async fn l_enchainement_sans_blanc_n_arrete_rien() {
    let (faux, sortie) = faux_qui_ignore_clear(2, 0).await;

    sortie.set_next_media(&media(SUIVANTE)).await.unwrap();

    assert!(
        !faux.recu().iter().any(|c| c.starts_with("<Stop")
            || c.starts_with("<Play ")
            || c.contains(r#"clear="1""#)),
        "{:?}",
        faux.recu()
    );
    assert_eq!(faux.playlist(), vec![COURANTE, SUIVANTE]);
    assert_eq!(faux.joue(), (Some(COURANTE.to_string()), 2));
}

/// #6115 (suite) — un faux HQPlayer réglé comme le moteur 6.0.4 mesuré par
/// le pont LMS-HQPlayer de Simon Arnold : `clear="1"` ignoré dans TOUS les
/// états (`PlaylistAdd` répond OK et ajoute), `PlaylistClear` qui vide.
/// Déjà sondé une fois, comme le fait le sondeur.
async fn faux_6_0_4(etat: u8) -> (FauxHqplayer, HqplayerOutput) {
    let faux = FauxHqplayer::en_lecture(COURANTE, 60.0, 300.0).await;
    faux.regler(|e| {
        e.ignore_clear_toujours = true;
        e.etat = etat;
    });
    let sortie = faux.sortie();
    let _ = sortie.get_status().await.unwrap();
    (faux, sortie)
}

/// LE CAS DE PIERO (fil 2240, rc4) : « If I select another song the previous
/// continue to play instead of the new. This is happening even if I stop the
/// previous one. » HQPlayer ARRÊTÉ, un autre titre : sans `PlaylistClear`, la
/// playlist devient [précédent, nouveau] et `Play` repart sur le précédent.
#[tokio::test]
async fn arrete_puis_un_autre_titre_joue_le_nouveau() {
    let (faux, sortie) = faux_6_0_4(0).await;

    sortie.play_media(&media(AUTRE)).await.unwrap();

    assert_eq!(
        faux.joue(),
        (Some(AUTRE.to_string()), 2),
        "#6115 à l'arrêt : HQPlayer rejoue le titre précédent ; `clear=\"1\"` \
         ignoré, la playlist n'a pas été vidée. Reçu {:?}",
        faux.recu()
    );
    assert_eq!(faux.playlist(), vec![AUTRE]);
    assert!(
        !faux.recu().iter().any(|c| c.starts_with("<Stop")),
        "à l'arrêt, toujours aucun `Stop` : {:?}",
        faux.recu()
    );
}

/// Même moteur, en pleine lecture puis en pause : l'arrêt de #6180 ne suffit
/// pas si `clear="1"` est ignoré aussi à l'arrêt.
#[tokio::test]
async fn en_lecture_ou_en_pause_un_autre_titre_joue_le_nouveau() {
    for etat in [2u8, 1] {
        let (faux, sortie) = faux_6_0_4(etat).await;

        sortie.play_media(&media(AUTRE)).await.unwrap();

        assert_eq!(
            faux.joue(),
            (Some(AUTRE.to_string()), 2),
            "état {etat} : reçu {:?}",
            faux.recu()
        );
        assert_eq!(faux.playlist(), vec![AUTRE], "état {etat}");
    }
}

/// L'ordre sur le fil, celui du chargement vérifié en direct par le pont
/// LMS : `Stop` (si une piste joue), `PlaylistClear`, `PlaylistAdd`, `Play`.
#[tokio::test]
async fn l_ordre_est_stop_playlistclear_playlistadd_play() {
    let (faux, sortie) = faux_6_0_4(2).await;

    sortie.play_media(&media(AUTRE)).await.unwrap();

    let recu = faux.recu();
    let transport: Vec<&str> = recu
        .iter()
        .map(String::as_str)
        .filter(|c| !c.starts_with("<Status"))
        .collect();
    assert_eq!(
        transport,
        vec![
            "<Stop />",
            "<PlaylistClear />",
            &format!(r#"<PlaylistAdd uri="{AUTRE}" queued="0" clear="1"></PlaylistAdd>"#),
            "<Play />",
        ],
        "{recu:?}"
    );
}
