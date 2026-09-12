//! #3479 — le rapport de bogue dit-il enfin ce que l'étage d'égalisation
//! PRODUIT, et pas seulement ce qu'il annonce ?
//!
//! ## Le constat
//!
//! Reivax66 signale que l'activation de l'égaliseur **coupe le son sans
//! interrompre la lecture**. Trois instruments lui ont été livrés sans jamais
//! toucher au traitement du signal : `eq_change_journal` (v0.9.141),
//! `eq_format_apres_traitement` (v0.9.141), puis `duree_ms` /
//! `amortissement` (v0.9.145). Son export du 08/09, pris en v0.9.142, en porte
//! **25 lignes**, toutes concordantes :
//!
//! ```text
//! famille="locale"  chemin="local_a_chaud"  premier_echec="-"
//! format_avant=44100 Hz / 2 canaux / f32  format_apres=44100 Hz / 2 canaux / f32
//! preamp_db_g=-12.0  preamp_db_d=-12.0  bandes=1
//! ## Ring starvation
//! - Smart DX1 : 0 événement(s), 0 échantillon(s) manquant(s) sur 46 688 256 servis
//! ```
//!
//! Ces lignes disent que l'étage **s'installe**. Aucune ne dit ce qu'il
//! **rend**. Or `EqProcessor::process_interleaved` (`tune-core/src/audio/eq.rs`)
//! remet à ZÉRO tout échantillon non fini et le compte
//! (`non_finite_samples`) : une cascade de biquads devenue instable produit du
//! silence pendant que l'anneau reste alimenté et servi à l'heure. C'est
//! exactement le symptôme décrit, et c'est le seul mécanisme interne à l'étage
//! qui le produise.
//!
//! Ce compteur existait, atteignait déjà `/zones/{id}/signal-path`, et
//! **n'entrait ni dans le rapport que le testeur dépose ni dans le journal**.
//! Mesuré, et illisible — le même angle mort que la famine de l'anneau avant
//! #3205.
//!
//! ## Ce que ces témoins gardent
//!
//! La distinction que #3205 avait déjà dû imposer aux routes `buffer-stats`, et
//! qui vaut ici mot pour mot :
//!
//! | ce que la sortie sait | ce que le rapport doit dire |
//! |---|---|
//! | rien (aucun étage DSP observable) | **aucune ligne** |
//! | elle a compté, et rien n'a été mis à zéro | `0` |
//! | elle a remis 4 096 échantillons à zéro | `4096` |
//!
//! La deuxième ligne est celle qui compte : un `0` MESURÉ écarte une
//! hypothèse ; une ligne fabriquée pour une sortie qui n'observe rien en
//! fabriquerait une.
//!
//! ## Ce que ces témoins NE gardent pas
//!
//! ⛔ Ils n'établissent **pas** la cause du silence de Reivax66, et ce lot ne
//! prétend pas la corriger. Rien ici ne touche au traitement du signal. La
//! machine de compilation n'a pas de carte son : aucune lecture réelle n'y est
//! exécutable. Ce qui est garanti, c'est qu'au prochain export le chiffre y
//! sera.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use tune_core::outputs::traits::OutputDspMetrics;
use tune_core::outputs::{OutputCapabilities, OutputStatus, OutputTarget, TransportState};
use tune_server::state::AppState;

/// Le titre de la section que ce fichier garde.
const SECTION: &str = "## DSP — egaliseur";

/// Une sortie d'essai dont on choisit ce que `dsp_metrics()` rend.
struct SortieDEssai {
    device_id: String,
    nom: String,
    metriques: Option<OutputDspMetrics>,
}

impl SortieDEssai {
    /// Une sortie qui n'observe aucun étage DSP — tout renderer réseau.
    fn sans_dsp(device_id: &str, nom: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            nom: nom.to_string(),
            metriques: None,
        }
    }

    /// Une sortie qui rend l'audio elle-même et compte ce que son étage
    /// d'égalisation a produit.
    fn avec_dsp(device_id: &str, nom: &str, metriques: OutputDspMetrics) -> Self {
        Self {
            device_id: device_id.to_string(),
            nom: nom.to_string(),
            metriques: Some(metriques),
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for SortieDEssai {
    fn name(&self) -> &str {
        &self.nom
    }
    fn device_id(&self) -> &str {
        &self.device_id
    }
    fn output_type(&self) -> &str {
        "essai"
    }
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, false)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn dsp_metrics(&self) -> Option<OutputDspMetrics> {
        self.metriques
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: TransportState::Stopped,
            ..Default::default()
        })
    }
    async fn is_available(&self) -> bool {
        true
    }
}

/// Le rapport tel que le testeur le DÉPOSE, lu par la route montée.
///
/// Appeler la fonction de composition en direct laisserait passer le défaut
/// « écrit mais pas branché » : la section pourrait exister et la route ne
/// jamais l'appeler.
async fn rapport(sorties: Vec<SortieDEssai>) -> String {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite");
    {
        let mut registre = state.outputs.lock().await;
        for sortie in sorties {
            registre.register(Box::new(sortie));
        }
    }
    let app = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(
            Request::get("/api/v1/system/bug-report/markdown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 8 * 1024 * 1024)
        .await
        .expect("corps du rapport");
    let texte = String::from_utf8_lossy(&octets).into_owned();
    assert_eq!(statut, StatusCode::OK, "{texte}");
    texte
}

/// La section du rapport, sans le reste — les journaux récents citent des noms
/// de sortie et feraient passer un `contains` sur le texte entier.
fn section(rapport: &str) -> String {
    let Some(debut) = rapport.find(SECTION) else {
        return String::new();
    };
    let reste = &rapport[debut..];
    match reste[SECTION.len()..].find("\n## ") {
        Some(fin) => reste[..SECTION.len() + fin].to_string(),
        None => reste.to_string(),
    }
}

/// ⭐ LE garde : un étage d'égalisation qui a remis des échantillons à zéro le
/// dit dans le rapport, avec son chiffre.
#[tokio::test]
async fn un_etage_qui_rend_du_silence_le_dit_dans_le_rapport() {
    let texte = rapport(vec![SortieDEssai::avec_dsp(
        "essai:smart-dx1",
        "Smart DX1",
        OutputDspMetrics {
            eq_overs: 7,
            eq_non_finite_samples: 4_096,
        },
    )])
    .await;
    let section = section(&texte);
    assert!(
        !section.is_empty(),
        "#3479 — le rapport ne porte AUCUNE section « {SECTION} » : \
         `eq_non_finite_samples` reste mesuré et illisible, et le seul \
         mécanisme interne à l'étage qui produise le symptôme décrit \
         (« coupe le son sans interrompre la lecture ») ne laisse toujours \
         aucune trace dans ce que le testeur dépose.\n---\n{texte}"
    );
    assert!(
        section.contains("Smart DX1"),
        "la sortie doit être nommée :\n{section}"
    );
    assert!(
        section.contains("4096"),
        "#3479 — le nombre d'échantillons remis à ZÉRO est LE chiffre de cette \
         section ; sans lui elle ne tranche rien :\n{section}"
    );
    assert!(
        section.contains('7'),
        "la saturation compte aussi : elle dit l'inverse du silence :\n{section}"
    );
}

/// ⭐⭐ La contre-épreuve qui donne son sens au ticket : un `0` **mesuré** est
/// une information — il écarte le repliement sur zéro — et il ne doit pas se
/// confondre avec une sortie qui n'observe rien.
///
/// Sans ce témoin, on pourrait « corriger » le défaut en n'écrivant la section
/// que lorsque le compteur est non nul, et un export sain ne dirait plus rien
/// du tout : on ne saurait pas distinguer « l'EQ n'a rien mis à zéro » de
/// « personne n'a regardé ».
#[tokio::test]
async fn un_zero_mesure_se_distingue_d_une_sortie_qui_n_observe_rien() {
    let texte = rapport(vec![
        SortieDEssai::avec_dsp(
            "essai:local-sain",
            "Sortie locale saine",
            OutputDspMetrics {
                eq_overs: 0,
                eq_non_finite_samples: 0,
            },
        ),
        SortieDEssai::sans_dsp("essai:denon-dlna", "Denon AVR-X1600H"),
    ])
    .await;
    let section = section(&texte);
    assert!(
        section.contains("Sortie locale saine"),
        "#3479 — une sortie qui a COMPTÉ et n'a rien mis à zéro doit figurer \
         avec son `0` : c'est ce zéro-là qui écarte une hypothèse :\n{section}"
    );
    assert!(
        !section.contains("Denon AVR-X1600H"),
        "#3479 — une sortie qui n'observe aucun étage DSP ne doit pas se voir \
         fabriquer un `0` : ce serait le défaut exact de #3205, un zéro qui se \
         lit comme « mesuré, et sain » alors que rien n'a été mesuré :\n{section}"
    );
}

/// Aucune sortie observable du tout : pas de section vide.
///
/// Une section titrée sans une seule ligne se lirait comme « mesuré, rien à
/// signaler » sur une installation où aucune sortie ne sait compter.
#[tokio::test]
async fn aucune_sortie_observable_ne_produit_aucune_section() {
    let texte = rapport(vec![SortieDEssai::sans_dsp("essai:reseau", "Renderer")]).await;
    assert!(
        section(&texte).is_empty(),
        "#3479 — aucune sortie n'observe d'étage DSP : le rapport ne doit pas \
         porter une section vide, qui se lirait comme une mesure"
    );
}
