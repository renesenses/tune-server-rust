//! Garde légère : chaque ordre de pause ou de reprise laisse UNE ligne `info!`
//! au journal, avec la zone et l'origine — comme `api_next_requested`.
//!
//! Avant, ni la route `pause`, ni `orchestrator.pause`, ni la reprise ne
//! laissaient de trace au niveau info (hors sortie Windows exclusive) : un
//! journal de testeur ne permettait pas de dire qui avait mis la zone en pause.
//!
//! Aucun témoin voisin ne capte le journal pour `api_next_requested` (il
//! faudrait un abonné `tracing` GLOBAL, donc un binaire à lui seul) : la garde
//! lit donc le texte des points d'entrée, fonction par fonction.

const PLAYBACK: &str = include_str!("../playback.rs");
const RENDERER: &str = include_str!("../upnp_media_renderer.rs");
const GREFFONS: &str = include_str!("../../plugins_host.rs");
const ASSISTANT: &str = include_str!("../../../../tune-core/src/ai/executor.rs");

/// Le corps qui suit `debut`, jusqu'au prochain `fn ` de même niveau.
fn corps<'a>(source: &'a str, debut: &str) -> &'a str {
    let i = source
        .find(debut)
        .unwrap_or_else(|| panic!("point d'entrée introuvable : {debut}"));
    let reste = &source[i + debut.len()..];
    let fin = reste.find("\nasync fn ").unwrap_or(reste.len());
    let fin = fin.min(reste.find("\n    fn ").unwrap_or(reste.len()));
    let fin = fin.min(reste.find("\n    async fn ").unwrap_or(reste.len()));
    &reste[..fin]
}

fn exige(corps: &str, origine: &str, evenement: &str, lieu: &str) {
    // Sans blancs : rustfmt replie l'appel sur plusieurs lignes quand il est long.
    let corps: String = corps.split_whitespace().collect();
    let ligne = format!("origine=\"{origine}\",\"{evenement}\")");
    assert!(
        corps.contains(&ligne),
        "{lieu} : la ligne `{evenement}` (origine {origine}) a disparu du journal"
    );
    assert_eq!(
        corps.matches(&format!("\"{evenement}\")")).count(),
        1,
        "{lieu} : une seule ligne `{evenement}` par ordre, pas de répétition"
    );
}

#[test]
fn chaque_pause_et_chaque_reprise_laissent_une_ligne_info() {
    exige(
        corps(PLAYBACK, "async fn pause("),
        "api",
        "pause_requested",
        "route pause",
    );
    exige(
        corps(PLAYBACK, "async fn resume("),
        "api",
        "resume_requested",
        "route resume",
    );
    let transfert = corps(PLAYBACK, "if source_paused {");
    let transfert = &transfert[..transfert.find("Err(e) =>").unwrap_or(transfert.len())];
    exige(
        transfert,
        "transfert",
        "pause_requested",
        "transfert d'une zone en pause",
    );
    let pause_renderer = corps(RENDERER, "RendererCommand::Pause => {");
    let pause_renderer = &pause_renderer[..pause_renderer
        .find("RendererCommand::Stop")
        .unwrap_or(pause_renderer.len())];
    exige(
        pause_renderer,
        "renderer",
        "pause_requested",
        "renderer UPnP, Pause",
    );
    let lecture_renderer = corps(RENDERER, "RendererCommand::Play => {");
    let lecture_renderer = &lecture_renderer[..lecture_renderer
        .find("RendererCommand::Pause")
        .unwrap_or(lecture_renderer.len())];
    exige(
        lecture_renderer,
        "renderer",
        "resume_requested",
        "renderer UPnP, Play de reprise",
    );
    exige(
        corps(GREFFONS, "fn pause(&self, zone: i64)"),
        "greffon",
        "pause_requested",
        "hôte de greffons",
    );
    exige(
        corps(ASSISTANT, "async fn pause(&self)"),
        "assistant",
        "pause_requested",
        "assistant, pause",
    );
    exige(
        corps(ASSISTANT, "async fn resume(&self)"),
        "assistant",
        "resume_requested",
        "assistant, reprise",
    );
}
