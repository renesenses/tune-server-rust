//! #8565 (fil 2778) — une session lancée depuis l'application Spotify joue
//! dans la zone exposée, et la suit.
use super::*;

fn ev(evenement: &str) -> EvenementLibrespot {
    EvenementLibrespot {
        evenement: evenement.into(),
        ..Default::default()
    }
}

fn titre(piste: &str, nom: &str) -> EvenementLibrespot {
    EvenementLibrespot {
        evenement: "track_changed".into(),
        piste: Some(piste.into()),
        titre: Some(nom.into()),
        artistes: Some("Artiste".into()),
        duree_ms: Some(200_000),
        ..Default::default()
    }
}

fn joue(piste: &str) -> EvenementLibrespot {
    EvenementLibrespot {
        evenement: "playing".into(),
        piste: Some(piste.into()),
        ..Default::default()
    }
}

fn volume(v: u16) -> EvenementLibrespot {
    EvenementLibrespot {
        evenement: "volume_changed".into(),
        volume: Some(v),
        ..Default::default()
    }
}

#[test]
fn decider_8565_playing_ouvre_la_zone_exposee_puis_suit_la_session() {
    let mut e = EtatSession::default();
    let t0 = Instant::now();
    assert!(decider(&mut e, &titre("a", "Un"), Some(7), false, t0).is_empty());
    let ouvrir = decider(&mut e, &joue("a"), Some(7), false, t0);
    assert!(
        matches!(&ouvrir[..], [Action::Ouvrir { zone_id: 7, morceau }] if morceau.titre.as_deref() == Some("Un")),
        "{ouvrir:?}"
    );
    assert_eq!(
        decider(&mut e, &ev("paused"), Some(7), false, t0),
        vec![Action::Pause { zone_id: 7 }]
    );
    assert!(
        decider(&mut e, &ev("paused"), Some(7), false, t0).is_empty(),
        "une seule pause"
    );
    assert_eq!(
        decider(&mut e, &joue("a"), Some(7), false, t0),
        vec![Action::Reprise { zone_id: 7 }]
    );
    // Suivant, depuis l'application : le titre change, la session reste.
    let suivant = decider(&mut e, &titre("b", "Deux"), Some(7), false, t0);
    assert!(
        matches!(&suivant[..], [Action::Morceau { zone_id: 7, morceau }] if morceau.titre.as_deref() == Some("Deux")),
        "{suivant:?}"
    );
    assert!(decider(&mut e, &joue("b"), Some(7), false, t0).is_empty());
    assert!(decider(&mut e, &ev("seeked"), Some(7), false, t0).is_empty());
    assert!(decider(&mut e, &ev("end_of_track"), Some(7), false, t0).is_empty());
    assert_eq!(
        decider(&mut e, &ev("stopped"), Some(7), false, t0),
        vec![Action::Arreter { zone_id: 7 }]
    );
    assert_eq!(e.zone(), None);
}

#[test]
fn decider_8565_le_volume_de_la_connexion_n_est_jamais_applique() {
    let mut e = EtatSession::default();
    let t0 = Instant::now();
    // Avant l'ouverture : rien.
    assert!(decider(&mut e, &volume(u16::MAX), Some(3), false, t0).is_empty());
    decider(&mut e, &joue("a"), Some(3), false, t0);
    // Juste après : le volume mémorisé par l'application, ignoré.
    assert!(decider(&mut e, &volume(u16::MAX), Some(3), false, t0).is_empty());
    // Ensuite : appliqué, ramené entre 0 et 1.
    let plus_tard = t0 + DELAI_AVANT_VOLUME;
    let a = decider(&mut e, &volume(u16::MAX / 2), Some(3), false, plus_tard);
    match &a[..] {
        [Action::Volume { zone_id: 3, volume }] => assert!((volume - 0.5).abs() < 0.001),
        autre => panic!("{autre:?}"),
    }
}

#[test]
fn decider_8565_une_lecture_lancee_par_tune_garde_la_main() {
    let mut e = EtatSession::default();
    let t0 = Instant::now();
    assert!(decider(&mut e, &joue("a"), Some(7), true, t0).is_empty());
    decider(&mut e, &joue("a"), Some(7), false, t0);
    assert_eq!(
        decider(&mut e, &joue("b"), Some(7), true, t0),
        vec![Action::Arreter { zone_id: 7 }]
    );
}

#[test]
fn decider_8565_recepteur_desactive_ou_zone_changee_ferme_la_session() {
    let mut e = EtatSession::default();
    let t0 = Instant::now();
    assert!(
        decider(&mut e, &joue("a"), None, false, t0).is_empty(),
        "récepteur désactivé : rien"
    );
    decider(&mut e, &joue("a"), Some(7), false, t0);
    assert_eq!(
        decider(&mut e, &ev("paused"), Some(8), false, t0),
        vec![Action::Arreter { zone_id: 7 }]
    );
    let r = decider(&mut e, &joue("a"), Some(8), false, t0);
    assert!(
        matches!(&r[..], [Action::Ouvrir { zone_id: 8, .. }]),
        "{r:?}"
    );
}

#[cfg(unix)]
mod avec_un_faux_librespot {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex as StdMutex;

    /// Un faux librespot : exécute, ligne à ligne, les ordres posés dans le
    /// fichier `ordres` — `ev <EVENEMENT> <PISTE> <VOLUME>` publie un
    /// évènement par `--onevent`, `pcm <N>` écrit N octets 0x55.
    const FAUX: &str = r#"#!/bin/sh
D=$(dirname "$0")
printf '%s\n' "$*" > "$D/args"
EV=""
while [ $# -gt 0 ]; do
  case "$1" in
    --onevent) EV="$2"; shift ;;
  esac
  shift
done
while :; do
  if [ -f "$D/ordres" ]; then
    mv "$D/ordres" "$D/encours"
    while read A B C E; do
      case "$A" in
        ev) PLAYER_EVENT="$B" TRACK_ID="$C" VOLUME="$E" NAME="Titre $C" DURATION_MS=1000 $EV ;;
        pcm) head -c "$B" /dev/zero | tr '\000' '\125' ;;
      esac
    done < "$D/encours"
    rm -f "$D/encours"
  fi
  sleep 0.05
done
"#;

    #[derive(Default)]
    struct FaussesZones {
        appels: StdMutex<Vec<String>>,
        refuser: bool,
    }

    #[async_trait]
    impl ZonesConnect for Arc<FaussesZones> {
        async fn ouvrir(&self, zone_id: i64, m: &Morceau) -> Result<(), String> {
            self.appels.lock().unwrap().push(format!(
                "ouvrir {zone_id} {}",
                m.titre.clone().unwrap_or_default()
            ));
            if self.refuser {
                Err("zone absente".into())
            } else {
                Ok(())
            }
        }
        async fn joue_la_session(&self, _: i64) -> bool {
            true
        }
        async fn morceau(&self, zone_id: i64, m: &Morceau) {
            self.appels.lock().unwrap().push(format!(
                "morceau {zone_id} {}",
                m.titre.clone().unwrap_or_default()
            ));
        }
        async fn pause(&self, zone_id: i64) {
            self.appels.lock().unwrap().push(format!("pause {zone_id}"));
        }
        async fn reprise(&self, zone_id: i64) {
            self.appels
                .lock()
                .unwrap()
                .push(format!("reprise {zone_id}"));
        }
        async fn arreter(&self, zone_id: i64) {
            self.appels
                .lock()
                .unwrap()
                .push(format!("arreter {zone_id}"));
        }
        async fn volume(&self, zone_id: i64, v: f64) {
            self.appels
                .lock()
                .unwrap()
                .push(format!("volume {zone_id} {v:.2}"));
        }
    }

    fn recepteur(dossier: &std::path::Path) -> Arc<SpotifyConnectManager> {
        let bin = dossier.join("librespot");
        std::fs::write(&bin, FAUX).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Arc::new(SpotifyConnectManager::avec_binaire(
            "Tune".into(),
            0,
            Some(bin.to_string_lossy().into_owned()),
        ))
    }

    fn ordonner(dossier: &std::path::Path, ordres: &str) {
        let tmp = dossier.join("ordres.tmp");
        std::fs::write(&tmp, ordres).unwrap();
        std::fs::rename(&tmp, dossier.join("ordres")).unwrap();
    }

    async fn attendre_appels(z: &FaussesZones, n: usize) -> Vec<String> {
        for _ in 0..200 {
            if z.appels.lock().unwrap().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        z.appels.lock().unwrap().clone()
    }

    /// De bout en bout : les évènements d'un (faux) librespot pilotent la
    /// zone exposée — ouverture, titre suivant, pause, reprise, arrêt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_8565_les_evenements_de_l_application_pilotent_la_zone_exposee() {
        let dossier = tempfile::tempdir().unwrap();
        let recepteur = recepteur(dossier.path());
        let zones = Arc::new(FaussesZones::default());
        let (tx, rx) = mpsc::unbounded_channel();
        recepteur.brancher_les_evenements(tx);
        tokio::spawn(
            PiloteConnect {
                recepteur: recepteur.clone(),
                zones: zones.clone(),
            }
            .piloter(rx),
        );
        recepteur.enable(42).await.unwrap();
        let args = std::fs::read_to_string(dossier.path().join("args")).unwrap_or_default();
        ordonner(
            dossier.path(),
            "ev track_changed a\nev playing a\nev track_changed b\nev paused b\nev playing b\nev stopped b\n",
        );
        let appels = attendre_appels(&zones, 5).await;
        recepteur.disable().await;
        assert_eq!(
            appels,
            vec![
                "ouvrir 42 Titre a",
                "morceau 42 Titre b",
                "pause 42",
                "reprise 42",
                "arreter 42",
            ],
            "args de librespot : {args}"
        );
    }

    /// Zone exposée absente : la raison est retenue et rendue par le statut.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_8565_zone_absente_le_statut_le_dit() {
        let dossier = tempfile::tempdir().unwrap();
        let recepteur = recepteur(dossier.path());
        let zones = Arc::new(FaussesZones {
            refuser: true,
            ..Default::default()
        });
        let pilote = PiloteConnect {
            recepteur: recepteur.clone(),
            zones: zones.clone(),
        };
        recepteur.enable(9).await.unwrap();
        let mut etat = EtatSession::default();
        pilote.traiter(&mut etat, &joue("a")).await;
        recepteur.disable().await;
        assert_eq!(etat.zone(), None, "la session ne reste pas ouverte");
        assert_eq!(
            recepteur.status().await["session_error"],
            "zone absente",
            "le statut dit pourquoi rien ne joue"
        );
    }

    /// La session est UN flux continu : le PCM de librespot traverse la fin
    /// d'un titre, et du silence comble ses blancs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_8565_le_flux_traverse_les_titres_et_comble_les_blancs() {
        let dossier = tempfile::tempdir().unwrap();
        let recepteur = recepteur(dossier.path());
        recepteur.enable(1).await.unwrap();
        let fournisseur = FournisseurConnect::new(recepteur.clone());
        assert!(fournisseur.en_direct());
        let flux = tokio::task::spawn_blocking(move || {
            fournisseur.ouvrir_direct(SOURCE_ID, Consommation::new(|| 0))
        })
        .await
        .unwrap()
        .unwrap();
        assert!(recepteur.pompe().consommateur_continu());
        assert!(!recepteur.pompe().consommateur_de_tune());
        let un = OCTETS_PAR_SECONDE / 4;
        ordonner(
            dossier.path(),
            &format!("pcm {un}\nev end_of_track a\npcm {un}\n"),
        );
        let mut lecteur = flux.lecteur;
        let lu = tokio::task::spawn_blocking(move || {
            let mut pcm = 0usize;
            let mut silence = 0usize;
            let mut tampon = vec![0u8; 8192];
            let debut = Instant::now();
            while pcm < 2 * un && debut.elapsed() < Duration::from_secs(10) {
                let n = lecteur.read(&mut tampon).unwrap();
                assert!(n > 0, "le flux ne se ferme pas à la fin d'un titre");
                assert_eq!(n % OCTETS_PAR_TRAME, 0);
                pcm += tampon[..n].iter().filter(|&&o| o == 0x55).count();
                silence += tampon[..n].iter().filter(|&&o| o == 0).count();
            }
            (pcm, silence)
        })
        .await
        .unwrap();
        recepteur.disable().await;
        assert_eq!(lu.0, 2 * un, "les deux titres arrivent en entier");
        assert!(lu.1 > 0, "du silence comble l'attente du premier octet");
    }
}
