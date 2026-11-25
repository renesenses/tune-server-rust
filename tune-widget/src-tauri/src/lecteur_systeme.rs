//! Le Widget vu comme un LECTEUR par le système : ce qu'il reçoit (touches
//! multimédia du clavier, Centre de contrôle) et ce qu'il annonce (« À
//! l'écoute »). Accord de Bertrand du 23/11, rc14.
//!
//! Sous macOS, `centre_multimedia.rs` branche ce module sur MediaPlayer
//! (`MPRemoteCommandCenter`, `MPNowPlayingInfoCenter`) : les touches ⏯ ⏭ ⏮
//! reviennent SANS aucune autorisation — ni Accessibilité, ni Surveillance de
//! l'entrée —, au contraire du `CGEventTap` de `tauri-plugin-global-shortcut`
//! retiré le 20/11 (#9255). Sous Windows, les touches restent à
//! `raccourcis_globaux.rs` ; sous Linux, rien ne change.
//!
//! Ce module est la partie commune et testable : de quelle route du serveur
//! une commande reçue devient l'appel, et ce que le Widget annonce à partir de
//! `GET /api/v1/widget/data`. Les boutons du Widget et les entrées du menu
//! Lecture passent par la même fonction ([`envoyer`]).

// Hors macOS, seule la partie « commande » sert (le menu Lecture, les
// raccourcis globaux de Windows) : l'annonce « À l'écoute » est propre à macOS.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use serde_json::Value;

/// Une commande reçue du système (ou du menu Lecture).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Commande {
    /// ⏯ : pause si la zone joue, reprise sinon.
    LecturePause,
    /// Bouton Lecture du Centre de contrôle.
    Lecture,
    /// Bouton Pause du Centre de contrôle.
    Pause,
    Suivante,
    Precedente,
    /// Le curseur de position du Centre de contrôle, en millisecondes.
    Position(i64),
}

/// L'appel au serveur Tune qu'une commande déclenche (toujours un `POST`).
#[derive(Debug, PartialEq)]
pub struct Appel {
    /// Chemin sous l'adresse du serveur, `/api/v1/zones/<id>/…`.
    pub chemin: String,
    /// Corps JSON, pour un déplacement seulement.
    pub corps: Option<Value>,
}

impl Commande {
    /// Il faut savoir si la zone joue pour choisir entre pause et reprise.
    pub fn depend_de_l_etat(self) -> bool {
        self == Commande::LecturePause
    }
}

/// L'appel d'une commande sur `zone_id`. `en_lecture` ne sert qu'à ⏯ : faute
/// de le savoir (serveur muet), c'est une reprise, comme les boutons du Widget.
pub fn appel_de(commande: Commande, zone_id: i64, en_lecture: bool) -> Appel {
    let route = |fin: &str| format!("/api/v1/zones/{zone_id}/{fin}");
    match commande {
        Commande::LecturePause if en_lecture => Appel {
            chemin: route("pause"),
            corps: None,
        },
        Commande::LecturePause | Commande::Lecture => Appel {
            chemin: route("resume"),
            corps: None,
        },
        Commande::Pause => Appel {
            chemin: route("pause"),
            corps: None,
        },
        Commande::Suivante => Appel {
            chemin: route("next"),
            corps: None,
        },
        Commande::Precedente => Appel {
            chemin: route("previous"),
            corps: None,
        },
        // Le serveur refuse une position négative (#3966) : on ne l'envoie pas.
        Commande::Position(ms) => Appel {
            chemin: route("seek"),
            corps: Some(serde_json::json!({ "position_ms": ms.max(0) })),
        },
    }
}

/// La zone joue-t-elle, d'après `GET /api/v1/widget/data?zone_id=<id>` ?
pub fn joue(donnees: &Value) -> bool {
    donnees.get("state").and_then(Value::as_str) == Some("playing")
}

/// Envoie une commande au serveur `url` pour `zone_id`. Les erreurs du réseau
/// sont rendues, jamais fatales : l'appelant les journalise.
pub async fn envoyer(
    http: &reqwest::Client,
    url: &str,
    zone_id: i64,
    commande: Commande,
) -> Result<(), String> {
    let en_lecture = if commande.depend_de_l_etat() {
        match http
            .get(format!("{url}/api/v1/widget/data?zone_id={zone_id}"))
            .send()
            .await
        {
            Ok(r) => r.json::<Value>().await.map(|d| joue(&d)).unwrap_or(false),
            Err(_) => false,
        }
    } else {
        false
    };
    let appel = appel_de(commande, zone_id, en_lecture);
    let mut requete = http.post(format!("{url}{}", appel.chemin));
    if let Some(corps) = &appel.corps {
        requete = requete.json(corps);
    }
    let reponse = requete.send().await.map_err(|e| e.to_string())?;
    if !reponse.status().is_success() {
        return Err(format!("{} → {}", appel.chemin, reponse.status()));
    }
    Ok(())
}

/// L'état de lecture annoncé au système.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Etat {
    EnLecture,
    EnPause,
    Arrete,
}

/// Ce que le Widget annonce dans « À l'écoute ».
#[derive(Clone, Debug, PartialEq)]
pub struct Ecoute {
    pub titre: String,
    pub artiste: Option<String>,
    pub album: Option<String>,
    /// 0 quand le serveur ne la connaît pas (radio).
    pub duree_ms: i64,
    pub position_ms: i64,
    pub etat: Etat,
    /// Adresse complète de la pochette, si le morceau en a une.
    pub pochette: Option<String>,
}

/// Un titre que le serveur pose le temps de reprendre une lecture (même
/// filtre que `zone_cible.rs` et `src/app.js`).
const EN_REPRISE: &str = "Recovering...";

fn texte(v: &Value, cle: &str) -> Option<String> {
    v.get(cle)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Ce qu'il faut annoncer pour `zone_id`, d'après la réponse de
/// `GET /api/v1/widget/data` : l'entrée de la zone dans `zones` (chaque
/// entrée porte `state`, `now_playing` et `position_ms`). `None` : rien à
/// annoncer (zone inconnue, rien en cours, reprise en cours).
pub fn ecoute_de(donnees: &Value, zone_id: i64, serveur: &str) -> Option<Ecoute> {
    let zone = donnees
        .get("zones")?
        .as_array()?
        .iter()
        .find(|z| z.get("zone_id").and_then(Value::as_i64) == Some(zone_id))?;
    let piste = zone.get("now_playing").filter(|v| v.is_object())?;
    let titre = texte(piste, "title").filter(|t| t != EN_REPRISE)?;
    let etat = match zone.get("state").and_then(Value::as_str) {
        Some("playing") => Etat::EnLecture,
        Some("paused") => Etat::EnPause,
        _ => Etat::Arrete,
    };
    let duree_ms = piste
        .get("duration_ms")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let mut position_ms = zone
        .get("position_ms")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    if duree_ms > 0 {
        position_ms = position_ms.min(duree_ms);
    }
    Some(Ecoute {
        titre,
        artiste: texte(piste, "artist_name"),
        album: texte(piste, "album_title"),
        duree_ms,
        position_ms,
        etat,
        pochette: texte(piste, "cover_path").and_then(|c| url_de_pochette(serveur, &c)),
    })
}

/// L'adresse d'une pochette, comme `urlDePochette` de `src/contrat.js` : une
/// adresse complète telle quelle, un chemin `/api/…` sous le serveur, sinon le
/// nom du fichier sous `/api/v1/library/artwork/`.
pub fn url_de_pochette(serveur: &str, chemin: &str) -> Option<String> {
    let bas = chemin.to_ascii_lowercase();
    if bas.starts_with("http://") || bas.starts_with("https://") {
        return Some(chemin.to_string());
    }
    if chemin.starts_with("/api/") {
        return Some(format!("{serveur}{chemin}"));
    }
    let fichier = chemin.rsplit('/').next().filter(|f| !f.is_empty())?;
    Some(format!(
        "{serveur}/api/v1/library/artwork/{}",
        urlencoding::encode(fichier)
    ))
}

/// Écart toléré entre la position que le système déduit (dernière annonce +
/// temps écoulé, en lecture) et celle du serveur avant de la ré-annoncer.
pub const DERIVE_TOLEREE_MS: i64 = 2_000;

/// Faut-il ré-annoncer `actuelle`, sachant `precedente` annoncée il y a
/// `ecoule_ms` ? Oui si le morceau, l'état ou la durée changent, ou si la
/// position a dérivé (déplacement, mise en mémoire tampon). Sinon le système
/// fait avancer la position seul (vitesse de lecture 1).
pub fn a_reannoncer(precedente: Option<&Ecoute>, actuelle: &Ecoute, ecoule_ms: i64) -> bool {
    let Some(p) = precedente else {
        return true;
    };
    if p.titre != actuelle.titre
        || p.artiste != actuelle.artiste
        || p.album != actuelle.album
        || p.duree_ms != actuelle.duree_ms
        || p.etat != actuelle.etat
        || p.pochette != actuelle.pochette
    {
        return true;
    }
    let attendue = if p.etat == Etat::EnLecture {
        p.position_ms + ecoule_ms.max(0)
    } else {
        p.position_ms
    };
    (actuelle.position_ms - attendue).abs() > DERIVE_TOLEREE_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn chaque_commande_mene_a_sa_route() {
        let a = |c, joue| appel_de(c, 7, joue);
        assert_eq!(
            a(Commande::LecturePause, true).chemin,
            "/api/v1/zones/7/pause"
        );
        assert_eq!(
            a(Commande::LecturePause, false).chemin,
            "/api/v1/zones/7/resume"
        );
        assert_eq!(a(Commande::Lecture, true).chemin, "/api/v1/zones/7/resume");
        assert_eq!(a(Commande::Pause, false).chemin, "/api/v1/zones/7/pause");
        assert_eq!(a(Commande::Suivante, false).chemin, "/api/v1/zones/7/next");
        assert_eq!(
            a(Commande::Precedente, false).chemin,
            "/api/v1/zones/7/previous"
        );
        let seek = a(Commande::Position(83_500), false);
        assert_eq!(seek.chemin, "/api/v1/zones/7/seek");
        assert_eq!(seek.corps, Some(json!({ "position_ms": 83_500 })));
        // Le serveur refuse un négatif (#3966).
        assert_eq!(
            a(Commande::Position(-20), false).corps,
            Some(json!({ "position_ms": 0 }))
        );
        assert!(a(Commande::Suivante, false).corps.is_none());
        assert!(Commande::LecturePause.depend_de_l_etat());
        assert!(!Commande::Pause.depend_de_l_etat());
    }

    /// Un faux serveur Tune sur la boucle locale : rend `etat` à
    /// `GET /widget/data` et note chaque requête (« MÉTHODE chemin corps »).
    fn faux_serveur(etat: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let ecoute = TcpListener::bind("127.0.0.1:0").unwrap();
        let adresse = format!("http://{}", ecoute.local_addr().unwrap());
        let journal = Arc::new(Mutex::new(Vec::new()));
        let j = journal.clone();
        std::thread::spawn(move || {
            for flux in ecoute.incoming() {
                let Ok(mut flux) = flux else { return };
                let mut lecteur = BufReader::new(flux.try_clone().unwrap());
                let mut premiere = String::new();
                lecteur.read_line(&mut premiere).unwrap();
                let mut longueur = 0usize;
                loop {
                    let mut l = String::new();
                    lecteur.read_line(&mut l).unwrap();
                    if l.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                        longueur = v.trim().parse().unwrap();
                    }
                }
                let mut corps = vec![0; longueur];
                lecteur.read_exact(&mut corps).unwrap();
                let mut morceaux = premiere.split_whitespace();
                let ligne = format!(
                    "{} {} {}",
                    morceaux.next().unwrap(),
                    morceaux.next().unwrap(),
                    String::from_utf8(corps).unwrap()
                );
                j.lock().unwrap().push(ligne.trim().to_string());
                let reponse = format!(r#"{{"state":"{etat}","zones":[]}}"#);
                let _ = write!(
                    flux,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reponse}",
                    reponse.len()
                );
            }
        });
        (adresse, journal)
    }

    #[tokio::test]
    async fn les_commandes_recues_appellent_l_api_de_la_zone() {
        let http = reqwest::Client::new();

        let (url, journal) = faux_serveur("playing");
        envoyer(&http, &url, 3, Commande::LecturePause)
            .await
            .unwrap();
        envoyer(&http, &url, 3, Commande::Suivante).await.unwrap();
        envoyer(&http, &url, 3, Commande::Precedente).await.unwrap();
        envoyer(&http, &url, 3, Commande::Position(42_000))
            .await
            .unwrap();
        assert_eq!(
            *journal.lock().unwrap(),
            [
                "GET /api/v1/widget/data?zone_id=3",
                "POST /api/v1/zones/3/pause",
                "POST /api/v1/zones/3/next",
                "POST /api/v1/zones/3/previous",
                r#"POST /api/v1/zones/3/seek {"position_ms":42000}"#,
            ]
        );

        // En pause : ⏯ reprend. Lecture et Pause n'interrogent pas l'état.
        let (url, journal) = faux_serveur("paused");
        envoyer(&http, &url, 5, Commande::LecturePause)
            .await
            .unwrap();
        envoyer(&http, &url, 5, Commande::Pause).await.unwrap();
        envoyer(&http, &url, 5, Commande::Lecture).await.unwrap();
        assert_eq!(
            *journal.lock().unwrap(),
            [
                "GET /api/v1/widget/data?zone_id=5",
                "POST /api/v1/zones/5/resume",
                "POST /api/v1/zones/5/pause",
                "POST /api/v1/zones/5/resume",
            ]
        );
    }

    #[tokio::test]
    async fn un_serveur_muet_est_une_erreur_rendue() {
        let http = reqwest::Client::new();
        let libre = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", libre.local_addr().unwrap());
        drop(libre);
        assert!(envoyer(&http, &url, 1, Commande::LecturePause)
            .await
            .is_err());
    }

    fn donnees() -> Value {
        json!({
            "state": "stopped",
            "zones": [
                {"zone_id": 1, "state": "stopped", "position_ms": 0, "now_playing": null},
                {"zone_id": 4, "state": "playing", "position_ms": 61_000, "now_playing": {
                    "track_id": 12, "title": "So What", "artist_name": "Miles Davis",
                    "album_title": "Kind of Blue", "duration_ms": 562_000,
                    "cover_path": "/data/covers/kind of blue.jpg"
                }},
                {"zone_id": 6, "state": "paused", "position_ms": 9_000_000, "now_playing": {
                    "title": "Radio FIP", "artist_name": "", "duration_ms": 0,
                    "cover_path": "https://exemple.fr/fip.png"
                }},
                {"zone_id": 8, "state": "playing", "now_playing": {"title": "Recovering..."}}
            ]
        })
    }

    #[test]
    fn l_ecoute_suit_la_zone_du_widget() {
        let e = ecoute_de(&donnees(), 4, "http://salon:8888").unwrap();
        assert_eq!(
            e,
            Ecoute {
                titre: "So What".into(),
                artiste: Some("Miles Davis".into()),
                album: Some("Kind of Blue".into()),
                duree_ms: 562_000,
                position_ms: 61_000,
                etat: Etat::EnLecture,
                pochette: Some(
                    "http://salon:8888/api/v1/library/artwork/kind%20of%20blue.jpg".into()
                ),
            }
        );
        // Radio : sans durée, la position n'est pas bornée ; champ vide = absent.
        let radio = ecoute_de(&donnees(), 6, "http://salon:8888").unwrap();
        assert_eq!(radio.etat, Etat::EnPause);
        assert_eq!(radio.artiste, None);
        assert_eq!(radio.position_ms, 9_000_000);
        assert_eq!(
            radio.pochette.as_deref(),
            Some("https://exemple.fr/fip.png")
        );
        // Rien en cours, reprise en cours, zone inconnue : rien à annoncer.
        assert_eq!(ecoute_de(&donnees(), 1, "http://s"), None);
        assert_eq!(ecoute_de(&donnees(), 8, "http://s"), None);
        assert_eq!(ecoute_de(&donnees(), 99, "http://s"), None);
        assert_eq!(ecoute_de(&json!("Not Found"), 4, "http://s"), None);
    }

    #[test]
    fn la_pochette_comme_le_mini_lecteur() {
        let s = "http://h:8888";
        assert_eq!(
            url_de_pochette(s, "/api/v1/radios/3/logo").as_deref(),
            Some("http://h:8888/api/v1/radios/3/logo")
        );
        assert_eq!(
            url_de_pochette(s, "C:\\x/abc.jpg").as_deref(),
            Some("http://h:8888/api/v1/library/artwork/abc.jpg")
        );
        assert_eq!(url_de_pochette(s, "dossier/"), None);
    }

    #[test]
    fn on_ne_reannonce_que_ce_qui_change() {
        let e = ecoute_de(&donnees(), 4, "http://s").unwrap();
        assert!(a_reannoncer(None, &e, 0));
        // 2 s plus tard, 2 s plus loin : le système l'a déjà déduit.
        let plus_loin = Ecoute {
            position_ms: 63_000,
            ..e.clone()
        };
        assert!(!a_reannoncer(Some(&e), &plus_loin, 2_000));
        // Un déplacement : la position saute.
        let saut = Ecoute {
            position_ms: 200_000,
            ..e.clone()
        };
        assert!(a_reannoncer(Some(&e), &saut, 2_000));
        // En pause, la position ne doit pas avancer.
        let pause = Ecoute {
            etat: Etat::EnPause,
            ..e.clone()
        };
        assert!(a_reannoncer(Some(&e), &pause, 2_000));
        assert!(!a_reannoncer(Some(&pause), &pause, 60_000));
        let autre = Ecoute {
            titre: "Freddie Freeloader".into(),
            ..e.clone()
        };
        assert!(a_reannoncer(Some(&e), &autre, 0));
    }
}
