//! #4851 — l'enchaînement par la file de HQPlayer.
//!
//! Un faux HQPlayer écoute sur une socket locale, parle le XML du protocole
//! de contrôle (lu dans `hqp-control-601-src.zip`, Signalyst) et tient une
//! playlist : `PlaylistAdd` (avec `clear`), `PlaylistRemove index`,
//! `PlaylistGet` → `PlaylistItem index uri`, `Status` avec `track`,
//! `position`, `length` et `<metadata uri>`. Il ne joue aucun son : il
//! mesure ce que la sortie ÉCRIT et ce qu'elle déduit de ce qu'il répond.
//!
//! Il sert aussi au banc du sondeur (`poller/hqplayer_enchainement_4851.rs`).

use super::*;
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

/// L'état du faux HQPlayer, partagé avec le test.
#[derive(Debug, Default)]
pub(crate) struct EtatFaux {
    /// Chaque commande reçue, telle quelle (sans la déclaration XML).
    pub(crate) recu: Vec<String>,
    /// La playlist de HQPlayer, dans l'ordre.
    pub(crate) playlist: Vec<String>,
    /// L'entrée jouée (index dans `playlist`).
    pub(crate) courante: usize,
    /// 0 arrêté, 1 pause, 2 lecture (enum Signalyst).
    pub(crate) etat: u8,
    pub(crate) position_s: f64,
    pub(crate) longueur_s: f64,
    /// `Status` porte-t-il `<metadata uri>` ? Les deux formes sont mesurées.
    pub(crate) annonce_uri: bool,
    /// Premier index rendu par `PlaylistGet` : 1, exprès, pour qu'une sortie
    /// qui CALCULERAIT l'index au lieu de le relire se trompe.
    pub(crate) base_index: usize,
    /// #6115 — le comportement signalé en rc4 : tant que la lecture n'est
    /// pas ARRÊTÉE (état 0), `PlaylistAdd clear="1"` est ignoré.
    pub(crate) ignore_clear_hors_arret: bool,
    /// #6115 (suite) — le moteur 6.0.4 tel que le pont LMS-HQPlayer de Simon
    /// Arnold l'a mesuré : `clear="1"` est ignoré dans TOUS les états,
    /// arrêt compris ; `PlaylistAdd` répond OK et ajoute.
    pub(crate) ignore_clear_toujours: bool,
    /// #6115 — nombre de `Status` rendus en état 3 (STOPREQ) après un
    /// `Stop`, avant l'état 0. Zéro : arrêt immédiat.
    pub(crate) status_avant_arret: u32,
    /// #6115 — `Stop` ne mène jamais à l'état 0 (borne d'attente).
    pub(crate) n_arrete_jamais: bool,
    /// #7244 — Play is acknowledged, but the ASIO engine stays stopped.
    pub(crate) refuse_la_sortie: bool,
    restant_avant_arret: u32,
}

pub(crate) struct FauxHqplayer {
    pub(crate) port: u16,
    pub(crate) etat: Arc<StdMutex<EtatFaux>>,
}

fn attribut(ligne: &str, nom: &str) -> Option<String> {
    extract_xml_attr(ligne, nom).map(|v| {
        quick_xml::escape::unescape(&v)
            .map(|c| c.into_owned())
            .unwrap_or(v)
    })
}

fn repondre(etat: &mut EtatFaux, ligne: &str) -> String {
    etat.recu.push(ligne.to_string());
    if ligne.starts_with("<Status") {
        if etat.etat == 3 && !etat.n_arrete_jamais {
            if etat.restant_avant_arret == 0 {
                etat.etat = 0;
            } else {
                etat.restant_avant_arret -= 1;
            }
        }
        let meta = if etat.annonce_uri {
            etat.playlist
                .get(etat.courante)
                .map(|u| format!(r#"<metadata uri="{}" />"#, escape_xml(u)))
                .unwrap_or_default()
        } else {
            String::new()
        };
        return format!(
            r#"<Status state="{}" track="{}" tracks_total="{}" position="{:.1}" length="{:.1}" volume="100">{meta}</Status>"#,
            etat.etat,
            etat.courante + etat.base_index,
            etat.playlist.len(),
            etat.position_s,
            etat.longueur_s,
        );
    }
    if ligne.starts_with("<PlaylistGet") {
        let mut xml = String::from("<PlaylistGet>");
        for (i, uri) in etat.playlist.iter().enumerate() {
            xml.push_str(&format!(
                r#"<PlaylistItem index="{}" uri="{}" length="200" />"#,
                i + etat.base_index,
                escape_xml(uri)
            ));
        }
        xml.push_str("</PlaylistGet>");
        return xml;
    }
    if ligne.starts_with("<PlaylistAdd") {
        let vide = attribut(ligne, "clear").as_deref() == Some("1");
        if vide && etat.ignore_clear_hors_arret && etat.etat != 0 {
            return r#"<PlaylistAdd result="OK" />"#.into();
        }
        if vide && !etat.ignore_clear_toujours {
            etat.playlist.clear();
            etat.courante = 0;
        }
        if let Some(uri) = attribut(ligne, "uri") {
            etat.playlist.push(uri);
        }
        return r#"<PlaylistAdd result="OK" />"#.into();
    }
    if ligne.starts_with("<PlaylistClear") {
        // #6115 (suite) — pont LMS, moteur 6.0.4 : en lecture, la piste qui
        // joue reste et le reste part (mesuré). À l'arrêt, tout part : DÉDUIT
        // de son `Stop`, `PlaylistClear`, `PlaylistAdd`, `Play` qui change
        // proprement de titre, pas mesuré à part.
        if etat.etat == 0 {
            etat.playlist.clear();
        } else if let Some(joue) = etat.playlist.get(etat.courante).cloned() {
            etat.playlist = vec![joue];
        }
        etat.courante = 0;
        return r#"<PlaylistClear result="OK" />"#.into();
    }
    if ligne.starts_with("<PlaylistRemove") {
        if let Some(i) = attribut(ligne, "index").and_then(|v| v.parse::<usize>().ok())
            && i >= etat.base_index
            && i - etat.base_index < etat.playlist.len()
        {
            let i = i - etat.base_index;
            etat.playlist.remove(i);
            if i < etat.courante {
                etat.courante -= 1;
            }
        }
        return r#"<PlaylistRemove result="OK" />"#.into();
    }
    if ligne.starts_with("<GetInfo") {
        return r#"<GetInfo name="HQPlayer Embedded" product="HQPlayer" version="5" />"#.into();
    }
    if ligne.starts_with("<Play ") || ligne.starts_with("<Play/") {
        etat.etat = if etat.refuse_la_sortie { 0 } else { 2 };
    } else if ligne.starts_with("<Stop") {
        if etat.n_arrete_jamais || etat.status_avant_arret > 0 {
            etat.etat = 3;
            etat.restant_avant_arret = etat.status_avant_arret;
        } else {
            etat.etat = 0;
        }
    }
    let nom: String = ligne
        .trim_start_matches('<')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    format!(r#"<{nom} result="OK" />"#)
}

impl FauxHqplayer {
    /// Un HQPlayer en lecture de `courante`, à `position_s` sur `longueur_s`.
    pub(crate) async fn en_lecture(courante: &str, position_s: f64, longueur_s: f64) -> Self {
        let ecoute = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let etat = Arc::new(StdMutex::new(EtatFaux {
            playlist: vec![courante.to_string()],
            etat: 2,
            position_s,
            longueur_s,
            annonce_uri: true,
            base_index: 1,
            ..Default::default()
        }));
        let partage = etat.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = ecoute.accept().await else {
                    return;
                };
                let etat = partage.clone();
                tokio::spawn(async move {
                    let mut socket = BufReader::new(socket);
                    loop {
                        let mut ligne = String::new();
                        match socket.read_line(&mut ligne).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                        let ligne = ligne.trim();
                        if ligne.is_empty() || ligne.starts_with("<?xml") {
                            continue;
                        }
                        let reponse = repondre(&mut etat.lock().unwrap(), ligne);
                        if socket
                            .get_mut()
                            .write_all(reponse.as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        Self { port, etat }
    }

    pub(crate) fn sortie(&self) -> HqplayerOutput {
        HqplayerOutput::new(
            "HQPlayer".into(),
            self.device_id(),
            "127.0.0.1".into(),
            self.port,
        )
    }

    pub(crate) fn device_id(&self) -> String {
        format!("hqplayer-127.0.0.1:{}", self.port)
    }

    /// HQPlayer passe de lui-même à l'entrée suivante de sa playlist.
    pub(crate) fn enchaine(&self, longueur_s: f64) {
        let mut e = self.etat.lock().unwrap();
        assert!(
            e.courante + 1 < e.playlist.len(),
            "le faux HQPlayer n'a pas de suivante dans sa playlist : {:?}",
            e.playlist
        );
        e.courante += 1;
        e.position_s = 0.5;
        e.longueur_s = longueur_s;
    }

    pub(crate) fn a(&self, position_s: f64) {
        self.etat.lock().unwrap().position_s = position_s;
    }

    pub(crate) fn playlist(&self) -> Vec<String> {
        self.etat.lock().unwrap().playlist.clone()
    }

    pub(crate) fn recu(&self) -> Vec<String> {
        self.etat.lock().unwrap().recu.clone()
    }

    pub(crate) fn sans_uri(&self) {
        self.etat.lock().unwrap().annonce_uri = false;
    }

    /// #6115 — modifie l'état du faux avant l'essai.
    pub(crate) fn regler(&self, f: impl FnOnce(&mut EtatFaux)) {
        f(&mut self.etat.lock().unwrap());
    }

    /// #6115 — l'URI que le faux HQPlayer joue, et son état (0, 1, 2, 3).
    pub(crate) fn joue(&self) -> (Option<String>, u8) {
        let e = self.etat.lock().unwrap();
        (e.playlist.get(e.courante).cloned(), e.etat)
    }
}

const COURANTE: &str = "http://192.168.0.20:8888/stream/aaaa.flac";
const SUIVANTE: &str = "http://192.168.0.20:8888/stream/bbbb.flac";
const INSEREE: &str = "http://192.168.0.20:8888/stream/cccc.flac?a=1&b=2";
const TROISIEME: &str = "http://192.168.0.20:8888/stream/dddd.flac";

fn media(url: &str) -> PlayMedia<'_> {
    PlayMedia {
        url,
        mime_type: "audio/flac",
        ..Default::default()
    }
}

#[test]
fn hqplayer_declare_l_enchainement() {
    let sortie = HqplayerOutput::new("HQPlayer".into(), "hqp".into(), "127.0.0.1".into(), 4321);
    assert!(
        sortie.capabilities().can_gapless && sortie.supports_internal_gapless(),
        "#4851 : sans can_gapless le sondeur renonce (gapless_skipped_exclusive_output) \
         et chaque piste attend cinq sondages Stopped puis repart de zéro"
    );
}

/// LE FAIT DE BASE : la suivante est AJOUTÉE à la playlist pendant la
/// lecture — ni `clear="1"`, qui viderait la playlist, ni `Play`, ni `Stop`.
#[tokio::test]
async fn la_suivante_est_ajoutee_a_la_playlist_sans_l_interrompre() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 280.0, 300.0).await;
    let sortie = faux.sortie();

    sortie.set_next_media(&media(SUIVANTE)).await.unwrap();

    assert_eq!(
        faux.recu(),
        vec![format!(
            r#"<PlaylistAdd uri="{SUIVANTE}" queued="1" clear="0"></PlaylistAdd>"#
        )],
        "une seule commande : l'ajout de la suivante, sans vider ni relancer"
    );
    assert_eq!(faux.playlist(), vec![COURANTE, SUIVANTE]);

    // Le sondeur relance l'armement à l'identique : rien de plus ne part.
    sortie.set_next_media(&media(SUIVANTE)).await.unwrap();
    assert_eq!(faux.recu().len(), 1, "{:?}", faux.recu());
}

/// Le second armement AVANT la transition (« Lire ensuite » dans la fenêtre,
/// préparation expirée) : la suivante périmée est retirée, à l'index que
/// HQPlayer lui donne, puis la nouvelle est ajoutée. Sinon HQPlayer
/// jouerait la périmée avant la bonne.
#[tokio::test]
async fn un_second_armement_remplace_la_suivante_au_lieu_de_l_empiler() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 280.0, 300.0).await;
    let sortie = faux.sortie();

    sortie.set_next_media(&media(SUIVANTE)).await.unwrap();
    let _ = sortie.get_status().await.unwrap();
    faux.a(282.0);
    let _ = sortie.get_status().await.unwrap();
    sortie.set_next_media(&media(INSEREE)).await.unwrap();

    assert_eq!(
        faux.playlist(),
        vec![COURANTE, INSEREE],
        "la périmée doit sortir de la playlist de HQPlayer ; reçu {:?}",
        faux.recu()
    );
    assert!(
        faux.recu()
            .iter()
            .any(|c| c == r#"<PlaylistRemove index="2" />"#),
        "l'index retiré est celui que HQPlayer donne (base 1 ici), relu et non \
         calculé : {:?}",
        faux.recu()
    );
}

/// Après la transition, la suivante posée EST la piste en cours : l'armement
/// de la piste d'après ne doit rien retirer — retirer la piste qui joue
/// couperait le son. Mesuré avec et sans `<metadata uri>` dans `Status`.
#[tokio::test]
async fn apres_la_transition_l_armement_suivant_ne_retire_pas_la_piste_qui_joue() {
    for annonce_uri in [true, false] {
        let faux = FauxHqplayer::en_lecture(COURANTE, 280.0, 300.0).await;
        if !annonce_uri {
            faux.sans_uri();
        }
        let sortie = faux.sortie();

        sortie.set_next_media(&media(SUIVANTE)).await.unwrap();
        faux.a(299.0);
        let _ = sortie.get_status().await.unwrap();
        faux.enchaine(200.0);
        let _ = sortie.get_status().await.unwrap();
        faux.a(180.0);
        let _ = sortie.get_status().await.unwrap();
        sortie.set_next_media(&media(TROISIEME)).await.unwrap();

        assert!(
            !faux.recu().iter().any(|c| c.starts_with("<PlaylistRemove")),
            "annonce_uri={annonce_uri} : la piste en cours a été retirée : {:?}",
            faux.recu()
        );
        assert_eq!(
            faux.playlist(),
            vec![COURANTE, SUIVANTE, TROISIEME],
            "annonce_uri={annonce_uri}"
        );
    }
}

/// Un `play_media` vide la playlist (`clear="1"`) : la suivante d'avant
/// n'existe plus chez HQPlayer, il n'y a rien à retirer ensuite.
#[tokio::test]
async fn play_media_oublie_la_suivante_videe_avec_la_playlist() {
    let faux = FauxHqplayer::en_lecture(COURANTE, 280.0, 300.0).await;
    let sortie = faux.sortie();

    sortie.set_next_media(&media(SUIVANTE)).await.unwrap();
    sortie.play_media(&media(INSEREE)).await.unwrap();
    sortie.set_next_media(&media(TROISIEME)).await.unwrap();

    assert!(
        !faux
            .recu()
            .iter()
            .any(|c| c.starts_with("<PlaylistGet") || c.starts_with("<PlaylistRemove")),
        "{:?}",
        faux.recu()
    );
    assert_eq!(faux.playlist(), vec![INSEREE, TROISIEME]);
}

#[test]
fn l_index_est_relu_dans_playlistget() {
    let xml = r#"<PlaylistGet><PlaylistItem index="1" uri="http://a/x.flac"/><PlaylistItem uri="http://a/y.flac?a=1&amp;b=2" index="2" length="3"/></PlaylistGet>"#;
    assert_eq!(
        index_dans_la_playlist(xml, "http://a/x.flac").as_deref(),
        Some("1")
    );
    assert_eq!(
        index_dans_la_playlist(xml, "http://a/y.flac?a=1&b=2").as_deref(),
        Some("2")
    );
    assert_eq!(index_dans_la_playlist(xml, "http://a/z.flac"), None);
}

#[test]
fn passage_sur_la_suivante() {
    // URI rapportée : preuve directe.
    assert!(passee_sur_la_suivante(
        "u2",
        Some("u2"),
        100_000,
        300_000,
        100_000
    ));
    // Retombée de position en fin de piste.
    assert!(passee_sur_la_suivante("u2", None, 298_000, 300_000, 500));
    // Recul au début d'une piste loin de sa fin : pas une transition.
    assert!(!passee_sur_la_suivante("u2", None, 120_000, 300_000, 0));
    // Toujours sur la courante, qui avance.
    assert!(!passee_sur_la_suivante(
        "u2",
        Some("u1"),
        280_000,
        300_000,
        281_000
    ));
}
