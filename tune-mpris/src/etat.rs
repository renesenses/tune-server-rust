//! L'état de la zone suivie, tel que MPRIS le publie.
//!
//! Alimenté par `GET /api/v1/zones` (au démarrage et à chaque reconnexion)
//! puis par les évènements `playback.*` du WebSocket. Rien ici ne parle à
//! D-Bus ni au réseau : tout se teste avec du JSON et un `Instant`.

use std::time::Instant;

use serde_json::Value;

use crate::requetes::Action;
use crate::volume::{self, Courbe};

/// `mpris:trackid` quand rien ne joue (spec MPRIS 2.2).
pub const PAS_DE_PISTE: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statut {
    Lecture,
    Pause,
    Arret,
}

impl Statut {
    pub fn depuis(texte: Option<&str>) -> Self {
        match texte {
            Some("playing") => Statut::Lecture,
            Some("paused") => Statut::Pause,
            _ => Statut::Arret,
        }
    }

    /// `PlaybackStatus` MPRIS.
    pub fn mpris(self) -> &'static str {
        match self {
            Statut::Lecture => "Playing",
            Statut::Pause => "Paused",
            Statut::Arret => "Stopped",
        }
    }
}

/// La piste en cours (sous-ensemble de `NowPlaying`, `tune_core::playback`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Piste {
    pub titre: String,
    pub artiste: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duree_ms: i64,
    pub cover_path: Option<String>,
    pub track_id: Option<i64>,
    pub source_id: Option<String>,
}

fn texte(v: &Value, cle: &str) -> Option<String> {
    v.get(cle)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Piste {
    /// Lit un `NowPlaying` sérialisé. `None` pour une charge vide ou le
    /// titre d'attente « Recovering... » que le serveur pose pendant une
    /// reprise (même filtre que le mini-lecteur).
    pub fn depuis_json(v: &Value) -> Option<Piste> {
        if !v.is_object() {
            return None;
        }
        let titre = texte(v, "title").unwrap_or_default();
        let track_id = v.get("track_id").and_then(Value::as_i64);
        if titre == "Recovering..." || (titre.is_empty() && track_id.is_none()) {
            return None;
        }
        Some(Piste {
            titre,
            artiste: texte(v, "artist_name"),
            album: texte(v, "album_title"),
            genre: texte(v, "genre"),
            duree_ms: v
                .get("duration_ms")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0),
            cover_path: texte(v, "cover_path"),
            track_id,
            source_id: texte(v, "source_id"),
        })
    }

    /// Même morceau ? Une radio change de titre sans changer d'identifiant,
    /// une durée découverte en cours (#5550) ne change pas le morceau.
    fn meme_morceau(&self, autre: &Piste) -> bool {
        self.track_id == autre.track_id
            && self.source_id == autre.source_id
            && self.titre == autre.titre
            && self.artiste == autre.artiste
    }
}

/// Une valeur de `Metadata`, sans dépendre de D-Bus.
#[derive(Debug, Clone, PartialEq)]
pub enum Meta {
    Texte(String),
    Textes(Vec<String>),
    Entier(i64),
    Chemin(String),
}

/// Ce qu'une mise à jour a changé, donc ce qu'il faut signaler sur le bus.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changements {
    pub statut: bool,
    pub metadonnees: bool,
    pub volume: bool,
    /// `Seeked(position en µs)` à émettre.
    pub saut: Option<i64>,
}

impl Changements {
    pub fn rien(&self) -> bool {
        *self == Changements::default()
    }
}

#[derive(Debug, Clone)]
pub struct Etat {
    pub zone_id: i64,
    pub nom_de_zone: String,
    pub statut: Statut,
    pub piste: Option<Piste>,
    /// Numéro de la piste pour `mpris:trackid` : change à chaque morceau.
    pub numero: u64,
    /// `file://` (ou `http://`) de la pochette, une fois obtenue.
    pub pochette: Option<String>,
    position_ms: i64,
    ancre: Instant,
    pub gain: f64,
    pub courbe: Courbe,
}

impl Etat {
    pub fn new(zone_id: i64, nom_de_zone: impl Into<String>, maintenant: Instant) -> Self {
        Etat {
            zone_id,
            nom_de_zone: nom_de_zone.into(),
            statut: Statut::Arret,
            piste: None,
            numero: 0,
            pochette: None,
            position_ms: 0,
            ancre: maintenant,
            gain: 0.0,
            courbe: Courbe::Lineaire,
        }
    }

    /// Position extrapolée : le serveur n'envoie `playback.position` que de
    /// loin en loin, l'applet la redemande quand il veut.
    pub fn position_ms(&self, maintenant: Instant) -> i64 {
        let mut p = self.position_ms;
        if self.statut == Statut::Lecture {
            p += maintenant.saturating_duration_since(self.ancre).as_millis() as i64;
        }
        match &self.piste {
            Some(piste) if piste.duree_ms > 0 => p.clamp(0, piste.duree_ms),
            _ => p.max(0),
        }
    }

    fn poser_position(&mut self, ms: i64, maintenant: Instant) {
        self.position_ms = ms.max(0);
        self.ancre = maintenant;
    }

    fn poser_statut(&mut self, statut: Statut, maintenant: Instant) -> bool {
        if self.statut == statut {
            return false;
        }
        // Figer la position courue avant de changer de régime.
        let p = self.position_ms(maintenant);
        self.poser_position(p, maintenant);
        self.statut = statut;
        true
    }

    /// Rend `true` si les métadonnées publiées changent.
    fn poser_piste(&mut self, piste: Option<Piste>, maintenant: Instant) -> bool {
        if self.piste == piste {
            return false;
        }
        let nouveau_morceau = match (&self.piste, &piste) {
            (Some(a), Some(b)) => !a.meme_morceau(b),
            (None, None) => false,
            _ => true,
        };
        let autre_pochette = self.piste.as_ref().and_then(|p| p.cover_path.as_ref())
            != piste.as_ref().and_then(|p| p.cover_path.as_ref());
        if nouveau_morceau {
            self.numero += 1;
            self.poser_position(0, maintenant);
        }
        if autre_pochette {
            self.pochette = None;
        }
        self.piste = piste;
        true
    }

    fn poser_gain(&mut self, gain: f64) -> bool {
        if (self.gain - gain).abs() < 1e-12 {
            return false;
        }
        self.gain = gain;
        true
    }

    /// Une entrée de `GET /api/v1/zones` (champs `state`, `current_track`,
    /// `position_ms`, `volume`, `volume_curve`, `name`).
    pub fn appliquer_zone(&mut self, zone: &Value, maintenant: Instant) -> Changements {
        let mut c = Changements::default();
        if let Some(nom) = texte(zone, "name")
            && nom != self.nom_de_zone
        {
            self.nom_de_zone = nom;
        }
        self.courbe = Courbe::depuis(zone.get("volume_curve").and_then(Value::as_str));
        let piste = zone.get("current_track").and_then(Piste::depuis_json);
        c.metadonnees = self.poser_piste(piste, maintenant);
        let statut = Statut::depuis(zone.get("state").and_then(Value::as_str));
        c.statut = self.poser_statut(statut, maintenant);
        if let Some(p) = zone.get("position_ms").and_then(Value::as_i64) {
            self.poser_position(p, maintenant);
        }
        if let Some(g) = zone.get("volume").and_then(Value::as_f64) {
            c.volume = self.poser_gain(g);
        }
        c
    }

    /// Un message du WebSocket. `None` s'il ne concerne pas cette zone ou
    /// n'est pas un évènement de lecture. Lecture par type EXACT, comme
    /// `lireEvenement` du mini-lecteur : `zone.volume_externe` (#5662) n'a
    /// pas de champ `volume` et ne doit rien remettre à zéro.
    pub fn appliquer_evenement(
        &mut self,
        message: &Value,
        maintenant: Instant,
    ) -> Option<Changements> {
        let genre = message.get("type")?.as_str()?;
        let data = message.get("data").filter(|d| d.is_object())?;
        if data.get("zone_id").and_then(Value::as_i64) != Some(self.zone_id) {
            return None;
        }
        let mut c = Changements::default();
        match genre {
            "playback.started" => {
                c.metadonnees = self.poser_piste(Piste::depuis_json(data), maintenant);
                c.statut = self.poser_statut(Statut::Lecture, maintenant);
                self.poser_position(0, maintenant);
            }
            "playback.track_changed" => {
                c.metadonnees = self.poser_piste(Piste::depuis_json(data), maintenant);
                if self.statut == Statut::Arret && self.piste.is_some() {
                    c.statut = self.poser_statut(Statut::Lecture, maintenant);
                }
            }
            "playback.paused" => c.statut = self.poser_statut(Statut::Pause, maintenant),
            "playback.resumed" => c.statut = self.poser_statut(Statut::Lecture, maintenant),
            "playback.stopped" => {
                c.statut = self.poser_statut(Statut::Arret, maintenant);
                self.poser_position(0, maintenant);
            }
            "playback.position" => {
                let p = data.get("position_ms")?.as_i64()?;
                self.poser_position(p, maintenant);
            }
            "playback.seek" => {
                let p = data.get("position_ms")?.as_i64()?;
                self.poser_position(p, maintenant);
                c.saut = Some(self.position_ms(maintenant) * 1000);
            }
            "playback.volume" => {
                let g = data.get("volume")?.as_f64()?;
                c.volume = self.poser_gain(g);
            }
            _ => return None,
        }
        Some(c)
    }

    /// Le serveur ne répond plus : rien ne joue, plus de métadonnées.
    pub fn deconnecte(&mut self, maintenant: Instant) -> Changements {
        let c = Changements {
            metadonnees: self.poser_piste(None, maintenant),
            statut: self.poser_statut(Statut::Arret, maintenant),
            ..Default::default()
        };
        self.poser_position(0, maintenant);
        c
    }

    pub fn trackid(&self) -> String {
        match &self.piste {
            Some(_) => format!("/org/tune/zone{}/piste/{}", self.zone_id, self.numero),
            None => PAS_DE_PISTE.to_string(),
        }
    }

    /// `Metadata` MPRIS (`mpris:*` et `xesam:*`).
    pub fn metadonnees(&self) -> Vec<(&'static str, Meta)> {
        let mut m = vec![("mpris:trackid", Meta::Chemin(self.trackid()))];
        let Some(p) = &self.piste else {
            return m;
        };
        if p.duree_ms > 0 {
            m.push(("mpris:length", Meta::Entier(p.duree_ms * 1000)));
        }
        if !p.titre.is_empty() {
            m.push(("xesam:title", Meta::Texte(p.titre.clone())));
        }
        if let Some(a) = &p.artiste {
            m.push(("xesam:artist", Meta::Textes(vec![a.clone()])));
        }
        if let Some(a) = &p.album {
            m.push(("xesam:album", Meta::Texte(a.clone())));
        }
        if let Some(g) = &p.genre {
            m.push(("xesam:genre", Meta::Textes(vec![g.clone()])));
        }
        if let Some(u) = &self.pochette {
            m.push(("mpris:artUrl", Meta::Texte(u.clone())));
        }
        m
    }

    /// `Volume` MPRIS : la position du curseur.
    pub fn volume_mpris(&self) -> f64 {
        volume::position_du_gain(self.gain, self.courbe)
    }

    /// `Volume` demandé par le bureau → la commande à envoyer.
    pub fn action_de_volume(&self, position: f64) -> Action {
        Action::Volume(volume::gain_de_position(position, self.courbe))
    }

    /// `Seek(décalage en µs)`. Au-delà de la fin, la spec veut la piste
    /// suivante ; avant le début, le début.
    pub fn action_de_saut(&self, decalage_us: i64, maintenant: Instant) -> Option<Action> {
        self.piste.as_ref()?;
        let cible = self
            .position_ms(maintenant)
            .saturating_add(decalage_us / 1000);
        let duree = self.piste.as_ref().map(|p| p.duree_ms).unwrap_or(0);
        if duree > 0 && cible >= duree {
            return Some(Action::Suivante);
        }
        Some(Action::Position(cible.max(0)))
    }

    /// `SetPosition(trackid, position en µs)` : ignorée si la piste a changé
    /// entre-temps ou si la position sort de la piste (spec MPRIS).
    pub fn action_de_position(&self, trackid: &str, position_us: i64) -> Option<Action> {
        let piste = self.piste.as_ref()?;
        if trackid != self.trackid() || position_us < 0 {
            return None;
        }
        let ms = position_us / 1000;
        if piste.duree_ms > 0 && ms > piste.duree_ms {
            return None;
        }
        Some(Action::Position(ms))
    }

    /// L'URL à télécharger pour la pochette de la piste en cours.
    pub fn pochette_a_chercher(&self, serveur: &str) -> Option<String> {
        if self.pochette.is_some() {
            return None;
        }
        let chemin = self.piste.as_ref()?.cover_path.as_deref()?;
        crate::pochette::url_de_pochette(serveur, chemin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn zone(state: &str) -> Value {
        json!({
            "id": 2, "name": "Salon", "state": state, "position_ms": 30_000,
            "volume": 0.5, "volume_curve": "linear",
            "current_track": {
                "track_id": 7, "title": "So What", "artist_name": "Miles Davis",
                "album_title": "Kind of Blue", "genre": "Jazz", "duration_ms": 545_000,
                "cover_path": "abc123", "source": "local"
            }
        })
    }

    fn ev(genre: &str, data: Value) -> Value {
        json!({ "type": genre, "data": data })
    }

    #[test]
    fn la_zone_initiale_donne_statut_metadonnees_et_volume() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "?", t0);
        let c = e.appliquer_zone(&zone("playing"), t0);
        assert!(c.statut && c.metadonnees && c.volume);
        assert_eq!(e.nom_de_zone, "Salon");
        assert_eq!(e.statut.mpris(), "Playing");
        assert_eq!(e.position_ms(t0), 30_000);
        assert_eq!(e.position_ms(t0 + Duration::from_secs(2)), 32_000);
        let m = e.metadonnees();
        assert!(m.contains(&("xesam:title", Meta::Texte("So What".into()))));
        assert!(m.contains(&("xesam:artist", Meta::Textes(vec!["Miles Davis".into()]))));
        assert!(m.contains(&("xesam:album", Meta::Texte("Kind of Blue".into()))));
        assert!(m.contains(&("xesam:genre", Meta::Textes(vec!["Jazz".into()]))));
        assert!(m.contains(&("mpris:length", Meta::Entier(545_000_000))));
        assert!(m.contains(&(
            "mpris:trackid",
            Meta::Chemin("/org/tune/zone2/piste/1".into())
        )));
        assert!(e.volume_mpris() - 0.5 < 1e-9);
        // Même état relu : rien à signaler.
        assert!(e.appliquer_zone(&zone("playing"), t0).rien());
    }

    #[test]
    fn en_pause_la_position_ne_court_plus_et_ne_depasse_jamais_la_duree() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        e.appliquer_zone(&zone("paused"), t0);
        assert_eq!(e.position_ms(t0 + Duration::from_secs(60)), 30_000);
        e.appliquer_evenement(&ev("playback.resumed", json!({"zone_id": 2})), t0);
        assert_eq!(e.position_ms(t0 + Duration::from_secs(3600)), 545_000);
    }

    #[test]
    fn evenements_d_une_autre_zone_ou_sans_zone_ignores() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        assert!(
            e.appliquer_evenement(&ev("playback.paused", json!({"zone_id": 3})), t0)
                .is_none()
        );
        assert!(
            e.appliquer_evenement(&ev("playback.paused", json!({})), t0)
                .is_none()
        );
        assert!(
            e.appliquer_evenement(&json!({"type": "playback.paused"}), t0)
                .is_none()
        );
    }

    #[test]
    fn volume_externe_ne_remet_pas_le_volume_a_zero() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        e.appliquer_zone(&zone("playing"), t0);
        let msg = ev(
            "zone.volume_externe",
            json!({"zone_id": 2, "device_volume": 12}),
        );
        assert!(e.appliquer_evenement(&msg, t0).is_none());
        // `playback.volume` sans nombre : ignoré aussi.
        assert!(
            e.appliquer_evenement(&ev("playback.volume", json!({"zone_id": 2})), t0)
                .is_none()
        );
        assert!((e.gain - 0.5).abs() < 1e-9);
        let c = e
            .appliquer_evenement(
                &ev("playback.volume", json!({"zone_id": 2, "volume": 0.25})),
                t0,
            )
            .unwrap();
        assert!(c.volume);
        assert!((e.gain - 0.25).abs() < 1e-9);
    }

    #[test]
    fn nouveau_morceau_change_le_trackid_et_remet_la_position_a_zero() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        e.appliquer_zone(&zone("playing"), t0);
        e.pochette = Some("file:///c/abc.jpg".into());
        let c = e
            .appliquer_evenement(
                &ev("playback.track_changed", json!({"zone_id": 2, "track_id": 8, "title": "Freddie Freeloader", "duration_ms": 589_000, "cover_path": "abc123"})),
                t0,
            )
            .unwrap();
        assert!(c.metadonnees && !c.statut);
        assert_eq!(e.trackid(), "/org/tune/zone2/piste/2");
        assert_eq!(e.position_ms(t0), 0);
        // Même album, même pochette : gardée.
        assert_eq!(e.pochette.as_deref(), Some("file:///c/abc.jpg"));
    }

    #[test]
    fn duree_decouverte_en_cours_ne_change_pas_de_morceau() {
        let t0 = Instant::now();
        let mut e = Etat::new(1, "", t0);
        e.appliquer_evenement(
            &ev(
                "playback.started",
                json!({"zone_id": 1, "title": "Radio X", "duration_ms": 0}),
            ),
            t0,
        );
        let avant = e.trackid();
        let t1 = t0 + Duration::from_secs(5);
        let c = e
            .appliquer_evenement(
                &ev(
                    "playback.track_changed",
                    json!({"zone_id": 1, "title": "Radio X", "duration_ms": 200_000}),
                ),
                t1,
            )
            .unwrap();
        assert!(c.metadonnees);
        assert_eq!(e.trackid(), avant);
        assert_eq!(e.position_ms(t1), 5_000, "la position continue");
    }

    #[test]
    fn autre_pochette_oublie_le_fichier_en_cache() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        e.appliquer_zone(&zone("playing"), t0);
        e.pochette = Some("file:///c/abc.jpg".into());
        e.appliquer_evenement(
            &ev(
                "playback.started",
                json!({"zone_id": 2, "track_id": 9, "title": "B", "cover_path": "zzz"}),
            ),
            t0,
        );
        assert_eq!(e.pochette, None);
        assert_eq!(
            e.pochette_a_chercher("http://nas:8888").as_deref(),
            Some("http://nas:8888/api/v1/library/artwork/zzz")
        );
        e.pochette = Some("file:///c/zzz.jpg".into());
        assert_eq!(e.pochette_a_chercher("http://nas:8888"), None);
        assert!(
            e.metadonnees()
                .contains(&("mpris:artUrl", Meta::Texte("file:///c/zzz.jpg".into())))
        );
    }

    #[test]
    fn pause_reprise_arret() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        e.appliquer_zone(&zone("playing"), t0);
        let t1 = t0 + Duration::from_secs(10);
        assert!(
            e.appliquer_evenement(&ev("playback.paused", json!({"zone_id": 2})), t1)
                .unwrap()
                .statut
        );
        assert_eq!(e.statut, Statut::Pause);
        assert_eq!(e.position_ms(t1 + Duration::from_secs(10)), 40_000);
        assert!(
            e.appliquer_evenement(&ev("playback.resumed", json!({"zone_id": 2})), t1)
                .unwrap()
                .statut
        );
        assert!(
            e.appliquer_evenement(&ev("playback.stopped", json!({"zone_id": 2})), t1)
                .unwrap()
                .statut
        );
        assert_eq!(e.statut.mpris(), "Stopped");
        assert_eq!(e.position_ms(t1), 0);
    }

    #[test]
    fn seek_du_serveur_emet_seeked_en_microsecondes() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        e.appliquer_zone(&zone("playing"), t0);
        let c = e
            .appliquer_evenement(
                &ev(
                    "playback.seek",
                    json!({"zone_id": 2, "position_ms": 120_000}),
                ),
                t0,
            )
            .unwrap();
        assert_eq!(c.saut, Some(120_000_000));
        // `playback.position` recale sans signal.
        let c = e
            .appliquer_evenement(
                &ev(
                    "playback.position",
                    json!({"zone_id": 2, "position_ms": 121_000}),
                ),
                t0,
            )
            .unwrap();
        assert!(c.rien());
        assert_eq!(e.position_ms(t0), 121_000);
    }

    #[test]
    fn recovering_et_piste_vide_ne_sont_pas_des_pistes() {
        assert_eq!(Piste::depuis_json(&json!({"title": "Recovering..."})), None);
        assert_eq!(Piste::depuis_json(&json!({"title": ""})), None);
        assert_eq!(Piste::depuis_json(&Value::Null), None);
        assert!(Piste::depuis_json(&json!({"track_id": 4, "title": ""})).is_some());
    }

    #[test]
    fn sans_piste_seul_le_trackid_notrack() {
        let e = Etat::new(2, "", Instant::now());
        assert_eq!(
            e.metadonnees(),
            vec![("mpris:trackid", Meta::Chemin(PAS_DE_PISTE.into()))]
        );
    }

    #[test]
    fn deconnexion_vide_tout() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        e.appliquer_zone(&zone("playing"), t0);
        let c = e.deconnecte(t0);
        assert!(c.statut && c.metadonnees);
        assert_eq!(e.statut, Statut::Arret);
        assert_eq!(e.piste, None);
        assert!(e.deconnecte(t0).rien());
    }

    #[test]
    fn seek_relatif_borne_au_debut_et_suivante_au_dela_de_la_fin() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        assert_eq!(e.action_de_saut(10_000_000, t0), None, "rien ne joue");
        e.appliquer_zone(&zone("paused"), t0);
        assert_eq!(
            e.action_de_saut(10_000_000, t0),
            Some(Action::Position(40_000))
        );
        assert_eq!(e.action_de_saut(-60_000_000, t0), Some(Action::Position(0)));
        assert_eq!(e.action_de_saut(600_000_000, t0), Some(Action::Suivante));
    }

    #[test]
    fn set_position_exige_le_trackid_courant() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        e.appliquer_zone(&zone("playing"), t0);
        let id = e.trackid();
        assert_eq!(
            e.action_de_position(&id, 90_000_000),
            Some(Action::Position(90_000))
        );
        assert_eq!(
            e.action_de_position("/org/tune/zone2/piste/99", 90_000_000),
            None
        );
        assert_eq!(e.action_de_position(&id, -1), None);
        assert_eq!(e.action_de_position(&id, 999_000_000_000), None);
    }

    #[test]
    fn volume_demande_passe_par_la_courbe_de_la_zone() {
        let t0 = Instant::now();
        let mut e = Etat::new(2, "", t0);
        let mut z = zone("playing");
        z["volume_curve"] = json!("db60");
        z["volume"] = json!(10f64.powf(-1.5));
        e.appliquer_zone(&z, t0);
        assert!((e.volume_mpris() - 0.5).abs() < 1e-9);
        match e.action_de_volume(0.5) {
            Action::Volume(g) => assert!((g - 10f64.powf(-1.5)).abs() < 1e-9),
            autre => panic!("{autre:?}"),
        }
    }
}
