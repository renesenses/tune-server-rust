//! #5662 — un volume illisible se replie sur 0,5, et le journal le dit.
//!
//! `DlnaOutput::lire_volume` remplace toute réponse qu'il ne sait pas lire
//! (corps vide, `CurrentVolume` absent, valeur non numérique, échec SOAP du
//! `GetGroupVolume` d'un Sonos) par 0,5. Ce repli reste en place : il évite
//! qu'un renderer bavard fasse tomber la lecture du statut. Mais il était
//! MUET, et un rapport ne pouvait pas distinguer un appareil réellement à
//! 50 % d'un appareil dont Tune n'a pas compris la réponse.
//!
//! Une ligne WARN par épisode (`dlna_volume_illisible`), avec la raison et un
//! extrait borné de la réponse ; une ligne INFO quand une réponse redevient
//! lisible (`dlna_volume_relisible`). Le sondage ne répète pas la ligne à
//! chaque tick.

use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{info, warn};

/// Longueur maximale, en caractères, de l'extrait de réponse journalisé.
pub(crate) const EXTRAIT_MAX: usize = 200;

/// Le volume que `lire_volume` rend quand la réponse est illisible.
pub(crate) const REPLI_ILLISIBLE: f64 = 0.5;

/// Pourquoi la réponse n'a pas donné de volume.
pub(crate) fn raison_illisible(reponse: &str) -> &'static str {
    if reponse.trim().is_empty() {
        return "reponse_vide";
    }
    match super::extract_tag(reponse, "CurrentVolume") {
        None => "current_volume_absent",
        Some(v) if v.trim().parse::<f64>().is_err() => "current_volume_non_numerique",
        // Ne devrait pas arriver : l'appelant n'appelle ici qu'en échec.
        Some(_) => "inconnue",
    }
}

/// Extrait borné de la réponse, sur une ligne : les blancs sont réduits à un
/// espace et la longueur plafonnée à [`EXTRAIT_MAX`] caractères.
pub(crate) fn extrait_borne(reponse: &str) -> String {
    let aplati = reponse.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut extrait: String = aplati.chars().take(EXTRAIT_MAX).collect();
    if aplati.chars().count() > EXTRAIT_MAX {
        extrait.push('…');
    }
    extrait
}

/// Journal par épisode : `true` dans `en_cours` tant que les réponses restent
/// illisibles.
pub(crate) fn constater_illisible(en_cours: &AtomicBool, zone: &str, raison: &str, reponse: &str) {
    if en_cours.swap(true, Ordering::Relaxed) {
        return;
    }
    warn!(
        zone = %zone,
        raison = %raison,
        repli = REPLI_ILLISIBLE,
        extrait = %extrait_borne(reponse),
        "dlna_volume_illisible"
    );
}

/// Une réponse lisible referme l'épisode en cours, s'il y en a un.
pub(crate) fn constater_lisible(en_cours: &AtomicBool, zone: &str, volume: f64) {
    if en_cours.swap(false, Ordering::Relaxed) {
        info!(zone = %zone, volume, "dlna_volume_relisible");
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use axum::{Router, routing::post};
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct JournalCapture(Arc<Mutex<Vec<u8>>>);
    impl JournalCapture {
        fn texte(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }
    impl std::io::Write for JournalCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
        type Writer = JournalCapture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn la_raison_nomme_le_defaut() {
        assert_eq!(raison_illisible("  "), "reponse_vide");
        assert_eq!(raison_illisible("<Body/>"), "current_volume_absent");
        assert_eq!(
            raison_illisible("<CurrentVolume>abc</CurrentVolume>"),
            "current_volume_non_numerique"
        );
    }

    #[test]
    fn l_extrait_est_borne_et_sur_une_ligne() {
        let long = format!("<a>\n{}\n</a>", "x".repeat(1000));
        let e = extrait_borne(&long);
        assert!(!e.contains('\n'));
        assert_eq!(e.chars().count(), EXTRAIT_MAX + 1, "200 caractères + …");
        assert_eq!(extrait_borne("<a> b </a>"), "<a> b </a>");
    }

    /// Faux renderer : `GetVolume` rend `corps`, modifiable en cours de test.
    async fn faux_renderer(corps: Arc<Mutex<String>>) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/rc/control",
            post(move || {
                let corps = corps.clone();
                async move { corps.lock().unwrap().clone() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (host, task)
    }

    /// Une réponse illisible donne UNE ligne WARN avec sa raison et son
    /// extrait, quel que soit le nombre de lectures ; le repli 0,5 est
    /// inchangé ; le retour à une réponse lisible écrit la fin d'épisode.
    #[tokio::test]
    async fn une_reponse_illisible_est_journalisee_une_fois_par_episode() {
        crate::journal_de_test::fiabiliser_la_capture();
        let corps = Arc::new(Mutex::new(
            "<s:Envelope><s:Body><u:GetVolumeResponse><CurrentVolume>n/a</CurrentVolume>\
             </u:GetVolumeResponse></s:Body></s:Envelope>"
                .to_string(),
        ));
        let (host, task) = faux_renderer(corps.clone()).await;
        let out = DlnaOutput::new(
            "Banc 5662".into(),
            "uuid:5662".into(),
            host.clone(),
            format!("{host}/av/control"),
            format!("{host}/rc/control"),
            None,
        );

        let journal = JournalCapture::default();
        let abonne = tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let garde = tracing::subscriber::set_default(abonne);
        for _ in 0..3 {
            let lu = out.lire_volume().await.unwrap();
            assert_eq!(lu, REPLI_ILLISIBLE, "le repli reste 0,5");
        }
        *corps.lock().unwrap() =
            "<s:Envelope><s:Body><CurrentVolume>40</CurrentVolume></s:Body></s:Envelope>".into();
        let lu = out.lire_volume().await.unwrap();
        assert!((lu - 0.4).abs() < 1e-9, "relu {lu}");
        let _ = out.lire_volume().await.unwrap();
        drop(garde);
        task.abort();

        let texte = journal.texte();
        let illisibles: Vec<&str> = texte
            .lines()
            .filter(|l| l.contains("dlna_volume_illisible"))
            .collect();
        assert_eq!(
            illisibles.len(),
            1,
            "une seule ligne pour l'épisode — journal :\n{texte}"
        );
        assert!(illisibles[0].contains("WARN"), "{}", illisibles[0]);
        assert!(
            illisibles[0].contains("current_volume_non_numerique"),
            "la raison doit être nommée : {}",
            illisibles[0]
        );
        assert!(
            illisibles[0].contains("<CurrentVolume>n/a</CurrentVolume>"),
            "l'extrait de la réponse doit figurer : {}",
            illisibles[0]
        );
        assert_eq!(
            texte
                .lines()
                .filter(|l| l.contains("dlna_volume_relisible"))
                .count(),
            1,
            "une seule fin d'épisode — journal :\n{texte}"
        );
    }
}
