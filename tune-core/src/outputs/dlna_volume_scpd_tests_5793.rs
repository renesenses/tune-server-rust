//! #5793 — un faux renderer qui ACQUITTE tout `SetVolume` et n'applique que
//! ce que son SCPD annonce.
//!
//! C'est la forme du défaut rapporté sur le darTZeel LHC-208 (fil 2147) :
//! aucune faute SOAP, aucun `dlna_set_volume_rejected`, et pas de changement
//! de volume. Ce banc ne prétend pas reproduire CET appareil, dont le SCPD
//! n'a pas été lu : il prouve que Tune commande désormais dans l'unité et sur
//! le canal que l'appareil déclare, au lieu de 0–100 sur `Master`.
use super::*;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Mutex;

const OK: &str = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:SetVolumeResponse xmlns:u=\"urn:schemas-upnp-org:service:RenderingControl:1\"/></s:Body></s:Envelope>";

/// SCPD d'un appareil qui compte de 0 à 255 et n'accepte que `LF`/`RF`.
const SCPD_255_LF_RF: &str = r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0"><serviceStateTable>
  <stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType>
    <allowedValueList><allowedValue>LF</allowedValue><allowedValue>RF</allowedValue></allowedValueList>
  </stateVariable>
  <stateVariable sendEvents="no"><name>Volume</name><dataType>ui2</dataType>
    <allowedValueRange><minimum>0</minimum><maximum>255</maximum><step>1</step></allowedValueRange>
  </stateVariable>
</serviceStateTable></scpd>"#;

/// Ce que l'appareil a réellement APPLIQUÉ, par canal.
#[derive(Default)]
struct Applique {
    par_canal: HashMap<String, u32>,
    ordres: usize,
}

struct FauxRenderer {
    host: String,
    applique: Arc<Mutex<Applique>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FauxRenderer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn champ(corps: &str, tag: &str) -> Option<String> {
    extract_tag(corps, tag).map(|v| v.trim().to_string())
}

/// Le faux appareil : 200 à tout, n'applique que `LF`/`RF` dans 0–255.
async fn faux_renderer() -> FauxRenderer {
    let applique = Arc::new(Mutex::new(Applique::default()));
    let etat = applique.clone();
    let app = Router::new()
        .route("/rc/scpd.xml", get(|| async { SCPD_255_LF_RF }))
        .route(
            "/rc/control",
            post(move |headers: axum::http::HeaderMap, corps: String| {
                let etat = etat.clone();
                async move {
                    let action = headers
                        .get("SOAPAction")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let canal = champ(&corps, "Channel").unwrap_or_default();
                    if action.contains("#SetVolume") {
                        let mut e = etat.lock().unwrap();
                        e.ordres += 1;
                        let niveau = champ(&corps, "DesiredVolume").and_then(|v| v.parse::<u32>().ok());
                        if let Some(n) = niveau
                            && (canal == "LF" || canal == "RF")
                            && n <= 255
                        {
                            e.par_canal.insert(canal, n);
                        }
                        return OK.to_string();
                    }
                    if action.contains("#GetVolume") {
                        let e = etat.lock().unwrap();
                        let n = e.par_canal.get(&canal).copied().unwrap_or(0);
                        return format!(
                            "<s:Envelope><s:Body><u:GetVolumeResponse><CurrentVolume>{n}</CurrentVolume></u:GetVolumeResponse></s:Body></s:Envelope>"
                        );
                    }
                    OK.to_string()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    FauxRenderer {
        host,
        applique,
        task,
    }
}

fn sortie(r: &FauxRenderer, scpd: Option<String>) -> DlnaOutput {
    DlnaOutput::new(
        "LHC banc".into(),
        "uuid:5793".into(),
        r.host.clone(),
        format!("{}/av/control", r.host),
        format!("{}/rc/control", r.host),
        None,
    )
    .with_rendering_control_scpd(scpd)
}

#[tokio::test]
async fn le_volume_part_dans_l_unite_et_sur_les_canaux_du_scpd() {
    let r = faux_renderer().await;
    let out = sortie(&r, Some(format!("{}/rc/scpd.xml", r.host)));

    out.set_volume(0.5).await.unwrap();
    {
        let a = r.applique.lock().unwrap();
        assert_eq!(
            a.par_canal.get("LF"),
            Some(&128),
            "LF doit recevoir 128/255"
        );
        assert_eq!(
            a.par_canal.get("RF"),
            Some(&128),
            "RF doit recevoir 128/255"
        );
    }

    // La baisse du fil 2147 : 0,18 puis 0,12 puis 0.
    for (v, attendu) in [(0.18, 46u32), (0.12, 31), (0.0, 0)] {
        out.set_volume(v).await.unwrap();
        let a = r.applique.lock().unwrap();
        assert_eq!(a.par_canal.get("LF"), Some(&attendu), "volume {v}");
        assert_eq!(a.par_canal.get("RF"), Some(&attendu), "volume {v}");
    }

    // Et la relecture revient dans l'échelle de Tune.
    out.set_volume(0.2).await.unwrap();
    let lu = out.lire_volume().await.unwrap();
    assert!((lu - 0.2).abs() < 0.01, "relu {lu}");
    // L'état mémorisé (régime évènements) aussi.
    assert!((out.fraction_du_niveau(51) - 0.2).abs() < 1e-9);
}

/// Témoin : la conduite d'avant #5793 (0–100 sur `Master`, sans SCPD) est
/// acquittée par ce même appareil… et n'applique RIEN. C'est le symptôme.
#[tokio::test]
async fn temoin_sans_scpd_l_ordre_est_acquitte_et_rien_n_est_applique() {
    let r = faux_renderer().await;
    let out = sortie(&r, None);
    out.set_volume(0.5).await.unwrap();
    let a = r.applique.lock().unwrap();
    assert_eq!(a.ordres, 1, "un seul SetVolume, sur Master");
    assert!(
        a.par_canal.is_empty(),
        "rien d'appliqué : {:?}",
        a.par_canal
    );
}

/// Un SCPD injoignable ne bloque pas la commande : profil standard, et
/// nouvel essai à la commande suivante.
#[tokio::test]
async fn un_scpd_injoignable_retombe_sur_le_profil_standard() {
    let r = faux_renderer().await;
    let out = sortie(&r, Some(format!("{}/absent.xml", r.host)));
    out.set_volume(0.5).await.unwrap();
    assert!(
        out.profil_volume.get().is_none(),
        "un échec ne fige pas le profil"
    );
    assert_eq!(r.applique.lock().unwrap().ordres, 1);
}
