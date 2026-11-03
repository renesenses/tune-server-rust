//! #6335 — la façade MediaRenderer d'une zone confie la suivante à
//! l'enchaînement sans blanc de la zone, au lieu de la relancer.
//!
//! Le cas de terrain (Tades, fil 2263) : un Tune Windows pilote la zone
//! « DDC-0 C19 » d'un Tune OS par sa façade UPnP ; derrière, un Diretta
//! Renderer tire le flux. Le Tune Windows arme sa suivante
//! (`SetNextAVTransportURI`, acceptée et tenue), puis demande `Next` à la fin.
//! La façade promouvait alors la suivante par un `orchestrator.play` complet :
//! nouvelle résolution, `SetAVTransportURI` + `Play` vers le Diretta, qui
//! repart de zéro — un blanc à chaque piste. Le watcher de fin de piste
//! faisait de même, au sondage de 2 s.
//!
//! Ici, comme pour #5304, les deux moitiés de production se parlent par
//! HTTP : le `DlnaOutput` du contrôleur envoie ses requêtes SOAP au routeur
//! de production de la façade. Seule la sortie audio de la zone est factice
//! (`MockOutput`) : aucun son. L'enchaînement de la zone est celui du
//! sondeur : sa décision d'armement (`prochaine_position_jouable`, la ligne
//! que `prepare_gapless` résout et arme) puis l'avance qu'il exécute à la
//! transition constatée (`advance_queue_metadata`).
use super::*;
use std::sync::atomic::{AtomicI64, Ordering};
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::outputs::dlna::DlnaOutput;
use tune_core::outputs::{OutputTarget, PlayMedia};
use tune_core::playback::PlayState;
use tune_core::poller::PositionPoller;

const COURANTE: &str = "http://127.0.0.1:9/stream/courante-6335.wav";
/// Le flux que le Tune Windows arme ~30 s avant la fin (`94a9f1ca…`).
const ARMEE: &str = "http://127.0.0.1:9/stream/armee-6335.wav";
const DUREE_MS: i64 = 69_625;

struct Banc {
    state: AppState,
    zone: i64,
    device: String,
    controleur: DlnaOutput,
    serveur: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Banc {
    fn drop(&mut self) {
        self.serveur.abort();
    }
}

impl Banc {
    async fn new() -> Self {
        // `sessions()` est une statique de processus : une plage d'ids à nous.
        static ZONE: AtomicI64 = AtomicI64::new(6_335_000);
        let zone = ZONE.fetch_add(1, Ordering::Relaxed);
        let device = format!("mock-6335-{zone}");
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("facade.db");
        let config = crate::config::TuneConfig {
            db_path: db.to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let state = AppState::new(db.to_str().unwrap(), 0, config).unwrap();
        state
            .backend
            .execute(
                "INSERT INTO zones (id,name,output_type,output_device_id) VALUES (?1,?2,?3,?4)",
                &[&zone, &"DDC-0 C19", &"mock", &device],
            )
            .unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set(&format!("zone_{zone}_upnp_renderer"), "true")
            .unwrap();
        state
            .outputs
            .lock()
            .await
            .register(Box::new(tune_core::outputs::mock::MockOutput::new(
                &device,
                "Diretta (factice)",
            )));
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hote = format!("http://{}", ecoute.local_addr().unwrap());
        let app = crate::routes::router(state.clone());
        let serveur = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let base = format!("{hote}{}/{zone}", upnp_renderer::RENDERER_MOUNT);
        let controleur = DlnaOutput::new(
            "DDC-0 C19 (Tune)".into(),
            format!("uuid:6335-{zone}"),
            hote.clone(),
            format!("{base}/AVTransport/control"),
            format!("{base}/RenderingControl/control"),
            None,
        );
        Self {
            state,
            zone,
            device,
            controleur,
            serveur,
            _dir: dir,
        }
    }

    /// Les lectures lancées sur la sortie de la zone : chacune est un arrêt
    /// suivi d'une relance chez le renderer réel.
    async fn lectures(&self) -> usize {
        let outputs = self.state.outputs.lock().await;
        let output = outputs.get(&self.device).unwrap();
        let output = output.lock().await;
        output
            .as_any()
            .downcast_ref::<tune_core::outputs::mock::MockOutput>()
            .unwrap()
            .play_call_count()
            .await
    }

    /// La courante lancée par le contrôleur, et sa suivante armée.
    async fn courante_puis_suivante(&self) {
        self.controleur
            .play_media(&media(COURANTE, "Piste 1"))
            .await
            .expect("la façade doit accepter SetAVTransportURI + Play");
        self.controleur
            .set_next_media(&media(ARMEE, "Piste 2"))
            .await
            .expect("la façade doit acquitter SetNextAVTransportURI");
    }

    fn file(&self) -> Vec<(Option<String>, Option<String>)> {
        PlayQueueRepo::with_backend(self.state.backend.clone())
            .get_ordered(self.zone)
            .unwrap()
            .into_iter()
            .map(|l| (l.source, l.source_id))
            .collect()
    }

    /// Ce que le sondeur armerait : la ligne que `prepare_gapless` résout.
    async fn ligne_que_le_sondeur_armerait(&self) -> Option<String> {
        let ps = self.state.playback.get_state(self.zone).await;
        let position =
            PositionPoller::prochaine_position_jouable(&self.state.backend, self.zone, &ps)?;
        PlayQueueRepo::with_backend(self.state.backend.clone())
            .get_at(self.zone, position)
            .ok()
            .flatten()
            .and_then(|l| l.source_id)
    }

    /// La transition sans blanc constatée par le sondeur de la zone.
    async fn la_zone_enchaine(&self) {
        let ps = self.state.playback.get_state(self.zone).await;
        let position =
            PositionPoller::prochaine_position_jouable(&self.state.backend, self.zone, &ps)
                .expect("la zone doit avoir une suivante à enchaîner");
        self.state
            .orchestrator
            .advance_queue_metadata(self.zone, position)
            .await
            .expect("l'avance sans blanc de la zone doit aboutir");
    }

    async fn vers_la_fin(&self, position_ms: i64) {
        self.state
            .playback
            .reset_position(self.zone, position_ms)
            .await;
    }
}

fn media(url: &'static str, titre: &'static str) -> PlayMedia<'static> {
    PlayMedia {
        url,
        mime_type: "audio/wav",
        title: Some(titre),
        artist: Some("Tades"),
        duration_ms: Some(DUREE_MS as u64),
        ..Default::default()
    }
}

/// 🔴 #6335 — témoin principal. La suivante tenue est jouée par
/// l'enchaînement de la ZONE, à la fin de la piste : la sortie ne voit
/// aucune relance, et la façade publie la nouvelle piste.
#[tokio::test]
async fn la_suivante_tenue_est_enchainee_par_la_zone_sans_relance() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    let lectures_avant = b.lectures().await;
    assert_eq!(lectures_avant, 1, "une seule lecture : la courante");

    assert_eq!(
        b.file(),
        vec![
            (Some("upnp".into()), Some(COURANTE.into())),
            (Some("upnp".into()), Some(ARMEE.into())),
        ],
        "#6335 : la suivante posée par SetNextAVTransportURI doit entrer dans \
         la file de la zone, derrière la courante. Sinon personne ne la \
         précharge, et la façade la relance par un play complet — un arrêt \
         puis une relance chez le Diretta à chaque piste."
    );
    assert_eq!(
        b.ligne_que_le_sondeur_armerait().await.as_deref(),
        Some(ARMEE),
        "le sondeur de la zone doit armer la suivante du contrôleur"
    );

    // Fin réelle de la piste : la zone enchaîne d'elle-même.
    b.vers_la_fin(DUREE_MS - 300).await;
    b.la_zone_enchaine().await;

    assert_eq!(
        b.lectures().await,
        lectures_avant,
        "#6335 : l'enchaînement ne doit relancer AUCUNE lecture sur la sortie"
    );
    let vue = snapshot(&b.state, b.zone).await;
    assert_eq!(vue.transport_state, "PLAYING");
    assert_eq!(
        vue.uri, ARMEE,
        "la façade doit annoncer la suivante devenue courante (CurrentURI) : \
         c'est ce que le contrôleur surveille pour adopter son flux armé"
    );
    assert!(
        vue.next_uri.is_empty(),
        "la suivante consommée ne doit plus être publiée"
    );
    assert!(
        vue.position_ms < 1_000,
        "la position doit repartir pour la nouvelle piste, pas rester à la \
         fin de l'ancienne : {} ms",
        vue.position_ms
    );
    let media = b.controleur.media_du_transport().await.unwrap();
    assert_eq!(media.courante.as_deref(), Some(ARMEE));
    assert_eq!(media.suivante, None);

    // Le watcher de la façade passe : il ne doit rien relancer non plus.
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    assert_eq!(
        b.lectures().await,
        lectures_avant,
        "#6335 : le watcher de 2 s a relancé la piste que la zone jouait déjà"
    );

    // Le `Next` du contrôleur, parti à SON horloge après la fin : acquitté,
    // rien à relancer.
    b.controleur
        .basculer_sur_la_suivante_preparee()
        .await
        .expect("un Next juste après l'enchaînement de la zone doit être acquitté");
    assert_eq!(b.lectures().await, lectures_avant);
    assert_eq!(snapshot(&b.state, b.zone).await.uri, ARMEE);
}

/// #6335 — le cas exact du journal : `Next` part quand la zone a encore
/// 1,6 s à jouer. Il ne doit pas couper cette fin ni relancer.
#[tokio::test]
async fn un_next_juste_avant_la_fin_laisse_la_zone_enchainer() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    let lectures_avant = b.lectures().await;
    b.vers_la_fin(68_000).await;

    b.controleur
        .basculer_sur_la_suivante_preparee()
        .await
        .expect("Next en fin de piste doit être acquitté");
    assert_eq!(
        b.lectures().await,
        lectures_avant,
        "#6335 : un Next à 1,6 s de la fin a relancé la suivante par-dessus \
         la fin de la courante"
    );
    let vue = snapshot(&b.state, b.zone).await;
    assert_eq!(vue.uri, COURANTE, "la courante finit de jouer");
    assert_eq!(
        vue.next_uri, ARMEE,
        "la suivante reste tenue jusqu'à l'enchaînement"
    );

    b.la_zone_enchaine().await;
    assert_eq!(b.lectures().await, lectures_avant);
    assert_eq!(snapshot(&b.state, b.zone).await.uri, ARMEE);
}

/// #6335 — un `Next` en milieu de piste est un saut voulu : la suivante part
/// tout de suite (comme avant), et le curseur de la file la suit — la fin de
/// cette piste ne doit pas la rejouer.
#[tokio::test]
async fn un_next_en_milieu_de_piste_saute_tout_de_suite() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    let lectures_avant = b.lectures().await;
    b.vers_la_fin(20_000).await;

    b.controleur
        .basculer_sur_la_suivante_preparee()
        .await
        .expect("Next en milieu de piste doit être acquitté");
    assert_eq!(
        b.lectures().await,
        lectures_avant + 1,
        "le saut lance la suivante"
    );
    assert_eq!(snapshot(&b.state, b.zone).await.uri, ARMEE);
    let ps = b.state.playback.get_state(b.zone).await;
    assert_eq!(
        PositionPoller::next_position(&ps),
        None,
        "après le saut, la file ne doit plus rien avoir à enchaîner : la fin \
         de la suivante la rejouerait (curseur {} / {})",
        ps.queue_position,
        ps.queue_length
    );
}

fn ligne_de_service(uri: &str) -> tune_core::db::play_queue_repo::StreamingQueueItem {
    (
        uri.into(),
        "x".into(),
        String::new(),
        None,
        None,
        0,
        Some("qobuz".into()),
        None,
        None,
    )
}

/// 🔴 #6634 — témoin. La zone avait sa propre file (un album lancé sur le
/// Tune OS), curseur sur la première ligne. Le contrôleur lance une piste
/// par SetURI + Play, SANS suivante : la fin de cette piste ne doit pas
/// enchaîner sur la file d'avant. Avant le correctif, la file et son curseur
/// restaient ceux du Tune OS, et le sondeur désignait leur ligne suivante —
/// « Tune OS a pris la main », fil 2348, réponse 8718.
#[tokio::test]
async fn sans_suivante_la_fin_de_la_piste_n_enchaine_pas_sur_l_ancienne_file() {
    let b = Banc::new().await;
    PlayQueueRepo::with_backend(b.state.backend.clone())
        .set_streaming_queue(
            b.zone,
            &[
                ligne_de_service("q1"),
                ligne_de_service("q2"),
                ligne_de_service("q3"),
            ],
        )
        .unwrap();
    b.state.playback.update_queue_info(b.zone, 0, 3).await;

    b.controleur
        .play_media(&media(COURANTE, "Piste 1"))
        .await
        .expect("SetAVTransportURI + Play");
    assert_eq!(b.lectures().await, 1);

    assert_eq!(
        b.ligne_que_le_sondeur_armerait().await,
        None,
        "#6634 : à la fin de la piste lancée par le contrôleur, la zone ne doit \
         enchaîner sur AUCUNE ligne de la file d'avant (file : {:?})",
        b.file()
    );
    assert_eq!(
        b.file(),
        vec![(Some("upnp".into()), Some(COURANTE.into()))],
        "#6634 : la file de la zone ne porte plus que la courante du contrôleur"
    );
    let ps = b.state.playback.get_state(b.zone).await;
    assert_eq!((ps.queue_position, ps.queue_length), (0, 1));

    // La file est celle de la façade : un Stop la retire.
    b.controleur.stop().await.ok();
    assert!(b.file().is_empty(), "le Stop retire la file de la façade");
}

/// #6634 — répétition de la piste sur la zone : une file `[courante]` la
/// rejouerait sans fin. La file est vidée, la zone s'arrête à la fin.
#[tokio::test]
async fn avec_la_repetition_de_la_piste_la_file_d_avant_est_videe() {
    let b = Banc::new().await;
    PlayQueueRepo::with_backend(b.state.backend.clone())
        .set_streaming_queue(b.zone, &[ligne_de_service("q1"), ligne_de_service("q2")])
        .unwrap();
    b.state.playback.update_queue_info(b.zone, 0, 2).await;
    b.state
        .playback
        .set_repeat(b.zone, tune_core::playback::RepeatMode::One)
        .await;

    b.controleur
        .play_media(&media(COURANTE, "Piste 1"))
        .await
        .expect("SetAVTransportURI + Play");

    assert!(b.file().is_empty(), "file : {:?}", b.file());
    assert_eq!(b.ligne_que_le_sondeur_armerait().await, None);
}

/// #6335 — `Stop` ou une nouvelle URI ne retirent que la file que la façade a
/// écrite. Une file que Tune a réécrite entre-temps (l'auditeur a lancé un
/// album sur la zone) n'est plus la sienne, et elle reste.
#[tokio::test]
async fn le_stop_ne_retire_que_la_file_ecrite_par_la_facade() {
    let b = Banc::new().await;
    b.controleur
        .play_media(&media(COURANTE, "Piste 1"))
        .await
        .expect("SetAVTransportURI + Play");
    assert_eq!(b.lectures().await, 1);
    assert!(snapshot(&b.state, b.zone).await.next_uri.is_empty());

    // L'auditeur réécrit la file de la zone dans Tune.
    PlayQueueRepo::with_backend(b.state.backend.clone())
        .set_streaming_queue(
            b.zone,
            &[
                ligne_de_service("q1"),
                ligne_de_service("q2"),
                ligne_de_service("q3"),
            ],
        )
        .unwrap();
    let file_de_l_auditeur = b.file();

    // Stop sur une file qui n'est pas la nôtre : elle reste.
    b.controleur.stop().await.ok();
    assert_eq!(
        b.file(),
        file_de_l_auditeur,
        "Stop ne retire pas la file de Tune"
    );

    // Avec une suivante, puis un Stop : la file écrite par la façade part.
    b.courante_puis_suivante().await;
    assert_eq!(b.file().len(), 2);
    b.controleur.stop().await.ok();
    assert!(
        b.file().is_empty(),
        "#6335 : après un Stop, la suivante effacée doit sortir de la file — \
         un Play de la courante l'enchaînerait sinon"
    );
    assert_eq!(
        b.state.playback.get_state(b.zone).await.state,
        PlayState::Stopped
    );
}

/// La décision du `Next`, cas par cas.
#[test]
fn la_decision_du_next() {
    use DecisionNext::*;
    let joue = PlayState::Playing;
    assert_eq!(decider_le_next(None, true, true, joue, 0, 0), DejaEnchainee);
    assert_eq!(decider_le_next(None, false, true, joue, 0, 0), Enchainer);
    assert_eq!(
        decider_le_next(Some(true), false, true, joue, 68_000, DUREE_MS),
        LaZoneEnchaine
    );
    assert_eq!(
        decider_le_next(Some(true), false, true, joue, 20_000, DUREE_MS),
        Enchainer
    );
    // Suivante non confiée (répétition, aléatoire) : la façade enchaîne.
    assert_eq!(
        decider_le_next(Some(false), false, true, joue, 68_000, DUREE_MS),
        Enchainer
    );
    // Zone reprise par Tune, arrêtée ou durée inconnue : pas d'attente.
    assert_eq!(
        decider_le_next(Some(true), false, false, joue, 68_000, DUREE_MS),
        Enchainer
    );
    assert_eq!(
        decider_le_next(
            Some(true),
            false,
            true,
            PlayState::Stopped,
            68_000,
            DUREE_MS
        ),
        Enchainer
    );
    assert_eq!(
        decider_le_next(Some(true), false, true, joue, 68_000, 0),
        Enchainer
    );
}

/// Le corps d'un `SetAVTransportURI` seul, sans `Play` (point de contrôle qui
/// change de contexte et lancera plus tard).
async fn poser_une_uri_sans_play(b: &Banc, uri: &str) {
    let hote = b.controleur.host().unwrap().to_string();
    let corps = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
         <s:Body><u:SetAVTransportURI xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
         <InstanceID>0</InstanceID><CurrentURI>{uri}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>\
         </u:SetAVTransportURI></s:Body></s:Envelope>"
    );
    let reponse = tune_core::http::client::shared()
        .post(format!(
            "{hote}{}/{}/AVTransport/control",
            upnp_renderer::RENDERER_MOUNT,
            b.zone
        ))
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header(
            "SOAPAction",
            "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"",
        )
        .body(corps)
        .send()
        .await
        .expect("la façade doit répondre");
    assert!(reponse.status().is_success(), "SetAVTransportURI refusé");
}

const AUTRE: &str = "http://127.0.0.1:9/stream/autre-6335.wav";

/// #6335 (relecture) — `SetAVTransportURI` d'une NOUVELLE URI, sans `Play` :
/// la suivante de l'ancien contexte sort de la file, la zone ne l'armera ni
/// ne l'enchaînera, et rien n'est relancé avant le `Play`.
#[tokio::test]
async fn une_nouvelle_uri_retire_la_suivante_confiee() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    let lectures_avant = b.lectures().await;
    assert_eq!(b.file().len(), 2);

    poser_une_uri_sans_play(&b, AUTRE).await;

    assert!(
        b.file().is_empty(),
        "#6335 : après un SetAVTransportURI, la suivante de l'ancien contexte \
         doit sortir de la file — la zone l'enchaînerait à la fin de la piste"
    );
    assert_eq!(b.ligne_que_le_sondeur_armerait().await, None);
    assert_eq!(
        b.lectures().await,
        lectures_avant,
        "rien n'est lancé avant le Play"
    );
    let vue = snapshot(&b.state, b.zone).await;
    assert_eq!(vue.uri, AUTRE);
    assert!(vue.next_uri.is_empty());
}

/// #6335 (relecture) — une suivante REMPLACÉE prend la place de l'ancienne
/// dans la file : c'est elle que le sondeur arme, et elle que la zone
/// enchaîne, sans relance.
#[tokio::test]
async fn une_suivante_remplacee_prend_la_place_de_l_ancienne() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    let lectures_avant = b.lectures().await;

    b.controleur
        .set_next_media(&media(AUTRE, "Piste 2 bis"))
        .await
        .expect("la façade doit acquitter le remplacement");

    assert_eq!(
        b.file(),
        vec![
            (Some("upnp".into()), Some(COURANTE.into())),
            (Some("upnp".into()), Some(AUTRE.into())),
        ],
        "#6335 : la suivante remplacée doit remplacer l'ancienne dans la file"
    );
    assert_eq!(
        b.ligne_que_le_sondeur_armerait().await.as_deref(),
        Some(AUTRE),
        "#6335 : le sondeur doit armer la suivante remplaçante, pas l'ancienne"
    );
    b.vers_la_fin(DUREE_MS - 300).await;
    b.la_zone_enchaine().await;
    assert_eq!(b.lectures().await, lectures_avant);
    let vue = snapshot(&b.state, b.zone).await;
    assert_eq!(vue.uri, AUTRE);
    assert!(vue.next_uri.is_empty());
}

/// #6335 (relecture) — une suivante remplacée que la file ne peut plus
/// prendre (répétition d'une piste activée entre-temps) : l'ancienne ne doit
/// pas rester dans la file sous son ancienne forme.
#[tokio::test]
async fn une_suivante_remplacee_puis_refusee_sort_de_la_file() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    b.state
        .playback
        .set_repeat(b.zone, tune_core::playback::RepeatMode::One)
        .await;

    b.controleur
        .set_next_media(&media(AUTRE, "Piste 2 bis"))
        .await
        .expect("la façade doit acquitter le remplacement");

    assert!(
        b.file().is_empty(),
        "#6335 : une suivante remplacée puis refusée doit faire sortir \
         l'ancienne de la file, sinon la zone l'enchaînerait"
    );
    let vue = snapshot(&b.state, b.zone).await;
    assert_eq!(vue.next_uri, AUTRE, "la suivante reste tenue par la façade");
}

/// Un faux renderer DLNA : il acquitte tout et retient les actions reçues.
struct FauxRenderer {
    hote: String,
    actions: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    tache: tokio::task::JoinHandle<()>,
}

impl Drop for FauxRenderer {
    fn drop(&mut self) {
        self.tache.abort();
    }
}

impl FauxRenderer {
    async fn new() -> Self {
        let actions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let a = actions.clone();
        let app = axum::Router::new().route(
            "/av/control",
            axum::routing::post(move |headers: axum::http::HeaderMap, corps: String| {
                let actions = a.clone();
                async move {
                    let action = headers
                        .get("SOAPAction")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.rsplit('#').next())
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string();
                    actions.lock().unwrap().push((action.clone(), corps));
                    format!(
                        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                         <u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>\
                         </s:Body></s:Envelope>"
                    )
                }
            }),
        );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hote = format!("http://{}", ecoute.local_addr().unwrap());
        let tache = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        Self {
            hote,
            actions,
            tache,
        }
    }

    fn sortie(&self, device: &str) -> DlnaOutput {
        DlnaOutput::new(
            "Diretta (faux)".into(),
            device.into(),
            self.hote.clone(),
            format!("{}/av/control", self.hote),
            format!("{}/rc/control", self.hote),
            None,
        )
    }

    /// Les `SetNextAVTransportURI` vides reçus : l'effacement de la suivante.
    fn effacements(&self) -> usize {
        self.actions
            .lock()
            .unwrap()
            .iter()
            .filter(|(action, corps)| {
                action == "SetNextAVTransportURI" && corps.contains("<NextURI></NextURI>")
            })
            .count()
    }
}

/// Une zone qui joue la courante du contrôleur, sa suivante confiée à la file,
/// sur une sortie DLNA (le faux renderer). Rien ne passe par
/// `orchestrator.play` : la lecture est inscrite telle que le `Play` de la
/// façade la laisse.
async fn zone_dlna_qui_joue(b: &Banc, renderer: &FauxRenderer) {
    let device = format!("{}-dlna", b.device);
    b.state
        .backend
        .execute(
            "UPDATE zones SET output_type = 'dlna', output_device_id = ?1 WHERE id = ?2",
            &[&device, &b.zone],
        )
        .unwrap();
    b.state
        .outputs
        .lock()
        .await
        .register(Box::new(renderer.sortie(&device)));
    b.state
        .playback
        .play(
            b.zone,
            tune_core::playback::NowPlaying {
                title: "Piste 1".into(),
                source: "upnp".into(),
                source_id: Some(COURANTE.into()),
                duration_ms: DUREE_MS,
                ..Default::default()
            },
        )
        .await;
    let play_seq = b.state.playback.get_state(b.zone).await.play_seq;
    if let Ok(mut carte) = sessions().lock() {
        carte.insert(
            b.zone,
            RendererSession {
                uri: COURANTE.into(),
                duration_ms: Some(DUREE_MS),
                revision: nouvelle_revision(),
                play_seq: Some(play_seq),
                next: Some(NextItem {
                    uri: ARMEE.into(),
                    title: None,
                    artist: None,
                    duration_ms: Some(DUREE_MS),
                    metadata: String::new(),
                    confiee: false,
                }),
                ..Default::default()
            },
        );
    }
    assert!(confier_la_suivante_a_la_file(&b.state, b.zone).await);
}

/// 🔴 #6335 (relecture) — la suivante que la façade retire a pu être armée
/// chez le renderer réel, qui l'enchaînerait seul : `SetAVTransportURI` sans
/// `Play` la fait effacer chez lui (`SetNextAVTransportURI` vide).
#[tokio::test]
async fn une_nouvelle_uri_efface_la_suivante_chez_le_renderer_dlna() {
    let b = Banc::new().await;
    let renderer = FauxRenderer::new().await;
    zone_dlna_qui_joue(&b, &renderer).await;
    assert_eq!(b.file().len(), 2);
    assert_eq!(renderer.effacements(), 0);

    poser_une_uri_sans_play(&b, AUTRE).await;

    assert!(b.file().is_empty());
    assert_eq!(
        renderer.effacements(),
        1,
        "#6335 : la suivante retirée de la file doit être effacée chez le \
         renderer DLNA — armée par le sondeur, il l'enchaînerait seul"
    );
}

/// #6335 (relecture) — `Stop` : le renderer est arrêté, rien à effacer.
#[tokio::test]
async fn un_stop_n_efface_pas_la_suivante_chez_le_renderer() {
    let b = Banc::new().await;
    let renderer = FauxRenderer::new().await;
    zone_dlna_qui_joue(&b, &renderer).await;

    b.controleur.stop().await.ok();

    assert!(b.file().is_empty());
    assert_eq!(renderer.effacements(), 0);
}

/// #6335 (relecture rc7, point 3) — répétition de TOUTE la file sur la zone :
/// `[courante, suivante]` reviendrait à l'ancienne courante après la
/// suivante. La suivante n'est pas confiée : la façade garde son
/// enchaînement d'avant, et la zone s'arrête après la suivante comme avant.
#[tokio::test]
async fn la_repetition_de_toute_la_file_ne_confie_pas_la_suivante() {
    let b = Banc::new().await;
    b.state
        .playback
        .set_repeat(b.zone, tune_core::playback::RepeatMode::All)
        .await;

    b.courante_puis_suivante().await;

    assert!(
        b.file().is_empty(),
        "#6335 : avec la répétition de toute la file, [courante, suivante] \
         rejouerait l'ancienne courante après la suivante — la suivante ne \
         doit pas être confiée à la file"
    );
    assert_eq!(b.ligne_que_le_sondeur_armerait().await, None);
    assert_eq!(
        snapshot(&b.state, b.zone).await.next_uri,
        ARMEE,
        "la suivante reste tenue par la façade"
    );
}

/// 🔴 #6335 (relecture rc7, condition d'admission) — la répétition de toute
/// la file activée APRÈS que la suivante a été confiée : une fois que la zone
/// a enchaîné, elle ne doit jamais revenir à la piste précédente.
#[tokio::test]
async fn apres_l_enchainement_la_zone_ne_revient_jamais_a_la_piste_precedente() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    assert_eq!(b.file().len(), 2, "suivante confiée (répétition coupée)");
    b.state
        .playback
        .set_repeat(b.zone, tune_core::playback::RepeatMode::All)
        .await;

    b.vers_la_fin(DUREE_MS - 300).await;
    b.la_zone_enchaine().await;
    assert_eq!(snapshot(&b.state, b.zone).await.uri, ARMEE);

    assert_eq!(
        b.ligne_que_le_sondeur_armerait().await,
        None,
        "#6335 : à la fin de la dernière piste armée par le contrôleur, la \
         zone doit s'arrêter — jamais revenir à la piste précédente, ni \
         rejouer la dernière en boucle (file : {:?})",
        b.file()
    );

    // Le contrôleur arme la piste d'après : la zone suit SON ordre.
    b.controleur
        .set_next_media(&media(AUTRE, "Piste 3"))
        .await
        .expect("la façade doit acquitter la suivante d'après");
    assert_eq!(snapshot(&b.state, b.zone).await.next_uri, AUTRE);
    assert_ne!(
        b.ligne_que_le_sondeur_armerait().await.as_deref(),
        Some(COURANTE),
        "#6335 : la zone ne doit jamais revenir à la piste précédente"
    );
}
