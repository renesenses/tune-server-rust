//! #6008 — le banc : le VRAI sondeur (`tick`) adopte le volume que remonte
//! l'appareil, et le journal doit le DIRE.
//!
//! Fil 2187 : un Cabasse Abyss remonte environ 80 %, Tune l'adopte, et rien
//! au journal ne permettait de savoir d'où venait ce volume. Banc partagé
//! avec #5695 (`volume_pure_5695_tests::Banc`) : une zone DLNA à 100 %, une
//! sortie factice dont on change le volume « depuis la télécommande ».
use super::volume_pure_5695_tests::Banc;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);
impl JournalCapture {
    fn lignes_adoptees(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains("volume_adopte_du_renderer"))
            .map(str::to_owned)
            .collect()
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

/// Pose l'abonné de capture sur le fil du test (runtime `current_thread`).
fn capturer() -> (JournalCapture, tracing::subscriber::DefaultGuard) {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let garde = tracing::subscriber::set_default(abonne);
    (journal, garde)
}

/// LE défaut de #6008 : l'appareil passe à 83 % hors de Tune, le sondeur
/// l'adopte (en base), et le journal doit porter UNE ligne INFO qui dit la
/// zone, l'appareil, l'ancien et le nouveau volume.
#[tokio::test]
async fn l_adoption_du_volume_du_renderer_est_journalisee_6008() {
    let (journal, _garde) = capturer();
    let mut banc = Banc::monter(false).await;
    banc.tic().await; // première observation : rien à adopter.
    banc.appareil_a(0.83).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_base() - 83.0).abs() < 1e-6,
        "témoin : le banc doit passer par le site d'adoption"
    );
    let lignes = journal.lignes_adoptees();
    assert_eq!(lignes.len(), 1, "une adoption, une ligne : {lignes:?}");
    let l = &lignes[0];
    assert!(l.contains("INFO"), "{l}");
    assert!(l.contains(&format!("zone_id={}", banc.zone_id)), "{l}");
    assert!(l.contains(r#"appareil="dlna-my-devialet""#), "{l}");
    assert!(l.contains("ancien=100"), "l'ancien volume (%) : {l}");
    assert!(l.contains("nouveau=83"), "le nouveau volume (%) : {l}");
}

/// Débit : rien tant que la valeur ne bouge pas, une ligne par vrai
/// changement.
#[tokio::test]
async fn une_ligne_par_changement_reel_rien_si_la_valeur_ne_bouge_pas_6008() {
    let (journal, _garde) = capturer();
    let mut banc = Banc::monter(false).await;
    banc.tic().await;
    banc.appareil_a(0.83).await;
    for _ in 0..5 {
        banc.tic().await;
    }
    banc.appareil_a(0.6).await;
    for _ in 0..5 {
        banc.tic().await;
    }
    let lignes = journal.lignes_adoptees();
    assert_eq!(
        lignes.len(),
        2,
        "deux changements, deux lignes : {lignes:?}"
    );
    assert!(lignes[1].contains("ancien=83") && lignes[1].contains("nouveau=60"));
}
