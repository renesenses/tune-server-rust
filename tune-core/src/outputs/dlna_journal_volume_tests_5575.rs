//! #5575 / #5793 — l'acquittement de `SetVolume` au journal INFO, borné, et la
//! relecture qui dit si l'appareil a VRAIMENT appliqué.
//!
//! Banc : un faux renderer 0–255 sur `LF`/`RF` (la forme du défaut du
//! darTZeel de Sevy). Deux variantes : il applique ce qu'il acquitte, ou il
//! acquitte tout et n'applique rien.

use super::*;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Mutex;
use std::time::Duration;

use crate::outputs::dlna_journal_volume;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish(),
        )
    }
    fn lignes(&self, evenement: &str) -> Vec<String> {
        self.text()
            .lines()
            .filter(|l| l.contains(evenement))
            .map(str::to_string)
            .collect()
    }
}

const OK: &str = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:SetVolumeResponse xmlns:u=\"urn:schemas-upnp-org:service:RenderingControl:1\"/></s:Body></s:Envelope>";

const SCPD_255_LF_RF: &str = r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0"><serviceStateTable>
  <stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType>
    <allowedValueList><allowedValue>LF</allowedValue><allowedValue>RF</allowedValue></allowedValueList>
  </stateVariable>
  <stateVariable sendEvents="no"><name>Volume</name><dataType>ui2</dataType>
    <allowedValueRange><minimum>0</minimum><maximum>255</maximum><step>1</step></allowedValueRange>
  </stateVariable>
</serviceStateTable></scpd>"#;

/// Volume courant du faux appareil et nombre d'ordres reçus.
#[derive(Default)]
struct Etat {
    volume: u32,
    ordres: usize,
    lectures: usize,
}

struct FauxRenderer {
    host: String,
    etat: Arc<Mutex<Etat>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FauxRenderer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `applique` : le faux appareil retient-il ce qu'il acquitte ?
async fn faux_renderer(applique: bool, volume_initial: u32) -> FauxRenderer {
    let etat = Arc::new(Mutex::new(Etat {
        volume: volume_initial,
        ..Etat::default()
    }));
    let e = etat.clone();
    let app = Router::new()
        .route("/rc/scpd.xml", get(|| async { SCPD_255_LF_RF }))
        .route(
            "/rc/control",
            post(move |headers: axum::http::HeaderMap, corps: String| {
                let etat = e.clone();
                async move {
                    let action = headers
                        .get("SOAPAction")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let mut e = etat.lock().unwrap();
                    if action.contains("#SetVolume") {
                        e.ordres += 1;
                        if applique
                            && let Some(n) = extract_tag(&corps, "DesiredVolume")
                                .and_then(|v| v.trim().parse::<u32>().ok())
                        {
                            e.volume = n;
                        }
                        return OK.to_string();
                    }
                    if action.contains("#GetVolume") {
                        e.lectures += 1;
                        return format!(
                            "<s:Envelope><s:Body><u:GetVolumeResponse><CurrentVolume>{}</CurrentVolume></u:GetVolumeResponse></s:Body></s:Envelope>",
                            e.volume
                        );
                    }
                    OK.to_string()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FauxRenderer { host, etat, task }
}

fn sortie(r: &FauxRenderer) -> DlnaOutput {
    DlnaOutput::new(
        "Salon LHC".into(),
        "uuid:5575".into(),
        r.host.clone(),
        format!("{}/av/control", r.host),
        format!("{}/rc/control", r.host),
        None,
    )
    .with_rendering_control_scpd(Some(format!("{}/rc/scpd.xml", r.host)))
}

/// Cent ordres en rafale : au plus DEUX lignes INFO sur une rafale de moins de 2 s (la première, puis le
/// rattrapage qui porte la dernière valeur), et une seule relecture.
#[tokio::test]
async fn cent_ordres_en_rafale_donnent_au_plus_deux_lignes_info() {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Capture::default();
    let _garde = journal.subscribe();
    let r = faux_renderer(true, 0).await;
    let out = sortie(&r);

    let debut = std::time::Instant::now();
    for i in 1..=100u32 {
        out.set_volume(f64::from(i) / 100.0).await.unwrap();
    }
    // Borne du module : 1 + ⌈durée / INTERVALLE⌉. Sur une rafale de moins de
    // 2 s (le cas courant), c'est 2.
    let duree = debut.elapsed();
    let borne = 1 + duree
        .as_millis()
        .div_ceil(dlna_journal_volume::INTERVALLE.as_millis()) as usize;
    assert_eq!(
        r.etat.lock().unwrap().ordres,
        200,
        "LF puis RF à chaque ordre"
    );
    // La fenêtre se referme : le rattrapage écrit la dernière valeur.
    tokio::time::sleep(dlna_journal_volume::INTERVALLE + Duration::from_millis(400)).await;

    let lignes = journal.lignes("dlna_set_volume_ok");
    assert!(
        !lignes.is_empty() && lignes.len() <= borne.max(2),
        "100 ordres en {duree:?} doivent donner au plus {} lignes INFO, pas {} :\n{}",
        borne.max(2),
        lignes.len(),
        journal.text()
    );
    let derniere = lignes.last().unwrap();
    for attendu in [
        "zone=Salon LHC",
        "volume_pct=100",
        "niveau=255",
        "canal=LF,RF",
        "instance=0",
        "reponse=OK",
    ] {
        assert!(
            derniere.contains(attendu),
            "« {attendu} » absent : {derniere}"
        );
    }
    assert!(
        r.etat.lock().unwrap().lectures == 1,
        "une seule relecture GetVolume par session de zone"
    );
}

/// L'appareil applique : la relecture le dit, une fois.
#[tokio::test]
async fn la_relecture_dit_que_l_appareil_a_applique() {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Capture::default();
    let _garde = journal.subscribe();
    let r = faux_renderer(true, 40).await;
    let out = sortie(&r);

    out.set_volume(0.5).await.unwrap();
    out.set_volume(0.6).await.unwrap();

    let relues = journal.lignes("dlna_volume_relu");
    assert_eq!(
        relues.len(),
        1,
        "une relecture par session :\n{}",
        journal.text()
    );
    let l = &relues[0];
    for attendu in [
        "zone=Salon LHC",
        "canal=LF ",
        "attendu=128",
        "lu=128",
        "applique=oui",
    ] {
        assert!(l.contains(attendu), "« {attendu} » absent : {l}");
    }
}

/// L'appareil acquitte et n'applique rien (la forme du défaut de Sevy) : la
/// relecture le DIT — attendu 128, lu 40.
#[tokio::test]
async fn la_relecture_denonce_un_acquittement_sans_effet() {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Capture::default();
    let _garde = journal.subscribe();
    let r = faux_renderer(false, 40).await;
    let out = sortie(&r);

    out.set_volume(0.5).await.unwrap();

    let relues = journal.lignes("dlna_volume_relu");
    assert_eq!(relues.len(), 1, "{}", journal.text());
    let l = &relues[0];
    for attendu in ["attendu=128", "lu=40", "applique=non"] {
        assert!(l.contains(attendu), "« {attendu} » absent : {l}");
    }
    assert!(
        journal.lignes("dlna_set_volume_ok").len() == 1,
        "l'acquittement reste porté en INFO : {}",
        journal.text()
    );
}
