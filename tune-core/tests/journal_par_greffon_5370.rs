//! #5370 — une ligne de journal par greffon chargé, avec sa durée.
//!
//! Binaire à lui seul, et pas test unitaire de `plugin_sdk` : tracing met en
//! cache, par ligne de journal, « quelqu'un écoute-t-il ? ». Dans le binaire
//! de la bibliothèque, un test voisin qui charge un greffon SANS abonné, sur un
//! autre fil, peut rencontrer `plugin_loaded` le premier et figer la réponse à
//! « personne » : mesuré sur Shrek, 3 rouges sur 3 puis 2 sur 5, vert seul.
//! Ici, personne d'autre ne passe par ces lignes.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use tune_core::plugin_sdk::{PluginContext, PluginLoader, TunePlugin};

// ── #5370 — une ligne de journal par greffon, avec sa durée ──────────

#[derive(Clone, Default)]
struct JournalCapture(Arc<StdMutex<Vec<u8>>>);

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

/// Un greffon dont le `setup()` prend le temps qu'on lui dit.
struct GreffonMesure {
    nom: &'static str,
    attente: Duration,
}

#[async_trait]
impl TunePlugin for GreffonMesure {
    fn name(&self) -> &str {
        self.nom
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn description(&self) -> &str {
        "greffon de mesure"
    }
    async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
        tokio::time::sleep(self.attente).await;
        Ok(())
    }
    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Les lignes du journal qui parlent du greffon `nom`.
fn lignes_de<'a>(journal: &'a str, nom: &str) -> Vec<&'a str> {
    let marque = format!("plugin_name={nom}");
    journal
        .lines()
        .filter(|l| l.split_whitespace().any(|mot| mot == marque))
        .collect()
}

/// #5370 — le testeur voyait « greffons » pendant un temps « indéfini » et
/// aucun journal ne disait lequel. Chaque greffon chargé doit laisser UNE
/// ligne `plugin_loaded` portant sa durée, et celui qui dépasse le seuil un
/// `plugin_setup_slow` à son nom — pas à celui du voisin.
#[tokio::test]
async fn chaque_greffon_laisse_une_ligne_de_journal_avec_sa_duree() {
    let journal = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _garde = tracing::subscriber::set_default(abonne);

    let dir = tempfile::tempdir().unwrap();
    let loader = PluginLoader::new(dir.path().to_path_buf())
        .with_slow_setup_threshold(Duration::from_millis(100));
    loader
        .register(Box::new(GreffonMesure {
            nom: "rapide",
            attente: Duration::ZERO,
        }))
        .await;
    loader
        .register(Box::new(GreffonMesure {
            nom: "lent",
            attente: Duration::from_millis(250),
        }))
        .await;

    let annonces = StdMutex::new(Vec::<String>::new());
    let loaded = loader
        .setup_all_observed("http://localhost:8888", &|nom| {
            annonces.lock().unwrap().push(nom.to_string())
        })
        .await;
    assert_eq!(loaded, vec!["rapide", "lent"]);
    assert_eq!(
        *annonces.lock().unwrap(),
        vec!["rapide", "lent"],
        "l'hôte doit être prévenu de chaque greffon, dans l'ordre, pour \
         que la page d'attente puisse le nommer"
    );

    let texte = journal.texte();
    for nom in ["rapide", "lent"] {
        let charges: Vec<_> = lignes_de(&texte, nom)
            .into_iter()
            .filter(|l| l.contains("plugin_loaded"))
            .collect();
        assert_eq!(
            charges.len(),
            1,
            "une ligne plugin_loaded, et une seule, pour {nom} :\n{texte}"
        );
        assert!(
            charges[0].contains("duration_ms="),
            "la ligne de {nom} doit porter sa durée :\n{}",
            charges[0]
        );
    }

    let lent = lignes_de(&texte, "lent");
    let alerte = lent
        .iter()
        .find(|l| l.contains("plugin_setup_slow"))
        .unwrap_or_else(|| panic!("aucun plugin_setup_slow pour « lent » :\n{texte}"));
    assert!(alerte.contains("WARN"), "{alerte}");
    assert!(alerte.contains("threshold_ms=100"), "{alerte}");
    let duree: u64 = alerte
        .split_whitespace()
        .find_map(|mot| mot.strip_prefix("duration_ms="))
        .and_then(|v| v.parse().ok())
        .expect("duration_ms absent de l'alerte");
    assert!(duree >= 250, "durée mesurée {duree} ms < 250 ms : {alerte}");

    assert!(
        !lignes_de(&texte, "rapide")
            .iter()
            .any(|l| l.contains("plugin_setup_slow")),
        "le greffon rapide ne doit pas être signalé lent :\n{texte}"
    );
}

/// La valeur numérique d'un champ `cle=` dans une ligne de journal.
fn champ(ligne: &str, cle: &str) -> Option<u64> {
    let prefixe = format!("{cle}=");
    ligne
        .split_whitespace()
        .find_map(|mot| mot.strip_prefix(prefixe.as_str()))
        .and_then(|v| v.parse().ok())
}

/// #5403 × #5370 — la résolution du conflit de `setup_all` garde les DEUX
/// comportements : la borne coupe le greffon qui pend (#5403), et la mesure de
/// #5370 vaut pour tous, coupé compris. La durée est mesurée dans tous les
/// cas, la lenteur signalée au-delà du seuil, et la coupure journalisée AVEC
/// sa durée. Le greffon coupé reste enfin dans le rapport du gestionnaire.
#[tokio::test]
async fn une_coupure_est_mesuree_signalee_lente_et_journalisee_avec_sa_duree_5403() {
    let journal = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _garde = tracing::subscriber::set_default(abonne);

    let dir = tempfile::tempdir().unwrap();
    let loader = PluginLoader::new(dir.path().to_path_buf())
        .with_slow_setup_threshold(Duration::from_millis(100))
        .with_setup_timeout(Duration::from_millis(400));
    for (nom, attente) in [
        ("rapide", Duration::ZERO),
        ("pendu", Duration::from_secs(3600)),
        ("lent", Duration::from_millis(250)),
    ] {
        loader
            .register(Box::new(GreffonMesure { nom, attente }))
            .await;
    }

    let loaded = tokio::time::timeout(
        Duration::from_secs(60),
        loader.setup_all("http://localhost:8888"),
    )
    .await
    .expect("la borne de #5403 doit couper « pendu »");
    assert_eq!(
        loaded,
        vec!["rapide", "lent"],
        "le démarrage continue après la coupure"
    );

    let texte = journal.texte();
    let pendu = lignes_de(&texte, "pendu");
    let coupure = pendu
        .iter()
        .find(|l| l.contains("plugin_setup_timed_out"))
        .unwrap_or_else(|| {
            panic!("aucun plugin_setup_timed_out pour « pendu » (#5403) :\n{texte}")
        });
    assert_eq!(champ(coupure, "timeout_ms"), Some(400), "{coupure}");
    let duree = champ(coupure, "duration_ms")
        .unwrap_or_else(|| panic!("la coupure doit porter sa durée (#5370) :\n{coupure}"));
    assert!(duree >= 400, "durée {duree} ms < borne : {coupure}");
    let lente = pendu
        .iter()
        .find(|l| l.contains("plugin_setup_slow"))
        .unwrap_or_else(|| {
            panic!("la coupure dépasse le seuil : plugin_setup_slow attendu (#5370) :\n{texte}")
        });
    assert!(
        lente.contains("timed_out"),
        "l'alerte doit dire le sort : {lente}"
    );
    assert!(
        !pendu.iter().any(|l| l.contains("plugin_loaded")),
        "{texte}"
    );

    // #5370 intact pour les autres.
    for nom in ["rapide", "lent"] {
        assert!(
            lignes_de(&texte, nom)
                .iter()
                .any(|l| l.contains("plugin_loaded") && champ(l, "duration_ms").is_some()),
            "plugin_loaded avec sa durée attendu pour {nom} :\n{texte}"
        );
    }
    assert!(
        lignes_de(&texte, "lent")
            .iter()
            .any(|l| l.contains("plugin_setup_slow"))
    );
    assert!(
        !lignes_de(&texte, "rapide")
            .iter()
            .any(|l| l.contains("plugin_setup_slow"))
    );

    // #5403 — visible dans le gestionnaire, avec la même durée que le journal.
    let rapport = loader.setup_report().lock().unwrap().clone();
    assert_eq!(rapport.errors.len(), 1, "{:?}", rapport.errors);
    assert_eq!(rapport.errors[0].name, "pendu");
    assert_eq!(rapport.errors[0].reason.as_str(), "setup_timeout");
    assert_eq!(rapport.errors[0].duration_ms, duree);
}
