//! `GET /system/peers` doit rendre le registre manuel UNI à la découverte mDNS
//! (#2746).
//!
//! ## Le défaut
//!
//! `system_peers` n'itérait que `load_peers()`, le registre manuel persisté.
//! `discovered_tune_peers()` existait, était juste, et n'avait que deux
//! appelants — `routes/peers.rs` et `/system/discover-servers` — dont AUCUN
//! n'était la route du panneau Réseau. Sur un réseau où le multicast passe et
//! où mDNS voit d'autres serveurs Tune, l'écran affichait quand même « aucun
//! serveur Tune détecté ». Écrit, mais pas branché.
//!
//! ## Ce que ces essais mesurent
//!
//! Le CORPS JSON de la réponse, jamais la condition qui le produit. Aucun
//! d'eux ne relit la règle de fusion ni ne la recopie : ils appellent
//! `peers_payload`, exactement ce que la route appelle, et lisent ce qui en
//! sort.
//!
//! ## Pourquoi `peers_payload` et pas seulement le routeur
//!
//! `MdnsScanner` garde ses appareils dans un `Arc<Mutex<MdnsState>>` privé,
//! rempli par un démon multicast : aucun point d'injection, et un test qui
//! dépendrait d'un vrai trafic mDNS serait instable sur un runner. La
//! découverte est donc un PARAMÈTRE de `peers_payload`, que `system_peers`
//! remplit d'une seule ligne — et `la_route_du_panneau_reste_branchee` garde
//! cette ligne, pour que la couture ne puisse pas être vraie sans être
//! appelée.
//!
//! Le témoin, lui, passe par le VRAI routeur : c'est là que la forme de la
//! réponse est vérifiée face à ce que le client consomme réellement.
//!
//! ## Les adresses employées
//!
//! `192.0.2.0/24` (TEST-NET-1, RFC 5737) : jamais l'adresse locale de la
//! machine d'essai, donc jamais filtrée comme « soi-même », et jamais un vrai
//! serveur Tune. Les pairs y sont donc tous injoignables, rendus
//! `online: false` — ce qui suffit : ces essais portent sur QUI figure dans la
//! liste, pas sur ce qu'un pair joignable y ajoute.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Écrit le registre manuel tel que `POST /system/peers` le persiste.
fn registre_manuel(state: &AppState, pairs: &[(&str, u16)]) {
    let entrees: Vec<Value> = pairs
        .iter()
        .map(|(host, port)| json!({ "host": host, "port": port }))
        .collect();
    SettingsRepo::with_backend(state.backend.clone())
        .set("tune_peers", &serde_json::to_string(&entrees).unwrap())
        .unwrap();
}

/// Une fiche telle que `discovered_tune_peers()` la rend.
fn decouvert(host: &str, port: u16) -> Value {
    json!({
        "id": format!("tune-{host}-{port}"),
        "name": "Tune (salon)",
        "host": host,
        "port": port,
        "available": true,
        "version": "0.9.128",
    })
}

/// Les couples `host:port` du corps rendu, dans l'ordre.
fn adresses(corps: &Value) -> Vec<String> {
    corps
        .as_array()
        .unwrap_or_else(|| panic!("le corps de /system/peers doit être un TABLEAU, reçu : {corps}"))
        .iter()
        .map(|p| {
            format!(
                "{}:{}",
                p.get("host").and_then(Value::as_str).unwrap_or("?"),
                p.get("port").and_then(Value::as_u64).unwrap_or(0)
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Le défaut lui-même.
// ---------------------------------------------------------------------------

/// Scanner peuplé, registre manuel VIDE : le pair découvert doit figurer dans
/// la réponse. C'est le cas exact du ticket, et celui qui tombe si l'on revient
/// à `load_peers` seul.
#[tokio::test]
async fn un_pair_seulement_decouvert_figure_dans_la_liste() {
    let state = new_state();

    let corps =
        tune_server::routes::system::peers_payload(&state, &[decouvert("192.0.2.10", 8888)]).await;

    assert_eq!(
        adresses(&corps),
        vec!["192.0.2.10:8888"],
        "un serveur Tune vu par mDNS doit apparaître dans /system/peers sans \
         qu'on ait eu à l'ajouter à la main. Corps rendu : {corps}"
    );
}

/// Le même pair, enregistré à la main ET découvert : une seule ligne.
///
/// La clef est le COUPLE `host:port`. Une déduplication retirée rendrait deux
/// lignes identiques dans l'écran.
#[tokio::test]
async fn un_pair_manuel_et_decouvert_n_apparait_qu_une_fois() {
    let state = new_state();
    registre_manuel(&state, &[("192.0.2.10", 8888)]);

    let corps =
        tune_server::routes::system::peers_payload(&state, &[decouvert("192.0.2.10", 8888)]).await;

    assert_eq!(
        adresses(&corps),
        vec!["192.0.2.10:8888"],
        "le même serveur, connu des deux sources, doit être listé UNE fois. \
         Corps rendu : {corps}"
    );
}

/// La déduplication porte sur `host:port`, jamais sur l'hôte seul.
///
/// Deux serveurs Tune sur une même machine — un conteneur à côté du serveur de
/// salon — sont DEUX serveurs. Regrouper par IP les écraserait l'un l'autre :
/// c'est la faute relevée sur le #2942, où dix appareils devenaient un.
#[tokio::test]
async fn deux_serveurs_sur_le_meme_hote_restent_deux() {
    let state = new_state();
    registre_manuel(&state, &[("192.0.2.10", 8888)]);

    let corps =
        tune_server::routes::system::peers_payload(&state, &[decouvert("192.0.2.10", 9999)]).await;

    assert_eq!(
        adresses(&corps),
        vec!["192.0.2.10:8888", "192.0.2.10:9999"],
        "deux ports sur un même hôte sont deux serveurs distincts, pas un \
         doublon. Corps rendu : {corps}"
    );
}

// ---------------------------------------------------------------------------
// Le TÉMOIN — par le vrai routeur, et par lui seul.
// ---------------------------------------------------------------------------

/// Registre manuel seul, découverte vide : le comportement d'avant le #2746,
/// inchangé, mesuré à travers le routeur complet.
///
/// Ce témoin garde ce que le client consomme vraiment, et qu'aucune correction
/// n'a le droit de déplacer : `tune-web-client` affecte la réponse
/// DIRECTEMENT à `TunePeer[]` (`src/lib/api.ts`, `getTunePeers`), puis lit
/// `.length` et l'itère (`SettingsView.svelte`). Un tableau enveloppé dans
/// `{items, total, discovery}` n'est pas itérable : l'écran se viderait, et il
/// se viderait sans erreur visible.
///
/// Il reste VERT sous les deux sabotages de la fusion — c'est sa fonction :
/// prouver qu'ils mordent la découverte, et rien d'autre.
#[tokio::test]
async fn un_pair_manuel_reste_liste_et_la_reponse_reste_un_tableau_nu() {
    let state = new_state();
    registre_manuel(&state, &[("192.0.2.11", 8888)]);

    let app: Router = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(
            Request::get("/api/v1/system/peers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);

    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap();

    assert!(
        corps.is_array(),
        "le client affecte cette réponse à `TunePeer[]` et l'itère : une \
         enveloppe viderait l'écran. Corps rendu : {corps}"
    );
    assert_eq!(adresses(&corps), vec!["192.0.2.11:8888"]);

    let pair = &corps[0];
    for champ in [
        "name", "host", "port", "version", "tracks", "zones", "online",
    ] {
        assert!(
            pair.get(champ).is_some(),
            "`{champ}` est lu par le panneau Réseau ; le retirer casserait \
             l'affichage. Fiche rendue : {pair}"
        );
    }
    assert_eq!(
        pair.get("online"),
        Some(&json!(false)),
        "un pair injoignable est annoncé hors ligne, pas inventé joignable"
    );
    assert_eq!(
        pair.get("tracks"),
        Some(&json!(0)),
        "les compteurs d'un pair injoignable sont à zéro — ils ne sont jamais \
         devinés (#2722)"
    );
}

// ---------------------------------------------------------------------------
// La couture doit rester APPELÉE.
// ---------------------------------------------------------------------------

/// `peers_payload` peut être parfaitement juste et n'être appelée par
/// personne : c'est exactement l'état dans lequel `discovered_tune_peers()` a
/// passé un mois avant ce ticket.
///
/// Les essais ci-dessus pilotent `peers_payload` directement, faute de pouvoir
/// peupler le scanner mDNS ; ils ne verraient donc PAS un `system_peers`
/// redevenu autonome. Cette garde-là le voit. Elle est coupée à `#[cfg(test)]`
/// dans le manifeste au même titre que les autres cibles, et lit la source par
/// `include_str!` comme le fait `import.rs`.
#[test]
fn la_route_du_panneau_reste_branchee_sur_la_decouverte() {
    const ADMIN: &str = "tune-server/src/routes/system/admin.rs";
    let source = include_str!("../src/routes/system/admin.rs");

    let (_, apres) = source
        .split_once("pub(super) async fn system_peers")
        .unwrap_or_else(|| panic!("`system_peers` a disparu de {ADMIN}"));
    let corps = apres.split_once("\n}").map(|(c, _)| c).unwrap_or(apres);

    assert!(
        corps.contains("discovered_tune_peers"),
        "`system_peers` ({ADMIN}, corps de la route `GET /system/peers`) \
         n'appelle plus `discovered_tune_peers()`.\n\
         \n\
         C'est le défaut du #2746 mot pour mot : la découverte mDNS existe, \
         elle est juste, et la route du panneau ne la lit pas. L'écran \
         réaffiche « aucun serveur Tune détecté » sur un réseau où le \
         multicast passe.\n\
         \n\
         Corps lu :\n{corps}"
    );
    assert!(
        corps.contains("peers_payload"),
        "`system_peers` ({ADMIN}) n'appelle plus `peers_payload`.\n\
         \n\
         Les essais de ce fichier mesurent `peers_payload` ; une route qui \
         refait la fusion dans son coin les rendrait verts contre rien.\n\
         \n\
         Corps lu :\n{corps}"
    );
}
