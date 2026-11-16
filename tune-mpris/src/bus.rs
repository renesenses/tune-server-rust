//! Les deux interfaces MPRIS sur le bus D-Bus de la session (Linux).
//!
//! `org.mpris.MediaPlayer2` et `org.mpris.MediaPlayer2.Player`, objet
//! `/org/mpris/MediaPlayer2`, nom `org.mpris.MediaPlayer2.Tune` (spec MPRIS
//! 2.2). Les méthodes ne parlent pas au réseau elles-mêmes : elles passent
//! par le trait [`Commandes`], qu'un test remplace par un faux.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use zbus::fdo::RequestNameFlags;
use zbus::fdo::RequestNameReply;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{Connection, interface};

use crate::etat::{Changements, Meta};
use crate::requetes::{Action, Commandes};
use crate::synchro::{EtatPartage, Notifieur};

pub const CHEMIN: &str = "/org/mpris/MediaPlayer2";
pub const NOM: &str = "org.mpris.MediaPlayer2.Tune";

/// `org.mpris.MediaPlayer2.Tune.zone<id>` quand le nom court est déjà pris
/// (un second compagnon pour une autre zone) : la spec réserve ce suffixe
/// aux instances multiples.
pub fn nom_d_instance(zone_id: i64) -> String {
    format!("{NOM}.zone{zone_id}")
}

fn erreur(e: String) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e)
}

pub struct Racine {
    pub etat: EtatPartage,
    /// Ouvert par `Raise` : l'interface web du serveur.
    pub serveur: String,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Racine {
    /// Ouvre l'interface web du serveur dans le navigateur.
    fn raise(&self) {
        // Attendu dans un fil à part : ni blocage du bus, ni processus zombie.
        let serveur = self.serveur.clone();
        std::thread::spawn(move || {
            if let Err(e) = std::process::Command::new("xdg-open")
                .arg(&serveur)
                .status()
            {
                tracing::warn!("xdg-open {serveur} : {e}");
            }
        });
    }

    /// `CanQuit` est faux : le compagnon se lance avec la session.
    fn quit(&self) {}

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> String {
        let nom = self.etat.lock().expect("etat").nom_de_zone.clone();
        if nom.is_empty() {
            "Tune".to_string()
        } else {
            format!("Tune ({nom})")
        }
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

pub struct Lecteur {
    pub etat: EtatPartage,
    pub commandes: Arc<dyn Commandes>,
}

impl Lecteur {
    async fn executer(&self, action: Option<Action>) -> zbus::fdo::Result<()> {
        match action {
            Some(a) => self.commandes.envoyer(a).await.map_err(erreur),
            None => Ok(()),
        }
    }
}

fn en_variant(meta: &Meta) -> OwnedValue {
    let valeur =
        match meta {
            Meta::Texte(t) => Value::from(t.clone()),
            Meta::Textes(t) => Value::from(t.clone()),
            Meta::Entier(n) => Value::from(*n),
            Meta::Chemin(c) => Value::from(ObjectPath::try_from(c.clone()).unwrap_or_else(|_| {
                ObjectPath::from_static_str_unchecked(crate::etat::PAS_DE_PISTE)
            })),
        };
    valeur
        .try_to_owned()
        .expect("aucun descripteur de fichier dans les métadonnées")
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Lecteur {
    async fn next(&self) -> zbus::fdo::Result<()> {
        self.executer(Some(Action::Suivante)).await
    }

    async fn previous(&self) -> zbus::fdo::Result<()> {
        self.executer(Some(Action::Precedente)).await
    }

    async fn pause(&self) -> zbus::fdo::Result<()> {
        self.executer(Some(Action::Pause)).await
    }

    async fn play_pause(&self) -> zbus::fdo::Result<()> {
        self.executer(Some(Action::LecturePause)).await
    }

    async fn stop(&self) -> zbus::fdo::Result<()> {
        self.executer(Some(Action::Arret)).await
    }

    async fn play(&self) -> zbus::fdo::Result<()> {
        self.executer(Some(Action::Lecture)).await
    }

    /// Décalage relatif en microsecondes.
    async fn seek(&self, offset: i64) -> zbus::fdo::Result<()> {
        let action = self
            .etat
            .lock()
            .expect("etat")
            .action_de_saut(offset, Instant::now());
        self.executer(action).await
    }

    async fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> zbus::fdo::Result<()> {
        let action = self
            .etat
            .lock()
            .expect("etat")
            .action_de_position(track_id.as_str(), position);
        self.executer(action).await
    }

    async fn open_uri(&self, _uri: String) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "Tune ne lit pas d'URI depuis MPRIS".into(),
        ))
    }

    #[zbus(signal)]
    pub async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.etat.lock().expect("etat").statut.mpris().to_string()
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    /// `Rate` est en écriture dans la spec ; seule la vitesse 1 existe.
    #[zbus(property)]
    fn set_rate(&mut self, _rate: f64) {}

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        self.etat
            .lock()
            .expect("etat")
            .metadonnees()
            .iter()
            .map(|(cle, meta)| (cle.to_string(), en_variant(meta)))
            .collect()
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.etat.lock().expect("etat").volume_mpris()
    }

    #[zbus(property)]
    async fn set_volume(&mut self, volume: f64) -> zbus::fdo::Result<()> {
        let action = self.etat.lock().expect("etat").action_de_volume(volume);
        self.executer(Some(action.clone())).await?;
        // Publié tout de suite (le `PropertiesChanged` part au retour du
        // setter) ; `playback.volume` corrige si le serveur a plafonné (#6721).
        if let Action::Volume(gain) = action {
            self.etat.lock().expect("etat").gain = gain;
        }
        Ok(())
    }

    /// Position extrapolée en µs. Pas de `PropertiesChanged` (spec) : les
    /// sauts passent par `Seeked`.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.etat.lock().expect("etat").position_ms(Instant::now()) * 1000
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}

/// Sert les deux interfaces sur `connexion` et prend le nom MPRIS : le nom
/// court s'il est libre, sinon celui de l'instance de la zone. Rend le nom
/// obtenu.
pub async fn publier(
    connexion: &Connection,
    racine: Racine,
    lecteur: Lecteur,
    zone_id: i64,
) -> zbus::Result<String> {
    let serveur = connexion.object_server();
    serveur.at(CHEMIN, racine).await?;
    serveur.at(CHEMIN, lecteur).await?;
    if prendre(connexion, NOM).await? {
        return Ok(NOM.to_string());
    }
    let instance = nom_d_instance(zone_id);
    if prendre(connexion, &instance).await? {
        return Ok(instance);
    }
    Err(zbus::Error::Failure(format!(
        "nom {instance} déjà pris : un compagnon suit déjà cette zone"
    )))
}

/// `true` si le nom est à nous ; `false` s'il appartient à un autre (zbus
/// rend alors l'erreur `NameTaken`).
async fn prendre(connexion: &Connection, nom: &str) -> zbus::Result<bool> {
    match connexion
        .request_name_with_flags(nom, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Ok(true),
        Ok(_) | Err(zbus::Error::NameTaken) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Signale sur le bus ce qu'une mise à jour de l'état a changé.
pub struct NotifieurBus {
    pub connexion: Connection,
}

#[async_trait::async_trait]
impl Notifieur for NotifieurBus {
    async fn notifier(&self, c: Changements) {
        if let Err(e) = signaler(&self.connexion, &c).await {
            tracing::warn!("signal MPRIS non émis : {e}");
        }
    }
}

async fn signaler(connexion: &Connection, c: &Changements) -> zbus::Result<()> {
    let lecteur = connexion
        .object_server()
        .interface::<_, Lecteur>(CHEMIN)
        .await?;
    let emetteur = lecteur.signal_emitter();
    let l = lecteur.get().await;
    if c.statut {
        l.playback_status_changed(emetteur).await?;
    }
    if c.metadonnees {
        l.metadata_changed(emetteur).await?;
        // `Identity` suit le nom de la zone, relu avec la liste des zones.
        let racine = connexion
            .object_server()
            .interface::<_, Racine>(CHEMIN)
            .await?;
        racine
            .get()
            .await
            .identity_changed(racine.signal_emitter())
            .await?;
    }
    if c.volume {
        l.volume_changed(emetteur).await?;
    }
    if let Some(position) = c.saut {
        Lecteur::seeked(emetteur, position).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::etat::Etat;
    use serde_json::json;
    use std::sync::Mutex;

    /// Le faux de l'API : il retient les commandes au lieu de les envoyer.
    #[derive(Default)]
    pub struct FauxServeur {
        pub recues: Mutex<Vec<Action>>,
    }

    #[async_trait::async_trait]
    impl Commandes for FauxServeur {
        async fn envoyer(&self, action: Action) -> Result<(), String> {
            self.recues.lock().unwrap().push(action);
            Ok(())
        }
    }

    fn lecteur() -> (Lecteur, Arc<FauxServeur>) {
        let t0 = Instant::now();
        let mut e = Etat::new(4, "Salon", t0);
        e.appliquer_zone(
            &json!({
                "id": 4, "name": "Salon", "state": "paused", "position_ms": 60_000,
                "volume": 0.5, "volume_curve": "linear",
                "current_track": {"track_id": 1, "title": "Blue in Green", "artist_name": "Miles Davis",
                                  "album_title": "Kind of Blue", "duration_ms": 337_000}
            }),
            t0,
        );
        let faux = Arc::new(FauxServeur::default());
        (
            Lecteur {
                etat: Arc::new(Mutex::new(e)),
                commandes: faux.clone(),
            },
            faux,
        )
    }

    #[tokio::test]
    async fn les_touches_multimedia_deviennent_des_commandes_de_zone() {
        let (l, faux) = lecteur();
        l.play_pause().await.unwrap();
        l.next().await.unwrap();
        l.previous().await.unwrap();
        l.pause().await.unwrap();
        l.play().await.unwrap();
        l.stop().await.unwrap();
        assert_eq!(
            *faux.recues.lock().unwrap(),
            vec![
                Action::LecturePause,
                Action::Suivante,
                Action::Precedente,
                Action::Pause,
                Action::Lecture,
                Action::Arret
            ]
        );
    }

    #[tokio::test]
    async fn seek_set_position_et_volume() {
        let (mut l, faux) = lecteur();
        l.seek(15_000_000).await.unwrap();
        let id = l.etat.lock().unwrap().trackid();
        l.set_position(ObjectPath::try_from(id).unwrap(), 5_000_000)
            .await
            .unwrap();
        // Une piste périmée : ignorée sans erreur.
        l.set_position(
            ObjectPath::try_from("/org/tune/zone4/piste/99").unwrap(),
            5_000_000,
        )
        .await
        .unwrap();
        l.set_volume(0.3).await.unwrap();
        assert_eq!(
            *faux.recues.lock().unwrap(),
            vec![
                Action::Position(75_000),
                Action::Position(5_000),
                Action::Volume(0.3)
            ]
        );
        assert!(l.open_uri("file:///x.flac".into()).await.is_err());
    }

    #[test]
    fn proprietes_lues_dans_l_etat() {
        let (l, _) = lecteur();
        assert_eq!(l.playback_status(), "Paused");
        assert_eq!(l.position(), 60_000_000);
        assert!((l.volume() - 0.5).abs() < 1e-9);
        let m = l.metadata();
        assert_eq!(
            String::try_from(m["xesam:title"].clone()).unwrap(),
            "Blue in Green"
        );
        assert_eq!(
            <Vec<String>>::try_from(m["xesam:artist"].clone()).unwrap(),
            vec!["Miles Davis".to_string()]
        );
        assert_eq!(
            i64::try_from(m["mpris:length"].clone()).unwrap(),
            337_000_000
        );
        let id = ObjectPath::try_from(m["mpris:trackid"].clone()).unwrap();
        assert_eq!(id.as_str(), "/org/tune/zone4/piste/1");
        let r = Racine {
            etat: l.etat.clone(),
            serveur: "http://localhost:8888".into(),
        };
        assert_eq!(r.identity(), "Tune (Salon)");
        assert!(!r.can_quit());
    }

    #[test]
    fn nom_d_instance_par_zone() {
        assert_eq!(nom_d_instance(4), "org.mpris.MediaPlayer2.Tune.zone4");
    }
}
