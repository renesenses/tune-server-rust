//! Bout en bout sur un VRAI `dbus-daemon`, mais privé : lancé par l'essai
//! dans un dossier temporaire, jamais le bus de la session. Sauté (avec un
//! message) si `dbus-daemon` n'est pas installé, sauf avec
//! `TUNE_MPRIS_DBUS_REQUIS=1` (la CI) : son absence fait alors échouer l'essai.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::json;
use tune_mpris::bus::{CHEMIN, Lecteur, NOM, NotifieurBus, Racine, publier};
use tune_mpris::etat::{Changements, Etat};
use tune_mpris::requetes::{Action, Commandes};
use tune_mpris::synchro::Notifieur;
use zbus::connection::Builder;
use zbus::proxy::CacheProperties;
use zbus::{ObjectPath, OwnedVariant};

struct BusPrive {
    processus: Child,
    adresse: String,
    dossier: std::path::PathBuf,
}

impl Drop for BusPrive {
    fn drop(&mut self) {
        let _ = self.processus.kill();
        let _ = self.processus.wait();
        let _ = std::fs::remove_dir_all(&self.dossier);
    }
}

fn lancer_bus() -> Option<BusPrive> {
    let dossier = std::env::temp_dir().join(format!("tune-mpris-bus-{}", std::process::id()));
    std::fs::create_dir_all(&dossier).ok()?;
    let config = dossier.join("bus.conf");
    std::fs::write(
        &config,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
            dossier.display()
        ),
    )
    .ok()?;
    let mut processus = match Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("dbus-daemon absent ({e}) : essai sauté");
            return None;
        }
    };
    let mut adresse = String::new();
    BufReader::new(processus.stdout.take()?)
        .read_line(&mut adresse)
        .ok()?;
    Some(BusPrive {
        processus,
        adresse: adresse.trim().to_string(),
        dossier,
    })
}

#[derive(Default)]
struct FauxServeur {
    recues: Mutex<Vec<Action>>,
}

#[async_trait::async_trait]
impl Commandes for FauxServeur {
    async fn envoyer(&self, action: Action) -> Result<(), String> {
        self.recues.lock().unwrap().push(action);
        Ok(())
    }
}

fn etat(zone_id: i64) -> Arc<Mutex<Etat>> {
    let t0 = Instant::now();
    let mut e = Etat::new(zone_id, "Salon", t0);
    e.appliquer_zone(
        &json!({
            "id": zone_id, "name": "Salon", "state": "paused", "position_ms": 10_000,
            "volume": 0.4, "volume_curve": "linear",
            "current_track": {"track_id": 3, "title": "All Blues", "artist_name": "Miles Davis",
                              "album_title": "Kind of Blue", "duration_ms": 693_000}
        }),
        t0,
    );
    Arc::new(Mutex::new(e))
}

async fn servir(
    adresse: &str,
    zone_id: i64,
) -> (zbus::Connection, String, Arc<Mutex<Etat>>, Arc<FauxServeur>) {
    let connexion = Builder::address(adresse).build().await.unwrap();
    let e = etat(zone_id);
    let faux = Arc::new(FauxServeur::default());
    let nom = publier(
        &connexion,
        Racine {
            etat: e.clone(),
            serveur: "http://localhost:8888".into(),
        },
        Lecteur {
            etat: e.clone(),
            commandes: faux.clone(),
        },
        zone_id,
    )
    .await
    .unwrap();
    (connexion, nom, e, faux)
}

#[tokio::test]
async fn un_client_mpris_lit_et_pilote_la_zone_sur_un_bus_prive() {
    let Some(bus) = lancer_bus() else {
        assert_ne!(
            std::env::var("TUNE_MPRIS_DBUS_REQUIS").as_deref(),
            Ok("1"),
            "dbus-daemon introuvable alors que TUNE_MPRIS_DBUS_REQUIS=1 : l'essai ne doit pas passer à vide"
        );
        return;
    };
    let (connexion, nom, etat, faux) = servir(&bus.adresse, 4).await;
    assert_eq!(nom, NOM);

    let client = Builder::address(bus.adresse.as_str())
        .build()
        .await
        .unwrap();
    let lecteur = zbus::proxy::Builder::<zbus::Proxy>::new(&client)
        .destination(NOM)
        .path(CHEMIN)
        .interface("org.mpris.MediaPlayer2.Player")
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    let racine = zbus::proxy::Builder::<zbus::Proxy>::new(&client)
        .destination(NOM)
        .path(CHEMIN)
        .interface("org.mpris.MediaPlayer2")
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();

    // Ce que `playerctl metadata` et les applets lisent.
    let identite: String = racine.get_property("Identity").await.unwrap();
    assert_eq!(identite, "Tune (Salon)");
    let statut: String = lecteur.get_property("PlaybackStatus").await.unwrap();
    assert_eq!(statut, "Paused");
    let meta: HashMap<String, OwnedVariant> = lecteur.get_property("Metadata").await.unwrap();
    assert_eq!(
        String::try_from(meta["xesam:title"].clone()).unwrap(),
        "All Blues"
    );
    assert_eq!(
        i64::try_from(meta["mpris:length"].clone()).unwrap(),
        693_000_000
    );
    let trackid = ObjectPath::try_from(meta["mpris:trackid"].clone())
        .unwrap()
        .into_owned();
    let position: i64 = lecteur.get_property("Position").await.unwrap();
    assert_eq!(position, 10_000_000);

    // Ce que les touches multimédia et `playerctl play-pause|next|…` appellent.
    lecteur.call_method("PlayPause", &()).await.unwrap();
    lecteur.call_method("Next", &()).await.unwrap();
    lecteur.call_method("Seek", &(5_000_000i64)).await.unwrap();
    lecteur
        .call_method("SetPosition", &(trackid, 20_000_000i64))
        .await
        .unwrap();
    lecteur.set_property("Volume", 0.8f64).await.unwrap();
    let volume: f64 = lecteur.get_property("Volume").await.unwrap();
    assert!(
        (volume - 0.8).abs() < 1e-9,
        "volume publié tout de suite : {volume}"
    );
    assert_eq!(
        *faux.recues.lock().unwrap(),
        vec![
            Action::LecturePause,
            Action::Suivante,
            Action::Position(15_000),
            Action::Position(20_000),
            Action::Volume(0.8),
        ]
    );

    // Un évènement du serveur devient `PropertiesChanged` puis `Seeked`.
    let proprietes = zbus::fdo::PropertiesProxy::builder(&client)
        .destination(NOM)
        .path(CHEMIN)
        .build()
        .await
        .unwrap();
    let mut changements = proprietes.receive_properties_changed().await.unwrap();
    let mut sauts = lecteur.receive_signal("Seeked").await.unwrap();
    let notifieur = NotifieurBus {
        connexion: connexion.clone(),
    };
    let c = etat
        .lock()
        .unwrap()
        .appliquer_evenement(
            &json!({"type": "playback.resumed", "data": {"zone_id": 4}}),
            Instant::now(),
        )
        .unwrap();
    notifieur.notifier(c).await;
    let signal = tokio::time::timeout(Duration::from_secs(5), changements.next())
        .await
        .expect("PropertiesChanged attendu")
        .unwrap();
    let args = signal.args().unwrap();
    assert_eq!(
        args.interface_name().as_str(),
        "org.mpris.MediaPlayer2.Player"
    );
    let statut = args
        .changed_properties()
        .get("PlaybackStatus")
        .expect("PlaybackStatus");
    assert_eq!(
        String::try_from(statut.try_to_owned().unwrap()).unwrap(),
        "Playing"
    );

    notifieur
        .notifier(Changements {
            saut: Some(42_000_000),
            ..Default::default()
        })
        .await;
    let saut = tokio::time::timeout(Duration::from_secs(5), sauts.next())
        .await
        .expect("Seeked attendu")
        .unwrap();
    let position: i64 = saut.body().deserialize().unwrap();
    assert_eq!(position, 42_000_000);

    // Un second compagnon, pour une autre zone, prend le nom d'instance.
    let (_autre, nom, _, _) = servir(&bus.adresse, 7).await;
    assert_eq!(nom, "org.mpris.MediaPlayer2.Tune.zone7");
}
