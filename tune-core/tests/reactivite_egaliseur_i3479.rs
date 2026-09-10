//! #3479 — la bascule d'égaliseur dit enfin COMBIEN DE TEMPS elle a mis, et ce
//! qui la retient.
//!
//! # La moitié du message qui n'était instruite nulle part
//!
//! Le 08/09/2026 à 16h56, Reivax66 dépose deux phrases (fil forum 1720, ticket
//! support 101) :
//!
//! > « **Maintenant ça fonctionne.** il reste à améliorer la réactivité mais je
//! > suppose qu'elle doit aussi dépendre du processeur de la machine hébergeant
//! > le serveur. »
//!
//! La première a été instruite ligne à ligne : 25 `eq_change_journal`, deux
//! familles de suspects écartées. **La seconde ne l'a été nulle part** — ni
//! ici, ni dans #3652. Et elle n'est pas une remarque de confort : ce dépôt a
//! déjà eu trois fois ce symptôme, sous trois formes qui n'ont aucune ligne de
//! code en commun.
//!
//! - **#1725 / #1710** — l'égaliseur n'agit qu'à la piste SUIVANTE. Le délai
//!   n'en est pas un : il est infini.
//! - **#2102** — chaque changement coupe et relance le flux. Le délai vaut une
//!   renégociation de sortie.
//! - Et l'attente légitime d'un utilisateur, qui n'est pas un défaut.
//!
//! Ses 25 lignes portent toutes `chemin="local_a_chaud"`, ce qui écarte les
//! deux premières — mais il fallait le code sous les yeux pour le savoir, et
//! aucune des deux durées n'était mesurée. **Le journal ne pouvait donc pas
//! répondre**, et c'était un manque de l'instrument, pas du testeur.
//!
//! # Les deux bords que ce témoin tient
//!
//! 1. **Toute bascule est CHIFFRÉE** : `duree_ms` porte le coût mesuré de la
//!    bascule elle-même — relire la zone, bâtir la cascade, la poser derrière
//!    le mutex que la boucle audio relit à chaque paquet. Sans elle,
//!    « améliorer la réactivité » désigne aussi bien 200 ms que 5 s, et ces
//!    deux valeurs ne désignent pas le même défaut — l'une des deux n'en est
//!    pas un.
//! 2. **Ce qui retient la bascule est NOMMÉ** : `amortissement`. Les deux
//!    chemins d'`apply_eq_change` ne sont pas amortis pareil, et cette
//!    asymétrie est délibérée — le chemin local ne l'est PAS, pour que bouger
//!    un curseur en écoutant s'entende tout de suite (#1725) ; le chemin réseau
//!    l'est, parce qu'y redémarrer le flux est audible (#1710). Un export ne
//!    permettait pas de le lire.
//!
//! # Pourquoi un binaire de test à lui seul
//!
//! Même leçon que #2665, #2890, #3180 et `journal_egaliseur_i3479` : `tracing`
//! met en cache POUR TOUT LE PROCESSUS la décision « ce point d'appel
//! intéresse-t-il quelqu'un ? ». L'abonné est donc GLOBAL et ce fichier ne
//! contient QU'UN test. `autotests = false` dans `tune-core/Cargo.toml` — la
//! cible y est déclarée, sans quoi ce fichier ne serait jamais compilé.

use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as MutexAsync;
use tune_core::db::migrations::run_migrations;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::registry::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::registry::ServiceRegistry;

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn vider(&self) -> String {
        let mut tampon = self.0.lock().unwrap();
        let texte = String::from_utf8_lossy(&tampon).into_owned();
        tampon.clear();
        texte
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

/// Relit la valeur d'un champ dans la ligne réellement émise.
///
/// La lecture se fait sur le TEXTE du journal, pas sur une valeur qu'on aurait
/// gardée sous la main : c'est ce texte-là, et lui seul, qu'on aura entre les
/// mains la prochaine fois qu'un testeur joindra un export.
fn champ<'a>(ligne: &'a str, nom: &str) -> Option<&'a str> {
    let debut = ligne.find(&format!("{nom}="))? + nom.len() + 1;
    let reste = &ligne[debut..];
    Some(match reste.find(' ') {
        Some(fin) => &reste[..fin],
        None => reste.trim_end(),
    })
}

#[tokio::test]
async fn une_bascule_d_eq_chiffre_sa_duree_et_nomme_son_amortissement() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);

    #[cfg_attr(not(feature = "local-audio"), allow(unused_mut))]
    let mut registre = OutputRegistry::new();
    #[cfg(feature = "local-audio")]
    registre.register(Box::new(tune_core::outputs::local::LocalOutput::new(
        "DAC".to_string(),
    )));

    let orch = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(MutexAsync::new(ServiceRegistry::new())),
        Arc::new(MutexAsync::new(registre)),
        None,
    ));

    let zones = ZoneRepo::with_backend(db.clone());
    let zone = zones
        .create("Salon", Some("local"), Some("local:DAC"))
        .unwrap();

    let profil = tune_core::audio::eq::EqProfile {
        enabled: true,
        bands: vec![tune_core::audio::eq::EqBandSpec {
            freq: 80.0,
            gain: 8.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set(
            &format!("zone_{zone}_eq_profile"),
            &serde_json::to_string(&profil).unwrap(),
        )
        .unwrap();

    let _ = capture.vider();

    // ── Bord 1 : la ligne réellement émise porte les deux champs ─────────────
    orch.apply_eq_change(zone).await;
    let journal = capture.vider();
    let ligne = journal
        .lines()
        .find(|l| l.contains("eq_change_journal"))
        .unwrap_or_else(|| panic!("aucune ligne eq_change_journal :\n{journal}"));

    let brut = champ(ligne, "duree_ms").unwrap_or_else(|| {
        panic!(
            "la ligne ne CHIFFRE pas la bascule : sans `duree_ms`, « améliorer \
             la réactivité » désigne aussi bien 200 ms que 5 s, et le journal \
             ne peut pas dire laquelle des deux (#3479) :\n{ligne}"
        )
    });
    let duree_ms: f64 = brut.parse().unwrap_or_else(|_| {
        panic!("`duree_ms` doit être un NOMBRE de millisecondes, pas `{brut}` :\n{ligne}")
    });
    assert!(
        duree_ms.is_finite() && duree_ms >= 0.0,
        "`duree_ms` vaut {duree_ms} : une durée mesurée est finie et positive :\n{ligne}"
    );

    let amortissement = champ(ligne, "amortissement").unwrap_or_else(|| {
        panic!(
            "la ligne ne NOMME pas ce qui retient la bascule : sans \
             `amortissement`, un export ne permet pas de dire si le délai vient \
             d'un anti-rebond du serveur ou d'autre chose (#3479) :\n{ligne}"
        )
    });
    // Rien ne joue : aucun redémarrage n'a pu être programmé.
    assert_eq!(
        amortissement, "sans_objet",
        "aucun chemin n'a été emprunté ici — l'amortissement doit le dire :\n{ligne}"
    );

    // ── Bord 2 : l'asymétrie des deux chemins est LISIBLE ────────────────────
    //
    // C'est elle, et elle seule, qui permet d'écarter #2102 et #1725/#1710
    // depuis un simple export. Le témoin n'assert pas des valeurs recopiées du
    // source — il assert le CONTRAT : le chemin local n'est pas amorti, le
    // chemin réseau l'est, et les deux ne se confondent pas.
    let local = PlaybackOrchestrator::amortissement_du_chemin("local_a_chaud");
    let reseau = PlaybackOrchestrator::amortissement_du_chemin("replay_programme");

    assert_eq!(
        local, "aucun",
        "le chemin local n'est PAS amorti, et c'est délibéré : l'`EqProcessor` \
         vit derrière un mutex relu à chaque paquet, donc chaque cran de \
         curseur le refait — c'est ce qui rend audible le geste que #1725 a \
         rendu possible. Le journal doit le dire, sans quoi on cherchera un \
         anti-rebond qui n'existe pas."
    );
    assert!(
        reseau.contains("anti_rebond"),
        "le chemin réseau REDÉMARRE le flux : sans anti-rebond, un curseur de \
         31 bandes produirait 31 coupures (#1710). Le journal doit le nommer, \
         sinon on imputera à la machine du testeur un délai que le serveur \
         s'impose lui-même. Valeur lue : `{reseau}`"
    );
    assert_ne!(
        local, reseau,
        "les deux chemins seraient amortis pareil — c'est justement \
         l'asymétrie qui permet d'écarter #2102 depuis un export : un \
         `local_a_chaud` ne redémarre AUCUN flux."
    );
}
