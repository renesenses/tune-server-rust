use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use tune_core::db::backend::DbBackend;
use tune_core::outputs::OutputRegistry;
use tune_core::outputs::oh_events::OpenHomeEventListener;

use tune_core::event_bus::EventBus;
use tune_core::event_types::EventType;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Resolve a UPnP service `controlURL` (from the device description) into an
/// absolute URL usable by the SOAP client.
///
/// The controlURL may be **relative** (`/MediaRenderer/AVTransport/Control`, the
/// common case) or already **absolute** (`http://host:port/...`). Frontier Silicon
/// radios (Ruark R3, Stream 94i Plus) advertise an absolute controlURL; blindly
/// prefixing `http://host:port` yielded `http://host:PORThttp://...` — the port
/// token became `PORThttp`, the URL failed to parse, and every SOAP call died with
/// `soap send: builder error` (Yves: no sound, UI stuck on "loading title"). This
/// mirrors the MediaServer handling in `ssdp.rs`.
fn resolve_control_url(host: &str, port: u16, control_url: &str) -> String {
    if control_url.starts_with("http://") || control_url.starts_with("https://") {
        control_url.to_string()
    } else {
        let sep = if control_url.starts_with('/') {
            ""
        } else {
            "/"
        };
        format!("http://{host}:{port}{sep}{control_url}")
    }
}

/// Register a discovered output and notify controllers as one operation. Zone
/// creation is deliberately independent: hidden devices and devices for which
/// automatic zone creation is disabled must still appear in Settings > Network.
fn register_discovered_output(
    registry: &mut OutputRegistry,
    output: Box<dyn tune_core::outputs::OutputTarget>,
    event_bus: &EventBus,
    dev: &tune_core::discovery::device::DiscoveredDevice,
    device_type: &str,
) {
    registry.register(output);
    event_bus.emit_typed(
        EventType::DeviceDiscovered,
        serde_json::json!({
            "device_id": &dev.id,
            "name": &dev.name,
            "device_type": device_type,
            "host": &dev.host,
        }),
    );
}

/// Set a zone's online state and, if it actually changed, broadcast a
/// `zone.updated` event so controllers see availability flip in real time.
/// (`set_online_by_device` alone is silent — clients never learned of it.)
fn set_zone_online(event_bus: &EventBus, db: &Arc<dyn DbBackend>, device_id: &str, online: bool) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
    let prev = zone_repo
        .get_by_device_id(device_id)
        .ok()
        .flatten()
        .map(|z| z.online);
    let _ = zone_repo.set_online_by_device(device_id, online);
    if prev != Some(online) {
        event_bus.emit_typed(
            EventType::ZoneUpdated,
            serde_json::json!({ "device_id": device_id, "online": online }),
        );
    }
}

// ---------------------------------------------------------------------------
// Known-renderer persistence (#1126)
// ---------------------------------------------------------------------------

/// Settings key holding the JSON array of renderers seen at least once via SSDP.
const KNOWN_RENDERERS_KEY: &str = "known_renderers";

/// A DLNA/OpenHome renderer we discovered via SSDP, persisted so it can be
/// re-probed directly over HTTP at startup (#1126).
///
/// Some renderers (Cyrus Stream X2) don't answer M-SEARCH when idle and rarely
/// emit `ssdp:alive`, so after a server restart normal SSDP rediscovery never
/// fires and the device's zone stays `online:false` forever — rejecting all
/// playback — even though the device is reachable (ping + description.xml over
/// HTTP work). Persisting its LOCATION/UUID lets [`reregister_known_renderers`]
/// HTTP-probe it and re-register through the same path SSDP uses, so the
/// EXISTING zone (keyed on the uuid-based device_id) reconnects instead of a
/// duplicate being created. Mirrors the manual-device store in
/// `routes::devices`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct KnownRenderer {
    device_id: String,
    location: String,
    name: String,
}

fn load_known_renderers(db: &Arc<dyn DbBackend>) -> Vec<KnownRenderer> {
    let repo = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone());
    match repo.get(KNOWN_RENDERERS_KEY) {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn save_known_renderers(db: &Arc<dyn DbBackend>, renderers: &[KnownRenderer]) {
    let repo = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone());
    match serde_json::to_string(renderers) {
        Ok(json) => {
            if let Err(e) = repo.set(KNOWN_RENDERERS_KEY, &json) {
                warn!(error = %e, "known_renderers_persist_failed");
            }
        }
        Err(e) => warn!(error = %e, "known_renderers_serialize_failed"),
    }
}

/// Replie le magasin sur UNE entrée par `LOCATION`, en gardant la première.
///
/// UPnP garantit qu'une `LOCATION` renvoie une seule description racine, donc
/// un seul appareil physique : deux entrées qui la partagent sont le même
/// matériel vu par deux de ses UDN. Fonction pure, testable sans réseau
/// (#1703).
fn dedup_renderers_by_location(renderers: Vec<KnownRenderer>) -> Vec<KnownRenderer> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    renderers
        .into_iter()
        .filter(|r| seen.insert(r.location.clone()))
        .collect()
}

/// Upsert a discovered renderer by device_id (replacing any prior entry).
/// Best-effort: a persistence failure is logged, never panics, and never
/// blocks discovery. Skips the settings write when the stored entry is already
/// identical, so the periodic SSDP re-discovery doesn't churn the DB.
fn persist_known_renderer(db: &Arc<dyn DbBackend>, device_id: &str, location: &str, name: &str) {
    let mut renderers = load_known_renderers(db);
    if renderers
        .iter()
        .any(|r| r.device_id == device_id && r.location == location && r.name == name)
    {
        return;
    }
    // Une `LOCATION` = un appareil physique (UPnP) : un HEOS Denon/Marantz
    // annonce racine et appareils embarqués sous des `uuid:` différents mais à
    // la même URL de description. Sans le `r.location != location`, le magasin
    // gagnait une entrée par UDN et le démarrage re-sondait cinq fois le même
    // ND8006 (#1703).
    renderers.retain(|r| r.device_id != device_id && r.location != location);
    renderers.push(KnownRenderer {
        device_id: device_id.to_string(),
        location: location.to_string(),
        name: name.to_string(),
    });
    save_known_renderers(db, &renderers);
}

/// Spawn the SSDP handler that registers DLNA/OpenHome outputs and auto-creates zones.
pub fn spawn_ssdp_handler(
    state: &AppState,
    config: &TuneConfig,
    oh_listener: Option<Arc<OpenHomeEventListener>>,
) {
    let (ssdp_tx, mut ssdp_rx) = tokio::sync::mpsc::channel(64);
    {
        let scanner = state.scanner.clone();
        tokio::spawn(async move {
            // On injecte le canal au scanner partagé au lieu de le remplacer :
            // remplacer l'instance imposait un mutex englobant, tenu ensuite à
            // travers chaque balayage réseau (#1432).
            scanner.set_event_tx(ssdp_tx).await;
            scanner.start().await;
        });
    }

    let outputs = state.outputs.clone();
    let db = state.backend.clone();
    let config = config.clone();
    let event_bus = state.event_bus.clone();
    let media_servers = state.media_servers.clone();
    let playback = state.playback.clone();
    let license = state.license.clone();
    tokio::spawn(async move {
        use tune_core::discovery::ssdp::SsdpEvent;
        let mut seen_hosts: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(event) = ssdp_rx.recv().await {
            match event {
                SsdpEvent::DeviceDiscovered(dev) => {
                    handle_ssdp_discovered(
                        &dev,
                        &outputs,
                        &db,
                        &config,
                        &event_bus,
                        &oh_listener,
                        &playback,
                        &license,
                        &mut seen_hosts,
                    )
                    .await;
                }
                SsdpEvent::DeviceLost(id) => {
                    let mut reg = outputs.lock().await;
                    // DLNA/OpenHome tolerance: a Samsung TV (and similar SOAP
                    // renderers) stops advertising on SSDP when it goes idle, yet
                    // still answers an AVTransport command if woken. Keep its
                    // output in the registry so a play attempt can wake it — the
                    // offline gate already allows an offline-but-registered zone
                    // (Bilou's "erreur 503 en DLNA"). Registration is idempotent,
                    // so a later re-advertise overwrites it with fresh URLs. The
                    // zone is still flagged offline for the UI. Non-SOAP outputs
                    // (chromecast, …) are dropped as before.
                    let soap_wakeable =
                        matches!(reg.type_of(&id).as_deref(), Some("dlna") | Some("openhome"));
                    if !soap_wakeable {
                        reg.remove(&id);
                    }
                    set_zone_online(&event_bus, &db, &id, false);
                    event_bus.emit_typed(
                        EventType::DeviceLost,
                        serde_json::json!({ "device_id": id }),
                    );
                    info!(id = %id, kept_registered = soap_wakeable, "device_lost_zone_offline");
                }
                SsdpEvent::MediaServerDiscovered(ms) => {
                    let id = ms.id.clone();
                    media_servers.lock().await.insert(id.clone(), ms);
                    info!(id = %id, "media_server_registered");
                }
            }
        }
    });
}

/// Charge utile de `zone.created`, dans la forme que le client attend.
///
/// La route API emet `{ "id", "zone": <la zone entiere> }` et le client teste
/// explicitement `data.zone` avant de fusionner la zone dans son magasin. Les
/// trois emetteurs de la decouverte publiaient a plat — `zone_id`, `name`,
/// `device_id`, `type` — donc sans cle `zone` : la condition etait fausse,
/// l'evenement ignore en silence, et une zone decouverte n'apparaissait qu'au
/// rechargement de la page (#2224). Une zone creee a la main, elle, apparaissait
/// tout de suite : deux formes pour un meme evenement.
///
/// On AJOUTE `id` et `zone` sans retirer les champs plats. D'autres
/// consommateurs lisent cet evenement — plugins abonnes, passerelle
/// `developer_api` — et n'ont pas a etre migres pour que l'interface se repare.
/// Un evenement qui satisfait les deux formes ne casse personne.
fn charge_utile_zone_creee(
    zone_repo: &tune_core::db::zone_repo::ZoneRepo,
    zone_id: i64,
    mut plat: serde_json::Value,
) -> serde_json::Value {
    // Lu avant l'emprunt mutable : sert de repli si la zone a disparu entre sa
    // creation et cette relecture.
    let nom = plat
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if let Some(obj) = plat.as_object_mut() {
        obj.insert("id".into(), serde_json::json!(zone_id));
        // Le CONTRAT client, pas la ligne de base : le volume y passe de 0..100
        // a 0..1, et l'etat de lecture est pose plutot qu'omis. Un
        // `to_value(&zone)` brut faisait repartir le volume a 50 la ou le
        // client attend 0.5 (JP Robbe, revue de #2229).
        //
        // Si la relecture echoue, on emet la forme plate seule plutot que rien :
        // l'ancien comportement, jamais pire.
        if let Ok(Some(zone)) = zone_repo.get(zone_id) {
            obj.insert(
                "zone".into(),
                tune_core::db::zone_repo::zone_creee_contrat_client(Some(&zone), zone_id, &nom),
            );
        }
    }
    plat
}

/// Reconnaître une annonce SSDP émise par CE serveur.
///
/// Tune publie chaque zone qui l'a demandé comme un MediaRenderer UPnP, sous
/// le nom `« {zone} (Tune) »` (`routes/upnp_media_renderer.rs`). Ces
/// annonces repartent sur le même multicast que celles des appareils du
/// réseau — et rien ne les distinguait à la réception. Tune se découvrait
/// donc lui-même : la zone « ND8006 » réapparaissait comme un appareil
/// « ND8006 (Tune) », proposé comme sortie, enregistré, et persisté parmi les
/// renderers connus.
///
/// Deux testeurs l'ont signalé sans qu'on fasse le lien — Jean Valjean
/// (« j'ai une zone ND8006(Tune), est-ce normal ? ») et Marco Polo
/// (« pourquoi les suffixes Tune, Tune… ? »). Et au démarrage suivant, le
/// magasin ainsi pollué faisait sonder nos propres adresses : trois
/// `known_renderer_probe_failed` sur `192.168.1.10:8888/upnp/renderer/…` dans
/// ses journaux, pour des zones qui n'existaient plus.
///
/// Le test porte sur les **trois** à la fois — chemin de montage, port d'API,
/// et adresse locale. Le chemin et le port seuls écarteraient aussi les zones
/// d'un AUTRE serveur Tune du réseau, qui sont, elles, parfaitement
/// pilotables : c'est nous qu'il faut exclure, pas nos semblables.
fn est_notre_propre_renderer(
    location: &str,
    port_annonce: u16,
    port_api: u16,
    nos_adresses: &[String],
) -> bool {
    if port_annonce != port_api {
        return false;
    }
    if !location.contains(tune_core::upnp_renderer::RENDERER_MOUNT) {
        return false;
    }
    let hote = location
        .split("://")
        .nth(1)
        .and_then(|reste| reste.split('/').next())
        .map(|hp| hp.split(':').next().unwrap_or(hp))
        .unwrap_or_default();
    !hote.is_empty() && nos_adresses.iter().any(|a| a == hote)
}

/// Les UDN de nos propres façades (`upnp_renderer_udn_<zone>`), tels que
/// `renderer_udn` les persiste à la première annonce. Contrairement aux
/// adresses, ils ne dépendent d'aucune énumération d'interfaces.
fn nos_udn_de_facade(db: &Arc<dyn DbBackend>) -> Vec<String> {
    tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .all()
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| k.starts_with("upnp_renderer_udn_"))
        .map(|(_, v)| v)
        .collect()
}

fn est_un_de_nos_udn_de_facade(db: &Arc<dyn DbBackend>, device_id: &str) -> bool {
    !device_id.is_empty() && nos_udn_de_facade(db).iter().any(|u| u == device_id)
}

/// Nos adresses, du point de vue d'une annonce reçue : l'IP du réseau local et
/// les formes locales, qu'un M-SEARCH émis depuis la machine elle-même peut
/// nous renvoyer.
fn nos_adresses() -> Vec<String> {
    nos_adresses_depuis(
        &tune_core::discovery::ssdp::local_ipv4_addresses(),
        tune_core::discovery::ssdp::get_local_ip(),
    )
}

/// L'assemblage seul, sans I/O — c'est lui que le test couvre.
///
/// `elue` est l'adresse que `get_local_ip()` retiendrait pour s'ANNONCER. Elle
/// ne suffit pas à se RECONNAÎTRE : une annonce SSDP porte l'adresse de
/// l'interface qui l'a émise, et sur une machine à plusieurs interfaces — Wi-Fi
/// et Ethernet, pont Docker, tunnel VPN — ce n'est pas la même. On prend donc
/// toutes les interfaces, et on garde l'élue en ceinture et bretelles pour le
/// cas où l'énumération échoue (conteneur sans droits sur les interfaces).
fn nos_adresses_depuis(
    interfaces: &[std::net::Ipv4Addr],
    elue: Option<std::net::Ipv4Addr>,
) -> Vec<String> {
    let mut v = vec!["127.0.0.1".to_string(), "localhost".to_string()];
    for ip in interfaces.iter().copied().chain(elue) {
        let s = ip.to_string();
        if !v.contains(&s) {
            v.push(s);
        }
    }
    v
}

async fn handle_ssdp_discovered(
    dev: &tune_core::discovery::device::DiscoveredDevice,
    outputs: &Arc<tokio::sync::Mutex<OutputRegistry>>,
    db: &Arc<dyn DbBackend>,
    config: &TuneConfig,
    event_bus: &Arc<tune_core::event_bus::EventBus>,
    oh_listener: &Option<Arc<OpenHomeEventListener>>,
    playback: &Arc<tune_core::playback::PlaybackManager>,
    _license: &Arc<tune_core::license::LicenseManager>,
    seen_hosts: &mut std::collections::HashSet<String>,
) {
    let is_renderer = dev.device_type == tune_core::discovery::device::OutputType::Dlna
        || dev.device_type == tune_core::discovery::device::OutputType::Openhome;
    if !is_renderer {
        return;
    }

    // Nos propres zones publiées : on ne se découvre pas soi-même.
    if let Some(loc) = dev.location.as_deref()
        && est_notre_propre_renderer(loc, dev.port, config.port, &nos_adresses())
    {
        debug!(
            id = %dev.id,
            name = %dev.name,
            location = %loc,
            "ssdp_notre_propre_renderer_ignore"
        );
        return;
    }

    // Deuxième rideau : l'UDN. L'exclusion par adresse ci-dessus echoue des
    // que l'annonce revient par un chemin que `nos_adresses()` n'enumere pas
    // (interface manquante, nom d'hote dans LOCATION, vieux build) — c'est la
    // greffe du 25/08 : .18 a enregistre sa propre facade de la zone 10 comme
    // zone « Eversolo DMP-A8 (Tune) », UDN identique a `upnp_renderer_udn_10`.
    // Un UDN de facade est tire au sort par NOUS et persiste : s'il revient
    // par SSDP, c'est forcement notre reflet.
    if est_un_de_nos_udn_de_facade(db, &dev.id) {
        warn!(
            id = %dev.id,
            name = %dev.name,
            "ssdp_notre_facade_reconnue_par_udn_ignoree"
        );
        return;
    }

    let svc_urls = dev
        .capabilities
        .get("service_urls")
        .and_then(|v| {
            serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
        })
        .unwrap_or_default();

    // Whether we actually registered an output for this renderer below — only
    // then is it worth persisting for restart recovery (#1126).
    let mut registered = false;

    if dev.device_type == tune_core::discovery::device::OutputType::Openhome {
        let evt_urls = dev
            .capabilities
            .get("event_sub_urls")
            .and_then(|v| {
                serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
            })
            .unwrap_or_default();
        let oh = tune_core::outputs::openhome::OpenHomeOutput::new(
            dev.name.clone(),
            dev.id.clone(),
            dev.host.clone(),
            dev.port,
            svc_urls.clone(),
            oh_listener.clone(),
            evt_urls,
        );
        let mut reg = outputs.lock().await;
        register_discovered_output(&mut reg, Box::new(oh), event_bus, dev, "openhome");
        registered = true;
        info!(name = %dev.name, id = %dev.id, "openhome_output_registered");
    } else {
        // Resolve each controlURL to an absolute URL (see `resolve_control_url`):
        // relative paths are joined onto host:port, absolute URLs kept as-is —
        // otherwise Frontier Silicon radios (Ruark, Stream 94i) fail with
        // `soap send: builder error` and play nothing.
        let av_url = svc_urls
            .get("avtransport")
            .map(|p| resolve_control_url(&dev.host, dev.port, p));
        let rc_url = svc_urls
            .get("renderingcontrol")
            .map(|p| resolve_control_url(&dev.host, dev.port, p));
        let cm_url = svc_urls
            .get("connectionmanager")
            .or_else(|| svc_urls.get("ConnectionManager"))
            .map(|p| resolve_control_url(&dev.host, dev.port, p));
        if let (Some(av), Some(rc)) = (av_url, rc_url) {
            // Garde-fou : un appareil PHYSIQUE dont l'URL de contrôle pointe
            // vers la façade UPnP d'un serveur Tune (`/upnp/renderer/`) est un
            // enregistrement croisé — les commandes de transport partiraient
            // dans un miroir qui acquitte tout sans rien faire (saga DMP-A8,
            // 25/08 : Stop/SetURI/Play « acquittés » par personne). On
            // journalise fort ; le comportement ne change pas encore, le
            // mécanisme exact de la greffe restant à établir.
            if av.contains("/upnp/renderer/") && !dev.id.contains("-tune-") {
                warn!(
                    name = %dev.name,
                    id = %dev.id,
                    ctrl = %av,
                    "dlna_output_ctrl_vers_facade_tune — enregistrement suspect"
                );
            }
            let delay = crate::config::resolve_play_delay(db, config, &dev.id, &dev.name);
            let dlna = tune_core::outputs::dlna::DlnaOutput::new(
                dev.name.clone(),
                dev.id.clone(),
                dev.host.clone(),
                av,
                rc,
                cm_url,
            )
            .with_play_delay(delay);
            let mut reg = outputs.lock().await;
            register_discovered_output(&mut reg, Box::new(dlna), event_bus, dev, "dlna");
            registered = true;
            info!(name = %dev.name, id = %dev.id, "dlna_output_registered");
            drop(reg);
            // Persist LOCATION + UUID so a lazy-SSDP renderer (Cyrus Stream X2)
            // can be re-probed over HTTP after a restart instead of vanishing
            // until it next answers multicast (#1126).
            if let Some(ref loc) = dev.location {
                crate::routes::devices::persist_discovered_dlna(
                    db, &dev.id, loc, &dev.name, &dev.host, dev.port,
                );
            }
        }
    }

    // Persist this renderer so it can be re-probed directly at the next startup,
    // even if it never answers SSDP M-SEARCH again (#1126). Best-effort.
    if registered && let Some(location) = dev.location.as_deref() {
        persist_known_renderer(db, &dev.id, location, &dev.name);
    }

    let skip_keywords = [
        "tv",
        "décodeur",
        "decoder",
        "kdl-",
        "bravia",
        "samsung",
        "lg ",
        "philips tv",
        "chromecast",
    ];
    let name_lower = dev.name.to_lowercase();
    let is_tv = skip_keywords.iter().any(|kw| name_lower.contains(kw));

    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
    if zone_repo.is_device_hidden(&dev.id) {
        tracing::debug!(name = %dev.name, id = %dev.id, "ssdp_zone_hidden_skipping");
        return;
    }
    if let Ok(Some(zone)) = zone_repo.get_by_device_id(&dev.id) {
        seen_hosts.insert(dev.host.clone());
        set_zone_online(event_bus, db, &dev.id, true);
        if let Some(zone_id) = zone.id {
            let vol = zone.volume as f64 / 100.0;
            playback.set_volume(zone_id, vol).await;
            // Backfill the host on zones created before host-based dedup existed,
            // so a later UUID change (Denon restart) can reconnect by host (#942).
            let _ = zone_repo.set_host(zone_id, &dev.host);
        }
        info!(name = %dev.name, id = %dev.id, "zone_device_reconnected");
        event_bus.emit(
            "device.reconnected",
            serde_json::json!({
                "device_id": &dev.id,
                "name": &dev.name,
                "host": &dev.host,
            }),
        );
    } else if let Some(existing_zone_id) = zone_repo.zone_id_by_host(&dev.host) {
        // Same physical renderer, NEW UPnP UUID (e.g. Denon Ceol N12 after a
        // restart, which changes its UUIDs): re-point the existing zone to the
        // live device_id instead of spawning a duplicate. This keeps the zone's
        // per-zone settings — crucially the "native FLAC" toggle and volume —
        // on the one zone the user actually plays (forum #942: duplicate Denon
        // zones meant the toggle was set on a zone that was never the one
        // playing, so Tidal FLAC kept being transcoded to WAV).
        seen_hosts.insert(dev.host.clone());
        let _ = zone_repo.update_device_id(existing_zone_id, &dev.id);
        let _ = zone_repo.set_host(existing_zone_id, &dev.host);
        set_zone_online(event_bus, db, &dev.id, true);
        let vol = zone_repo
            .get(existing_zone_id)
            .ok()
            .flatten()
            .map(|z| z.volume as f64 / 100.0);
        if let Some(vol) = vol {
            playback.set_volume(existing_zone_id, vol).await;
        }
        info!(
            name = %dev.name,
            id = %dev.id,
            host = %dev.host,
            zone_id = existing_zone_id,
            "zone_device_reconnected_by_host"
        );
        event_bus.emit(
            "device.reconnected",
            serde_json::json!({
                "device_id": &dev.id,
                "name": &dev.name,
                "host": &dev.host,
            }),
        );
    } else if !is_tv {
        // Check zone_auto_create setting
        let auto_create = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get("zone_auto_create")
            .ok()
            .flatten()
            .map(|v| v != "false")
            .unwrap_or(true);
        if !auto_create {
            info!(name = %dev.name, id = %dev.id, "ssdp_zone_auto_create_disabled_skipping");
            return;
        }

        // Auto-created zones start dormant; the free-tier cap is enforced at
        // first play (orchestrator.play), so discovery always registers.

        // Dedup by host: skip if we already created a zone for this IP
        // (e.g. Denon AVR exposes 5 UPnP services with different UUIDs
        // but the same host — only the first one should create a zone)
        if !seen_hosts.insert(dev.host.clone()) {
            tracing::debug!(name = %dev.name, host = %dev.host, id = %dev.id, "ssdp_zone_host_already_seen_skipping");
            return;
        }

        let short_name = dev.name.split(" - ").next().unwrap_or(&dev.name);
        let existing_zones = zone_repo.list().unwrap_or_default();
        let name_taken = existing_zones.iter().any(|z| z.name == short_name);
        let zone_name = if name_taken {
            dev.name.clone()
        } else {
            short_name.to_string()
        };

        // Persisted name dedup: a device exposing several UPnP services (a Sonos
        // is both a DLNA MediaRenderer and OpenHome) can be rediscovered under a
        // NEW device_id after a restart. get_by_device_id above misses it and
        // the per-pass seen_hosts is empty, so a duplicate zone with the SAME
        // disambiguated name (which includes the renderer UUID) gets created
        // (Bertrand: "Chambre - Sonos … RINCON…" ×2). Same full name = same
        // physical device → skip the duplicate; its live device_id reconnects
        // via the get_by_device_id path.
        if existing_zones.iter().any(|z| z.name == zone_name) {
            tracing::debug!(name = %zone_name, id = %dev.id, host = %dev.host, "ssdp_zone_name_exists_skipping_duplicate");
            return;
        }

        let type_str = if dev.device_type == tune_core::discovery::device::OutputType::Openhome {
            "openhome"
        } else {
            "dlna"
        };

        // Cross-protocol duplicate guard (Phase B, #1239) — the SSDP path
        // never had one: a Node already owning a BluOS zone (created by the
        // mDNS handler) would still get DLNA/OpenHome zones here whenever
        // the names differed. No live registry on this path; the persisted
        // host/MAC identity does the matching.
        if let Some(conflict) = physical_zone_conflict(
            &zone_repo,
            &existing_zones,
            |_| None,
            &dev.name,
            &dev.host,
            dev.mac_address.as_deref(),
            type_str,
        ) {
            info!(
                name = %dev.name,
                id = %dev.id,
                host = %dev.host,
                r#type = type_str,
                conflicting_zone = %conflict,
                "ssdp_zone_skipped_conflicting_protocol"
            );
            return;
        }

        match zone_repo.get_or_create(&zone_name, Some(type_str), &dev.id) {
            Ok((zid, true)) => {
                // Persist host + MAC so a later UUID change, protocol change
                // or DHCP renumbering reconnects here (#942, #1239).
                let _ = zone_repo.set_identity(zid, &dev.host, dev.mac_address.as_deref());
                event_bus.emit_typed(
                    EventType::ZoneCreated,
                    charge_utile_zone_creee(
                        &zone_repo,
                        zid,
                        serde_json::json!({
                            "zone_id": zid,
                            "name": zone_name,
                            "device_id": dev.id,
                            "type": type_str,
                        }),
                    ),
                );
                info!(name = %zone_name, zone_id = zid, device = %dev.id, r#type = type_str, "ssdp_zone_auto_created");
            }
            Ok((zid, false)) => {
                let _ = zone_repo.set_identity(zid, &dev.host, dev.mac_address.as_deref());
                set_zone_online(event_bus, db, &dev.id, true);
                info!(name = %zone_name, zone_id = zid, device = %dev.id, "ssdp_zone_already_existed");
            }
            Err(e) => {
                tracing::warn!(name = %zone_name, device = %dev.id, error = %e, "ssdp_zone_create_failed");
            }
        }
    }
}

/// Re-probe every persisted renderer at startup and re-register the reachable
/// ones (#1126).
///
/// Renderers with a lazy SSDP responder (Cyrus Stream X2) never resurface
/// through the multicast scan after a restart, so their zone would stay offline
/// and reject all playback. For each stored renderer we HTTP-probe its LOCATION
/// (via [`tune_core::discovery::ssdp::probe_renderer`]) and, only if the
/// descriptor's UUID still matches, feed it through the SAME
/// [`handle_ssdp_discovered`] path a live SSDP discovery uses — so the existing
/// zone (keyed on the uuid-based device_id) reconnects instead of a duplicate
/// being created. If the UUID differs a different device now lives at that URL,
/// so we skip it and let live discovery handle the newcomer. Best-effort per
/// device; failures are logged and never block boot. Mirrors
/// `routes::devices::reregister_manual_devices`.
pub async fn reregister_known_renderers(state: &AppState) {
    let stored = load_known_renderers(&state.backend);
    if stored.is_empty() {
        return;
    }
    // Les magasins écrits avant #1703 portent une entrée par UDN — cinq pour
    // un seul Marantz ND8006, toutes à la même URL de description. On les
    // replie avant de sonder, et on réécrit le magasin pour qu'il guérisse.
    // Purge des entrées que nous nous étions ajoutées à nous-mêmes avant que
    // l'auto-découverte ne soit filtrée : sans elle, un magasin déjà pollué
    // continuerait à sonder nos propres adresses à chaque démarrage, pour des
    // zones souvent supprimées depuis. Le magasin se soigne, comme il le fait
    // déjà pour les doublons par UDN.
    let a_nous = nos_adresses();
    let port_api = state.config.port;
    let stored_len = stored.len();
    let stored: Vec<KnownRenderer> = stored
        .into_iter()
        .filter(|kr| !est_notre_propre_renderer(&kr.location, port_api, port_api, &a_nous))
        .collect();
    if stored.len() != stored_len {
        info!(
            retires = stored_len - stored.len(),
            "known_renderers_purge_de_nos_propres_zones"
        );
    }
    if stored.is_empty() {
        save_known_renderers(&state.backend, &stored);
        return;
    }
    let stored_len = stored.len();
    let renderers = dedup_renderers_by_location(stored);
    if renderers.len() != stored_len {
        info!(
            before = stored_len,
            after = renderers.len(),
            "known_renderers_collapsed_by_location"
        );
        save_known_renderers(&state.backend, &renderers);
    }
    info!(count = renderers.len(), "reregistering_known_renderers");

    // No OpenHome event listener here: the live SSDP handler's listener isn't
    // reachable from AppState, and creating a second one would race it for the
    // fixed :8890 bind at boot (`OpenHomeEventListener::new` falls back to an
    // ephemeral port when 8890 is taken) — a subtle regression on the primary
    // listener. DLNA renderers (the #1126 case, e.g. Cyrus Stream X2) ignore the
    // listener entirely; an OpenHome renderer re-attached here gets its push
    // events back on its next SSDP advertise (which re-registers it with the real
    // listener) and polls its state until then.
    let oh_listener: Option<Arc<OpenHomeEventListener>> = None;

    let mut seen_hosts: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut recovered = 0usize;
    for kr in renderers {
        match tune_core::discovery::ssdp::probe_renderer(&kr.device_id, &kr.location).await {
            Some(dev) if dev.id == kr.device_id => {
                handle_ssdp_discovered(
                    &dev,
                    &state.outputs,
                    &state.backend,
                    &state.config,
                    &state.event_bus,
                    &oh_listener,
                    &state.playback,
                    &state.license,
                    &mut seen_hosts,
                )
                .await;
                recovered += 1;
                info!(
                    id = %kr.device_id,
                    name = %kr.name,
                    location = %kr.location,
                    "known_renderer_reregistered"
                );
            }
            Some(dev) => {
                warn!(
                    stored_id = %kr.device_id,
                    live_id = %dev.id,
                    location = %kr.location,
                    "known_renderer_uuid_changed_skipping"
                );
            }
            None => {
                warn!(
                    id = %kr.device_id,
                    location = %kr.location,
                    "known_renderer_probe_failed"
                );
            }
        }
    }
    info!(recovered, "known_renderers_reregister_complete");
}

/// Spawn the mDNS handler that registers Chromecast/AirPlay/BluOS/OAAT/Squeezebox outputs.
///
/// Returns the `MdnsScanner` handle (must be kept alive for the scanner to keep running).
pub fn spawn_mdns_handler(
    state: &AppState,
) -> Option<std::sync::Arc<tune_core::discovery::mdns::MdnsScanner>> {
    let (mdns_tx, mut mdns_rx) = tokio::sync::mpsc::channel(64);
    let handle = if let Ok(mdns) = tune_core::discovery::mdns::MdnsScanner::new(mdns_tx) {
        let mut mdns = mdns
            .with_chromecast()
            .with_airplay()
            .with_bluos()
            .with_oaat()
            .with_squeezebox()
            // Browse peer Tune servers too, so this server can list the other
            // Tune servers on the network (#1273). Each server already announces
            // itself via `register_self`; without this it never browsed back.
            .with_tune_peers();
        if let Err(e) = mdns.start() {
            tracing::warn!(error = %e, "mdns_start_failed");
        }
        let port = std::env::var("TUNE_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8888u16);
        if let Err(e) = mdns.register_self(port, tune_core::version()) {
            tracing::warn!(error = %e, "mdns_register_self_failed");
        }
        // Publish the scanner so routes (`/peers`, `/system/discover-servers`)
        // can list the discovered peers. AppState keeps it alive for the whole
        // process, so the returned handle is a convenience clone only.
        let mdns = std::sync::Arc::new(mdns);
        *state.mdns_scanner.lock().unwrap() = Some(mdns.clone());
        Some(mdns)
    } else {
        None
    };

    let outputs = state.outputs.clone();
    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    let playback = state.playback.clone();
    tokio::spawn(async move {
        use tune_core::discovery::device::OutputType;
        use tune_core::discovery::mdns::MdnsEvent;
        while let Some(event) = mdns_rx.recv().await {
            match event {
                MdnsEvent::DeviceDiscovered(dev) | MdnsEvent::DeviceUpdated(dev) => {
                    // Set when an AirPlay 2 device falls back to the legacy
                    // AirPlay output (daemon unavailable / deviceid unknown):
                    // the output is registered but no zone is auto-created.
                    let mut airplay_v2_fallback = false;
                    let (output, output_type_str): (
                        Option<Box<dyn tune_core::outputs::OutputTarget>>,
                        &str,
                    ) = match dev.device_type {
                        OutputType::Chromecast => {
                            let cast = tune_core::outputs::chromecast::ChromecastOutput::new(
                                dev.name.clone(),
                                dev.id.clone(),
                                dev.host.clone(),
                                dev.port,
                            );
                            (Some(Box::new(cast)), "chromecast")
                        }
                        OutputType::Airplay => {
                            let is_v2 = dev.airplay_version.as_deref() == Some("2");
                            // The AirPlay deviceid (a MAC) is stored on `mac_address`
                            // by the mDNS handler; `capabilities["deviceid"]` is never
                            // populated. Reading only the latter left ap_device_id empty,
                            // so the AirPlay 2 daemon rejected every connection ("MAC
                            // address must be 12 hex characters, got 0") — Matteo's Sonos
                            // Era 100 "Chambre Missou". Prefer mac_address, fall back to
                            // the capability.
                            let ap_dev_id = dev
                                .mac_address
                                .clone()
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    dev.capabilities
                                        .get("deviceid")
                                        .and_then(|v| v.as_str())
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_string)
                                })
                                .unwrap_or_default();
                            // Without a device id the AirPlay 2 daemon can't connect, so
                            // a v2 output would be a dead zone. Fall back to legacy AirPlay
                            // in that case instead of registering a broken v2 zone.
                            if is_v2
                                && tune_core::outputs::airplay2::daemon_available()
                                && !ap_dev_id.is_empty()
                            {
                                let ap2 = tune_core::outputs::airplay2::Airplay2Output::new(
                                    dev.name.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                    dev.id.clone(),
                                    ap_dev_id,
                                );
                                info!(name = %dev.name, "airplay2_output_registered");
                                (Some(Box::new(ap2)), "airplay2")
                            } else {
                                if is_v2 {
                                    // An AirPlay 2 device served by the legacy
                                    // path is a dead end: these devices demand
                                    // the pairing only the airplay2 daemon can
                                    // perform, so every ANNOUNCE gets a 403
                                    // (forum #1183, Samsung S95BA TV). Register
                                    // the output so a manually created zone can
                                    // still target it, but never auto-create a
                                    // zone for it (#788 already intended "skip
                                    // v2 zone if daemon absent").
                                    airplay_v2_fallback = true;
                                }
                                let ap = tune_core::outputs::airplay::AirplayOutput::new(
                                    dev.name.clone(),
                                    dev.id.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                );
                                (Some(Box::new(ap)), "airplay")
                            }
                        }
                        OutputType::Bluos => {
                            let bluos = tune_core::outputs::bluos::BluosOutput::new(
                                dev.name.clone(),
                                dev.id.clone(),
                                dev.host.clone(),
                                dev.port,
                            );
                            (Some(Box::new(bluos)), "bluos")
                        }
                        #[cfg(feature = "oaat")]
                        OutputType::Oaat => {
                            let oaat = tune_core::outputs::oaat::OaatOutput::new(
                                dev.name.clone(),
                                dev.host.clone(),
                                dev.port,
                                dev.id.clone(),
                            );
                            (Some(Box::new(oaat)), "oaat")
                        }
                        #[cfg(not(feature = "oaat"))]
                        OutputType::Oaat => {
                            tracing::warn!("OAAT support not compiled in");
                            (None, "oaat")
                        }
                        OutputType::Squeezebox => {
                            let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(
                                db.clone(),
                            );
                            let current = settings
                                .get("lms_host")
                                .ok()
                                .flatten()
                                .or_else(|| settings.get("squeezebox_host").ok().flatten())
                                .unwrap_or_default();
                            if current.is_empty() {
                                // Use the CLI port (9090), NOT the JSON-RPC port (9000)
                                let cli_port = dev.port;
                                let lms_addr = format!("{}:{}", dev.host, cli_port);
                                // Write to both keys: "lms_host" is what the web client reads,
                                // "squeezebox_host" is legacy
                                settings.set("lms_host", &lms_addr).ok();
                                settings.set("squeezebox_host", &lms_addr).ok();
                                settings.set("squeezebox_enabled", "true").ok();
                                info!(host = %lms_addr, "mdns_lms_discovered_auto_configured");
                            }
                            (None, "squeezebox")
                        }
                        _ => (None, ""),
                    };

                    if let Some(output) = output {
                        let mut reg = outputs.lock().await;
                        register_discovered_output(
                            &mut reg,
                            output,
                            &event_bus,
                            &dev,
                            output_type_str,
                        );
                        info!(name = %dev.name, host = %dev.host, port = dev.port, r#type = output_type_str, "mdns_output_registered");

                        let zone_repo =
                            tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
                        // Honour a user deletion: deleting a zone soft-hides it
                        // (is_hidden=1). The SSDP handler already skips hidden
                        // devices; the mDNS handler did not, so an AirPlay/
                        // Chromecast/BluOS device (e.g. Fabien's Beosound Stage)
                        // was reconnected — or, with no device_id match, re-created
                        // via auto_create — at startup, resurrecting the deleted
                        // zone even with "create zones automatically" OFF. Skip the
                        // whole reconnect/create block for hidden devices.
                        if zone_repo.is_device_hidden(&dev.id) {
                            info!(name = %dev.name, id = %dev.id, "mdns_zone_hidden_skipping");
                        } else if let Some((zid, was_hidden)) =
                            legacy_zone_to_reanchor(&zone_repo, &dev)
                        {
                            // Zone creee AVANT #1528, donc enregistree sous
                            // l'ancien identifiant derive de l'adresse. On la
                            // re-ancre sur le nouvel identifiant durable — c'est
                            // ce qui remplace la migration SQL, qui n'aurait pas
                            // pu calculer ces identifiants (ils ne sont connus
                            // qu'a la decouverte) et aurait fait perdre toutes
                            // les zones d'un coup.
                            //
                            // Une zone supprimee reste supprimee : on deplace
                            // son identifiant sans la remettre en ligne, sinon
                            // la mise a jour ressusciterait ce que
                            // l'utilisateur avait efface.
                            let _ = zone_repo.update_output_device(zid, &dev.id);
                            if !was_hidden {
                                set_zone_online(&event_bus, &db, &dev.id, true);
                            }
                            info!(
                                name = %dev.name,
                                id = %dev.id,
                                zone_id = zid,
                                hidden = was_hidden,
                                "mdns_zone_reanchored_from_legacy_id"
                            );
                        } else if let Ok(Some(zone)) = zone_repo.get_by_device_id(&dev.id) {
                            set_zone_online(&event_bus, &db, &dev.id, true);
                            if let Some(zone_id) = zone.id {
                                let vol = zone.volume as f64 / 100.0;
                                playback.set_volume(zone_id, vol).await;
                            }
                            info!(name = %dev.name, id = %dev.id, "mdns_zone_reconnected");
                            event_bus.emit(
                                "device.reconnected",
                                serde_json::json!({
                                    "device_id": &dev.id,
                                    "name": &dev.name,
                                }),
                            );
                        } else if airplay_v2_fallback {
                            // No existing zone for this device and the legacy
                            // AirPlay fallback can never play on it (v2 pairing
                            // required → ANNOUNCE 403): don't auto-create a
                            // guaranteed-dead zone, and don't let it capture an
                            // existing same-name/same-host zone of another
                            // protocol either (forum #1183).
                            info!(name = %dev.name, id = %dev.id, "mdns_zone_skipped_airplay_v2_fallback");
                        } else {
                            let existing = zone_repo.list().unwrap_or_default();

                            // When a higher-priority protocol discovers a device at the
                            // same host as an existing zone (e.g. BluOS vs AirPlay for a
                            // Bluesound Node), upgrade the zone to the better protocol
                            // instead of creating a duplicate.
                            let upgrade_zone = existing.iter().find(|z| {
                                if let Some(ref old_dev_id) = z.output_device_id {
                                    // Match by host: device IDs are formatted as
                                    // "{type}-{host}-{port}", extract the host part
                                    // from the existing zone's device_id.
                                    let old_host = old_dev_id.splitn(3, '-').nth(1).unwrap_or("");
                                    let is_same_host = old_host == dev.host;
                                    if !is_same_host {
                                        return false;
                                    }
                                    // Only upgrade if new protocol has higher priority
                                    let old_prio = match z.output_type.as_deref() {
                                        Some("oaat") => OutputType::Oaat.priority(),
                                        Some("openhome") => OutputType::Openhome.priority(),
                                        Some("bluos") => OutputType::Bluos.priority(),
                                        Some("squeezebox") => OutputType::Squeezebox.priority(),
                                        Some("dlna") => OutputType::Dlna.priority(),
                                        Some("chromecast") => OutputType::Chromecast.priority(),
                                        Some("airplay") => OutputType::Airplay.priority(),
                                        _ => 0,
                                    };
                                    dev.device_type.priority() > old_prio
                                } else {
                                    false
                                }
                            });

                            if let Some(z) = upgrade_zone
                                && let Some(zid) = z.id
                            {
                                // Remove the old lower-priority output
                                if let Some(ref old_dev_id) = z.output_device_id {
                                    reg.remove(old_dev_id);
                                }
                                let _ = zone_repo.update_output_device(zid, &dev.id);
                                let _ = zone_repo.update_output_type(zid, output_type_str);
                                set_zone_online(&event_bus, &db, &dev.id, true);
                                info!(
                                    name = %dev.name,
                                    id = %dev.id,
                                    old_id = ?z.output_device_id,
                                    old_type = ?z.output_type,
                                    new_type = output_type_str,
                                    "mdns_zone_upgraded_to_higher_priority"
                                );
                            } else {
                                // Check if a zone with the same name exists but different
                                // device_id (device_id may have changed after a firmware
                                // update / re-pairing).
                                let same_name_zone = existing.iter().find(|z| z.name == dev.name);
                                if let Some(z) = same_name_zone
                                    && let Some(zid) = z.id
                                {
                                    let _ = zone_repo.update_output_device(zid, &dev.id);
                                    let _ = zone_repo.update_output_type(zid, output_type_str);
                                    set_zone_online(&event_bus, &db, &dev.id, true);
                                    info!(name = %dev.name, id = %dev.id, old_id = ?z.output_device_id, "mdns_zone_device_updated");
                                } else if let Some(zid) =
                                    zone_repo.find_hidden_id_by_name(&dev.name)
                                {
                                    // Une zone SUPPRIMEE portant ce nom. Le
                                    // garde-fou `is_device_hidden` en haut de
                                    // ce bloc ne l'a pas vue : il teste le
                                    // nouvel identifiant, la ligne masquee
                                    // porte l'ancien. Et le rattrapage par nom
                                    // juste au-dessus ne pouvait pas la voir non
                                    // plus — `list()` filtre les masquees.
                                    // Sans ce cas, la zone renaissait a neuf a
                                    // chaque changement d'adresse (#1528).
                                    //
                                    // On la re-ancre sur le nouvel identifiant
                                    // SANS la demasquer : la suppression reste
                                    // une suppression, et le garde-fou redevient
                                    // operant des le tour suivant.
                                    let _ = zone_repo.update_output_device(zid, &dev.id);
                                    info!(name = %dev.name, id = %dev.id, zone_id = zid, "mdns_hidden_zone_reanchored");
                                } else {
                                    // Cross-protocol dedup (forum #1183): the
                                    // same physical device may already be a
                                    // zone through another protocol — e.g. a
                                    // Samsung S95BA TV present as a (renamed)
                                    // DLNA zone that also announces AirPlay
                                    // over mDNS. Match against the output
                                    // registry (same name + same host across
                                    // protocols) AND against existing zones by
                                    // the REAL host of their registered output
                                    // (`host_of`, robust for DLNA `uuid:…`
                                    // device_ids the old "{type}-{host}-{port}"
                                    // parsing mangled) or by case-insensitive
                                    // name. Skip zone creation on conflict.
                                    let registry_conflicts = reg.conflicting_outputs_same_host(
                                        &dev.name,
                                        output_type_str,
                                        &dev.host,
                                    );
                                    let zone_conflict = physical_zone_conflict(
                                        &zone_repo,
                                        &existing,
                                        |id| reg.host_of(id),
                                        &dev.name,
                                        &dev.host,
                                        dev.mac_address.as_deref(),
                                        output_type_str,
                                    );
                                    if !registry_conflicts.is_empty() || zone_conflict.is_some() {
                                        info!(
                                            name = %dev.name,
                                            id = %dev.id,
                                            host = %dev.host,
                                            r#type = output_type_str,
                                            registry_conflicts = ?registry_conflicts,
                                            conflicting_zone = ?zone_conflict,
                                            "mdns_zone_skipped_conflicting_protocol"
                                        );
                                    } else {
                                        // Check zone_auto_create setting
                                        let auto_create =
                                        tune_core::db::settings_repo::SettingsRepo::with_backend(
                                            db.clone(),
                                        )
                                        .get("zone_auto_create")
                                        .ok()
                                        .flatten()
                                        .map(|v| v != "false")
                                        .unwrap_or(true);
                                        if !auto_create {
                                            info!(name = %dev.name, id = %dev.id, "mdns_zone_auto_create_disabled_skipping");
                                        } else {
                                            // Auto-created zones start dormant; the
                                            // free-tier cap is enforced at first play.
                                            {
                                                match zone_repo.get_or_create(
                                                    &dev.name,
                                                    Some(output_type_str),
                                                    &dev.id,
                                                ) {
                                                    Ok((zid, true)) => {
                                                        // Persist the physical identity so later
                                                        // discoveries of the SAME device under
                                                        // another protocol/UUID find this zone.
                                                        let _ = zone_repo.set_identity(
                                                            zid,
                                                            &dev.host,
                                                            dev.mac_address.as_deref(),
                                                        );
                                                        event_bus.emit_typed(
                                                            EventType::ZoneCreated,
                                                            charge_utile_zone_creee(
                                                                &zone_repo,
                                                                zid,
                                                                serde_json::json!({
                                                                    "zone_id": zid,
                                                                    "name": dev.name,
                                                                    "device_id": dev.id,
                                                                    "type": output_type_str,
                                                                }),
                                                            ),
                                                        );
                                                        info!(name = %dev.name, zone_id = zid, r#type = output_type_str, "mdns_zone_auto_created");
                                                    }
                                                    Ok((zid, false)) => {
                                                        let _ = zone_repo.set_identity(
                                                            zid,
                                                            &dev.host,
                                                            dev.mac_address.as_deref(),
                                                        );
                                                        set_zone_online(
                                                            &event_bus, &db, &dev.id, true,
                                                        );
                                                        info!(name = %dev.name, zone_id = zid, "mdns_zone_already_existed");
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!(name = %dev.name, device = %dev.id, error = %e, "mdns_zone_create_failed");
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                MdnsEvent::DeviceLost(id) => {
                    let mut reg = outputs.lock().await;
                    reg.remove(&id);
                    drop(reg);
                    set_zone_online(&event_bus, &db, &id, false);
                    event_bus.emit_typed(
                        EventType::DeviceLost,
                        serde_json::json!({ "device_id": id }),
                    );
                    info!(id = %id, "mdns_output_removed_zone_offline");
                }
            }
        }
    });

    // After 15s, check for AirPlay zones whose host also speaks BluOS.
    // This catches Bluesound/NAD devices where _musc._tcp mDNS browse
    // didn't fire (common on Windows when multicast is partially blocked).
    {
        let outputs = state.outputs.clone();
        let db = state.backend.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            probe_airplay_for_bluos(&outputs, &db).await;
        });
    }

    handle
}

/// For every AirPlay zone, probe port 11000 to see if the device supports
/// BluOS.  If so, register a BluOS output and upgrade the zone.
async fn probe_airplay_for_bluos(
    outputs: &Arc<tokio::sync::Mutex<OutputRegistry>>,
    db: &Arc<dyn DbBackend>,
) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
    let zones = zone_repo.list().unwrap_or_default();

    let airplay_zones: Vec<_> = zones
        .iter()
        .filter(|z| z.output_type.as_deref() == Some("airplay"))
        .collect();

    if airplay_zones.is_empty() {
        return;
    }

    let client = tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();

    for z in airplay_zones {
        let Some(ref dev_id) = z.output_device_id else {
            continue;
        };
        // Extract host from mDNS device_id "airplay-{host}-{port}"
        let host = match dev_id.splitn(3, '-').nth(1) {
            Some(h) if !h.is_empty() => h,
            _ => continue,
        };

        // Skip if a BluOS zone already exists for this host
        let already_bluos = zones.iter().any(|zz| {
            zz.output_type.as_deref() == Some("bluos")
                && zz
                    .output_device_id
                    .as_deref()
                    .map_or(false, |id| id.splitn(3, '-').nth(1) == Some(host))
        });
        if already_bluos {
            continue;
        }

        let probe_url = format!("http://{host}:11000/Status");
        match client.get(&probe_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let bluos_id = format!("bluos-{host}-11000");
                let bluos = tune_core::outputs::bluos::BluosOutput::new(
                    z.name.clone(),
                    bluos_id.clone(),
                    host.to_string(),
                    11000,
                );

                let mut reg = outputs.lock().await;
                // Remove the old AirPlay output
                reg.remove(dev_id);
                reg.register(Box::new(bluos));
                drop(reg);

                if let Some(zid) = z.id {
                    let _ = zone_repo.update_output_device(zid, &bluos_id);
                    let _ = zone_repo.update_output_type(zid, "bluos");
                    let _ = zone_repo.set_online_by_device(&bluos_id, true);
                }

                info!(
                    name = %z.name,
                    host = host,
                    old_id = %dev_id,
                    new_id = %bluos_id,
                    "bluos_fallback_probe_upgraded_airplay_zone"
                );
            }
            _ => {
                tracing::debug!(host = host, name = %z.name, "bluos_fallback_probe_no_response");
            }
        }
    }
}

/// An existing zone that already exposes the same physical device through a
/// DIFFERENT protocol, if any. Used by the mDNS auto-create path to avoid
/// presenting one device as two zones (forum #1183: a Samsung S95BA TV already
/// present as a renamed DLNA zone also announces AirPlay over mDNS; the
/// auto-created legacy AirPlay zone was dead — the TV requires AirPlay 2
/// pairing).
///
/// A zone conflicts when its `output_type` differs from `new_type` AND either:
/// - the REAL host of its registered output equals `dev_host` — the host is
///   resolved through `resolve_host` (normally [`OutputRegistry::host_of`],
///   which recorded the output's `host()` at registration). This is robust for
///   DLNA zones whose `output_device_id` is a `uuid:…` string, which the old
///   `"{type}-{host}-{port}"` `splitn` parsing mangled into a UUID fragment; or
/// - the zone name equals the device name case-insensitively (a same-name
///   zone of another protocol, even if its output isn't registered yet).
///
/// [`OutputRegistry::host_of`]: tune_core::outputs::registry::OutputRegistry::host_of
/// Phase B of the MAC-identity chantier: the full cross-protocol duplicate
/// guard. Combines the in-memory check ([`find_cross_protocol_zone_conflict`]:
/// live registry host + exact name) with the **persisted** identity stored on
/// zones (host + MAC, [`ZoneRepo::find_visible_zone_by_identity`]). The
/// persisted side is what the in-memory check kept missing: it works when the
/// other protocol's output is not currently registered, when names differ
/// (BluOS vs UPnP friendly name), and across restarts and DHCP renumbering —
/// Bilou's Node ended up with three zones exactly through those gaps (#1239).
/// Returns the conflicting zone's name.
fn physical_zone_conflict(
    zone_repo: &tune_core::db::zone_repo::ZoneRepo,
    zones: &[tune_core::db::zone_repo::Zone],
    resolve_host: impl Fn(&str) -> Option<String>,
    dev_name: &str,
    dev_host: &str,
    dev_mac: Option<&str>,
    new_type: &str,
) -> Option<String> {
    if let Some(z) =
        find_cross_protocol_zone_conflict(zones, resolve_host, dev_name, dev_host, new_type)
    {
        return Some(z.name.clone());
    }
    if let Some((_, name, ztype)) = zone_repo.find_visible_zone_by_identity(dev_host, dev_mac) {
        // Same-type matches are the reconnect path (get_by_device_id /
        // zone_id_by_host handle those); only a DIFFERENT protocol on the
        // same physical device is a duplicate.
        if !ztype.is_empty() && !ztype.eq_ignore_ascii_case(new_type) {
            return Some(name);
        }
    }
    None
}

fn find_cross_protocol_zone_conflict<'a>(
    zones: &'a [tune_core::db::zone_repo::Zone],
    resolve_host: impl Fn(&str) -> Option<String>,
    dev_name: &str,
    dev_host: &str,
    new_type: &str,
) -> Option<&'a tune_core::db::zone_repo::Zone> {
    zones.iter().find(|z| {
        let zone_type = z.output_type.as_deref().unwrap_or("");
        if zone_type.is_empty() || zone_type.eq_ignore_ascii_case(new_type) {
            return false;
        }
        let same_host = z
            .output_device_id
            .as_deref()
            .and_then(&resolve_host)
            .is_some_and(|h| h.eq_ignore_ascii_case(dev_host));
        same_host || z.name.eq_ignore_ascii_case(dev_name)
    })
}

/// Register outputs coming from [`OutputProvider`]s handed to
/// `run::main_blocking` — the seam for out-of-tree output crates (e.g. the
/// private tune-diretta) that cannot appear in the public dependency graph.
///
/// Polls `discover()` at startup and then every 60 s (providers have no push
/// discovery the way SSDP/mDNS do), registers unseen device_ids, and applies
/// the same zone lifecycle as the mDNS handler: hidden zones stay deleted,
/// known device_ids reconnect (online + restored volume), renamed device_ids
/// re-attach by zone name, and new devices honour `zone_auto_create`.
/// La zone a re-ancrer sur le nouvel identifiant durable, s'il y en a une.
///
/// Rend `(zone_id, etait_masquee)`.
///
/// Les zones creees avant #1528 sont enregistrees sous l'identifiant derive de
/// l'adresse (`{type}-{host}-{port}`). Plutot qu'une migration SQL — impossible,
/// puisque les nouveaux identifiants ne sont connus qu'a la decouverte — chaque
/// appareil re-ancre sa zone a sa premiere reapparition. Les zones jamais
/// revues gardent leur ancien identifiant sans dommage.
///
/// Trois garde-fous, et le troisieme a ete ajoute apres coup — il manquait :
///
/// 1. **Ne rien faire si une zone porte deja le nouvel identifiant.** Sans
///    cela, une zone en double restee sur l'ancienne forme serait re-ancree
///    par-dessus la bonne, et deux zones partageraient la meme cle.
/// 2. Ne pas agir quand l'ancienne et la nouvelle forme coincident — cas d'un
///    appareil qui n'annonce aucun identifiant : il n'y a rien a deplacer.
/// 3. **Exiger que le nom corresponde.** L'ancien identifiant contient une
///    adresse IP, donc il n'identifie rien — c'est la these de ce correctif, et
///    l'oublier ici a coute une zone detournee (voir le commentaire sur le
///    test de nom).
fn legacy_zone_to_reanchor(
    zone_repo: &tune_core::db::zone_repo::ZoneRepo,
    dev: &tune_core::discovery::device::DiscoveredDevice,
) -> Option<(i64, bool)> {
    let legacy = tune_core::discovery::mdns::legacy_device_id(dev.device_type, &dev.host, dev.port);
    let new_id_taken = matches!(zone_repo.get_by_device_id(&dev.id), Ok(Some(_)));
    let zone = zone_repo.get_by_device_id(&legacy).ok().flatten()?;
    if !may_reanchor(&legacy, &dev.id, new_id_taken, &zone.name, &dev.name) {
        return None;
    }
    let zid = zone.id?;
    Some((zid, zone_repo.is_device_hidden(&legacy)))
}

/// La regle de re-ancrage, isolee pour etre testable — la fonction ci-dessus
/// demande une base et un appareil decouvert.
///
/// Le troisieme terme merite son histoire. Sur .18 le 13/08, l'Apple TV etait en
/// 192.168.1.37 ; le DHCP a donne cette adresse a une enceinte Sonos, qui
/// annonce aussi de l'AirPlay sur le port 7000 ; la zone « AppleTV14,1 » s'est
/// re-ancree sur le Sonos. Jouer sur l'Apple TV envoyait le son dans la chambre.
///
/// La cause : l'ancien identifiant contient une adresse IP, donc il n'identifie
/// rien — c'est la these meme de ce correctif, et le mecanisme de transition
/// l'avait oubliee. Le nom est le seul signal restant. Il est faillible, un
/// utilisateur renomme ; mais l'asymetrie tranche : un faux negatif laisse la
/// zone sur son ancien identifiant, c'est-a-dire l'etat d'avant le correctif,
/// tandis qu'un faux positif detourne le son vers une autre enceinte, en
/// silence.
fn may_reanchor(
    legacy_id: &str,
    new_id: &str,
    new_id_taken: bool,
    zone_name: &str,
    dev_name: &str,
) -> bool {
    legacy_id != new_id && !new_id_taken && zone_name == dev_name
}

/// Pourquoi chaque fournisseur de sortie hors-arbre est actif ou inerte.
///
/// #2392 : un module payant sans droit se retirait sans un mot. Vu de
/// l'extérieur, « non licencié », « non compilé » et « aucun appareil trouvé »
/// donnaient le même écran vide, et un bêta-testeur du module Diretta a
/// réinstallé Fedora, changé de système de fichiers et recompilé trente
/// minutes durant avant d'écrire « Je ne sais que faire ! ».
///
/// Cet instantané est lu par `/system/diagnostics`. Il tranche les trois cas :
/// un fournisseur **absent de la liste** n'est pas compilé ; un fournisseur
/// présent avec un `refusal` manque d'un droit, et le refus dit lequel et quoi
/// faire ; un fournisseur présent sans refus et à `devices: 0` cherche
/// vraiment et ne trouve rien.
///
/// Même motif que [`crate::boot_status`] : un état global minuscule, écrit par
/// la boucle qui le connaît, lu par la route qui l'affiche.
static STATUT_FOURNISSEURS: std::sync::LazyLock<std::sync::Mutex<serde_json::Value>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(serde_json::Value::Null));

/// L'instantané courant, pour `/system/diagnostics`. `null` tant qu'aucune
/// passe n'a eu lieu — y compris quand le binaire n'embarque aucun
/// fournisseur hors-arbre, ce qui est déjà une réponse.
pub fn provider_status_snapshot() -> serde_json::Value {
    STATUT_FOURNISSEURS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// L'état d'UN fournisseur après une passe de découverte.
///
/// Isolé du monde (pas de base, pas de réseau) pour être testable : c'est ici
/// que le silence de #2392 devient une raison nommée.
fn statut_du_fournisseur(
    nom: &str,
    module_requis: Option<&str>,
    appareils: usize,
    modules_licencies: &[String],
    compte_lie: bool,
) -> serde_json::Value {
    // Un fournisseur libre n'exige aucun droit : il ne peut donc pas être
    // refusé, et le nombre d'appareils reste le seul fait à lire.
    let refus = module_requis.and_then(|module| {
        let possede = modules_licencies.iter().any(|m| m == module);
        crate::premium_guard::ModuleRefusal::evaluate(possede, compte_lie)
            .map(|raison| raison.to_json(module))
    });

    serde_json::json!({
        "provider": nom,
        "required_module": module_requis,
        "devices": appareils,
        "refusal": refus,
    })
}

/// Un jeton de compte est-il stocké ? C'est la condition exacte sous laquelle
/// `refresh_account_premium` (`background.rs:1177`) sort sans rien faire et
/// sans rien dire, laissant `licensed_modules` vide à jamais.
fn compte_mozaik_lie(db: &Arc<dyn DbBackend>) -> bool {
    tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get("mozaik_access_token")
        .ok()
        .flatten()
        .is_some_and(|t| !t.is_empty())
}

pub fn spawn_output_providers(
    state: &AppState,
    providers: Vec<Arc<dyn tune_core::outputs::traits::OutputProvider>>,
) {
    if providers.is_empty() {
        return;
    }
    let outputs = state.outputs.clone();
    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    let playback = state.playback.clone();
    let license = state.license.clone();

    tokio::spawn(async move {
        loop {
            // Rebuilt every poll so a module bought (or refunded) mid-session
            // takes effect at the next discovery pass, without a restart.
            let ctx = tune_core::outputs::traits::ProviderContext {
                licensed_modules: license.modules().await,
            };
            let compte_lie = compte_mozaik_lie(&db);
            let mut statuts = Vec::with_capacity(providers.len());
            for provider in &providers {
                let trouves = provider.discover(&ctx).await;
                statuts.push(statut_du_fournisseur(
                    provider.provider_name(),
                    provider.required_module(),
                    trouves.len(),
                    &ctx.licensed_modules,
                    compte_lie,
                ));
                for output in trouves {
                    let dev_id = output.device_id().to_string();
                    let name = output.name().to_string();
                    let otype = output.output_type().to_string();
                    {
                        let mut reg = outputs.lock().await;
                        if reg.contains(&dev_id) {
                            continue;
                        }
                        reg.register(output);
                    }
                    info!(
                        provider = provider.provider_name(),
                        name = %name,
                        id = %dev_id,
                        r#type = %otype,
                        "provider_output_registered"
                    );

                    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
                    if zone_repo.is_device_hidden(&dev_id) {
                        info!(name = %name, id = %dev_id, "provider_zone_hidden_skipping");
                        continue;
                    }
                    if let Ok(Some(zone)) = zone_repo.get_by_device_id(&dev_id) {
                        set_zone_online(&event_bus, &db, &dev_id, true);
                        if let Some(zone_id) = zone.id {
                            let vol = zone.volume as f64 / 100.0;
                            playback.set_volume(zone_id, vol).await;
                        }
                        info!(name = %name, id = %dev_id, "provider_zone_reconnected");
                        event_bus.emit(
                            "device.reconnected",
                            serde_json::json!({
                                "device_id": &dev_id,
                                "name": &name,
                            }),
                        );
                    } else {
                        // Device_id may have changed (firmware update / re-pairing):
                        // re-attach an existing zone by name before creating one.
                        let existing = zone_repo.list().unwrap_or_default();
                        if let Some(z) = existing.iter().find(|z| z.name == name)
                            && let Some(zid) = z.id
                        {
                            let _ = zone_repo.update_output_device(zid, &dev_id);
                            let _ = zone_repo.update_output_type(zid, &otype);
                            set_zone_online(&event_bus, &db, &dev_id, true);
                            info!(name = %name, id = %dev_id, old_id = ?z.output_device_id, "provider_zone_device_updated");
                        } else {
                            let auto_create =
                                tune_core::db::settings_repo::SettingsRepo::with_backend(
                                    db.clone(),
                                )
                                .get("zone_auto_create")
                                .ok()
                                .flatten()
                                .map(|v| v != "false")
                                .unwrap_or(true);
                            if !auto_create {
                                info!(name = %name, id = %dev_id, "provider_zone_auto_create_disabled_skipping");
                            } else {
                                match zone_repo.get_or_create(&name, Some(&otype), &dev_id) {
                                    Ok((zid, true)) => {
                                        event_bus.emit_typed(
                                            EventType::ZoneCreated,
                                            charge_utile_zone_creee(
                                                &zone_repo,
                                                zid,
                                                serde_json::json!({
                                                    "zone_id": zid,
                                                    "name": name,
                                                }),
                                            ),
                                        );
                                        set_zone_online(&event_bus, &db, &dev_id, true);
                                        info!(name = %name, id = %dev_id, zone_id = zid, "provider_zone_created");
                                    }
                                    Ok((_, false)) => {
                                        set_zone_online(&event_bus, &db, &dev_id, true);
                                    }
                                    Err(e) => {
                                        tracing::warn!(name = %name, id = %dev_id, error = %e, "provider_zone_create_failed");
                                    }
                                }
                            }
                        }
                    }
                }
            }
            publier_statut_fournisseurs(statuts, &ctx.licensed_modules, compte_lie);
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

/// Publie l'instantané et **journalise ce qui a changé**, au-dessus de
/// `debug`.
///
/// « Une fois » compte autant que « au-dessus de `debug` » : la boucle repasse
/// toutes les soixante secondes, et un avertissement répété mille fois par
/// nuit se fait filtrer comme du bruit — donc redevient invisible, ce qui est
/// le défaut qu'on corrige. On ne réémet donc que sur changement réel
/// (démarrage, compte lié, module acheté, remboursement).
fn publier_statut_fournisseurs(
    statuts: Vec<serde_json::Value>,
    modules_licencies: &[String],
    compte_lie: bool,
) {
    let instantane = serde_json::json!({
        "account_linked": compte_lie,
        "licensed_modules": modules_licencies,
        "providers": statuts,
    });

    let change = {
        let mut courant = STATUT_FOURNISSEURS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // `null` = première passe : on parle toujours au démarrage.
        let change = courant.is_null()
            || refus_nommes(&courant) != refus_nommes(&instantane)
            || courant["account_linked"] != instantane["account_linked"];
        *courant = instantane;
        change
    };

    if !change {
        return;
    }

    // Le résumé de démarrage. Il vaut même quand AUCUN fournisseur ne déclare
    // encore `required_module()` : un binaire qui embarque un fournisseur
    // hors-arbre — donc, en pratique, un module payant — et qui tourne sans
    // compte lié ne recevra jamais le moindre droit. C'est très exactement la
    // configuration du bêta-testeur de #2392, et rien ne la disait.
    let instantane = provider_status_snapshot();
    let noms: Vec<&str> = instantane["providers"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|p| p["provider"].as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if compte_lie {
        info!(
            providers = ?noms,
            licensed_modules = ?modules_licencies,
            "output_providers_status"
        );
    } else {
        warn!(
            providers = ?noms,
            "output_providers_no_linked_account: no Mozaiklabs account is linked, so no paid module entitlement can reach this server — a module you own stays idle until the account is connected (a license key alone never carries it)"
        );
    }

    for (fournisseur, code, message) in refus_nommes(&instantane) {
        warn!(
            provider = %fournisseur,
            code = %code,
            account_linked = compte_lie,
            "output_provider_module_refused: {message}"
        );
    }
}

/// Les refus nommés d'un instantané : `(fournisseur, code, message)`.
fn refus_nommes(instantane: &serde_json::Value) -> Vec<(String, String, String)> {
    instantane["providers"]
        .as_array()
        .map(|liste| {
            liste
                .iter()
                .filter_map(|p| {
                    let refus = p.get("refusal")?.as_object()?;
                    Some((
                        p["provider"].as_str().unwrap_or_default().to_string(),
                        refus.get("code")?.as_str().unwrap_or_default().to_string(),
                        refus
                            .get("message")?
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        find_cross_protocol_zone_conflict, may_reanchor, refus_nommes, register_discovered_output,
        resolve_control_url, statut_du_fournisseur,
    };
    use tune_core::db::zone_repo::Zone;
    use tune_core::discovery::device::{DiscoveredDevice, OutputType};
    use tune_core::event_bus::EventBus;

    fn zone(name: &str, output_type: &str, device_id: &str) -> Zone {
        Zone {
            id: Some(1),
            name: name.to_string(),
            output_type: Some(output_type.to_string()),
            output_device_id: Some(device_id.to_string()),
            volume: 50,
            muted: false,
            online: true,
            gapless_enabled: true,
            group_id: None,
            sync_delay_ms: 0,
            last_position_ms: 0,
            last_track_id: None,
            last_track_source: None,
            last_track_source_id: None,
            max_sample_rate: None,
            fixed_volume: false,
            autoplay_enabled: false,
        }
    }

    /// #2273: the web client reloads Settings > Network only for `device.*`.
    /// Automatic discovery must therefore publish the same canonical contract
    /// as manual registration, independently of any later zone decision.
    #[tokio::test]
    async fn automatic_discovery_emits_canonical_device_event() {
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let dev = DiscoveredDevice::new(
            "cast-living-room".into(),
            "Salon".into(),
            OutputType::Chromecast,
            "192.0.2.42".into(),
            8009,
        );

        let output = tune_core::outputs::chromecast::ChromecastOutput::new(
            dev.name.clone(),
            dev.id.clone(),
            dev.host.clone(),
            dev.port,
        );
        let mut registry = tune_core::outputs::OutputRegistry::new();

        register_discovered_output(&mut registry, Box::new(output), &bus, &dev, "chromecast");

        let event = events.recv().await.expect("device event");
        assert!(registry.contains("cast-living-room"));
        assert_eq!(event.event_type, "device.discovered");
        assert_eq!(
            event.data,
            serde_json::json!({
                "device_id": "cast-living-room",
                "name": "Salon",
                "device_type": "chromecast",
                "host": "192.0.2.42",
            })
        );
    }

    /// Forum #1183: a Samsung S95BA TV is already a DLNA zone — renamed by the
    /// user, with a `uuid:…` device_id (so neither the exact-name dedup nor the
    /// old `splitn('-')` host extraction can match it) — and then announces
    /// AirPlay over mDNS at the same host. The conflict must be detected via
    /// the output registry's real host so no second (dead) zone is created.
    #[test]
    fn dlna_uuid_zone_conflicts_with_airplay_arrival_on_same_host() {
        let zones = vec![zone(
            "TV Salon", // renamed: name no longer matches the device name
            "dlna",
            "uuid:3a4eedf4-1bf0-4c9a-9c2b-0123456789ab",
        )];
        let resolve = |id: &str| {
            (id == "uuid:3a4eedf4-1bf0-4c9a-9c2b-0123456789ab").then(|| "192.168.1.42".to_string())
        };
        let hit = find_cross_protocol_zone_conflict(
            &zones,
            resolve,
            "Samsung S95BA",
            "192.168.1.42",
            "airplay",
        );
        assert_eq!(hit.map(|z| z.name.as_str()), Some("TV Salon"));

        // A different host is NOT a conflict.
        assert!(
            find_cross_protocol_zone_conflict(
                &zones,
                resolve,
                "Samsung S95BA",
                "192.168.1.99",
                "airplay",
            )
            .is_none()
        );
    }

    #[test]
    fn same_protocol_never_conflicts() {
        // Two AirPlay devices on the same host (e.g. an AV receiver exposing
        // several inputs) must not dedup against their own protocol.
        let zones = vec![zone("Ampli HC", "airplay", "airplay-192.168.1.42-7000")];
        let resolve = |_: &str| Some("192.168.1.42".to_string());
        assert!(
            find_cross_protocol_zone_conflict(
                &zones,
                resolve,
                "Ampli HC Zone 2",
                "192.168.1.42",
                "airplay",
            )
            .is_none()
        );
    }

    #[test]
    fn case_insensitive_name_conflicts_even_without_registered_output() {
        // The zone's output isn't registered (host unresolvable), but the name
        // matches case-insensitively across protocols → still a conflict.
        let zones = vec![zone("Samsung S95BA", "dlna", "uuid:dead-beef")];
        let resolve = |_: &str| None;
        assert!(
            find_cross_protocol_zone_conflict(
                &zones,
                resolve,
                "SAMSUNG s95ba",
                "192.168.1.42",
                "airplay",
            )
            .is_some()
        );
        // Different name + unresolvable host → no conflict.
        assert!(
            find_cross_protocol_zone_conflict(
                &zones,
                resolve,
                "Chambre",
                "192.168.1.42",
                "airplay",
            )
            .is_none()
        );
    }

    #[test]
    fn relative_control_url_joins_host_port() {
        // The common case: a leading-slash relative path (Denon DMP-A10, LHC).
        assert_eq!(
            resolve_control_url("192.168.68.50", 8080, "/MediaRenderer/AVTransport/Control"),
            "http://192.168.68.50:8080/MediaRenderer/AVTransport/Control"
        );
    }

    #[test]
    fn relative_without_leading_slash_gets_one() {
        assert_eq!(
            resolve_control_url("10.0.0.2", 55000, "upnp/control/AVTransport"),
            "http://10.0.0.2:55000/upnp/control/AVTransport"
        );
    }

    #[test]
    fn absolute_control_url_kept_as_is() {
        // Frontier Silicon radios (Ruark R3, Stream 94i Plus) advertise an
        // absolute controlURL. It must be used verbatim — prefixing host:port
        // would yield `http://host:PORThttp://...` (invalid port) → the reqwest
        // `soap send: builder error` that left Yves with no sound.
        let abs = "http://192.168.68.55:8080/dev0/srv1/control";
        assert_eq!(resolve_control_url("192.168.68.55", 8080, abs), abs);
        let abs_https = "https://192.168.68.55:443/control";
        assert_eq!(
            resolve_control_url("192.168.68.55", 443, abs_https),
            abs_https
        );
    }

    #[test]
    fn reanchor_refuses_a_zone_whose_name_no_longer_matches() {
        // Vecu sur .18 le 13/08. L'Apple TV etait en 192.168.1.37 ; le DHCP a
        // donne cette adresse a une enceinte Sonos qui annonce aussi de
        // l'AirPlay sur le port 7000. Sans ce refus, la zone « AppleTV14,1 »
        // se re-ancre sur le Sonos et le son part dans la chambre.
        assert!(!may_reanchor(
            "airplay-192.168.1.37-7000",
            "airplay-BA:C9:C4:56:04:E8",
            false,
            "AppleTV14,1",
            "Chambre",
        ));
    }

    #[test]
    fn reanchor_accepts_the_same_device_under_a_new_identity() {
        assert!(may_reanchor(
            "airplay-192.168.1.37-7000",
            "airplay-AA:BB:CC:DD:EE:FF",
            false,
            "AppleTV14,1",
            "AppleTV14,1",
        ));
    }

    #[test]
    fn reanchor_never_steals_an_identity_already_in_use() {
        // Une zone en double restee sur l'ancienne forme ne doit pas etre
        // re-ancree par-dessus la bonne : deux zones partageraient la cle.
        assert!(!may_reanchor(
            "airplay-192.168.1.37-7000",
            "airplay-AA:BB:CC:DD:EE:FF",
            true,
            "AppleTV14,1",
            "AppleTV14,1",
        ));
    }

    #[test]
    fn reanchor_does_nothing_when_both_forms_are_identical() {
        // L'appareil n'annonce aucun identifiant : rien a deplacer.
        assert!(!may_reanchor(
            "dlna-192.168.1.9-8080",
            "dlna-192.168.1.9-8080",
            false,
            "Salon",
            "Salon",
        ));
    }

    // ── Un appareil physique = une LOCATION (#1703) ───────────────────────

    // --- zone.created : une seule forme pour deux emetteurs ---

    mod forme_de_zone_creee {
        use super::super::charge_utile_zone_creee;
        use std::sync::Arc;
        use tune_core::db::backend::DbBackend;
        use tune_core::db::zone_repo::ZoneRepo;

        fn base() -> Arc<dyn DbBackend> {
            let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
            db.init_schema().unwrap();
            tune_core::db::migrations::run_migrations(&db).unwrap();
            Arc::new(db)
        }

        /// La regression : le client teste `data.zone` avant de fusionner. Les
        /// trois emetteurs de la decouverte publiaient a plat, sans cette cle —
        /// la condition etait fausse et la zone n'apparaissait qu'au
        /// rechargement de la page (#2224).
        #[test]
        fn la_decouverte_porte_la_zone_entiere_comme_la_route_api() {
            let db = base();
            let repo = ZoneRepo::with_backend(db.clone());
            let (zid, cree) = repo
                .get_or_create("Salon", Some("dlna"), "uuid:abcd")
                .expect("creation de zone");
            assert!(cree, "la zone doit etre neuve pour que le test ait un sens");

            let charge = charge_utile_zone_creee(
                &repo,
                zid,
                serde_json::json!({
                    "zone_id": zid,
                    "name": "Salon",
                    "device_id": "uuid:abcd",
                    "type": "dlna",
                }),
            );

            // Ce que le client attend, et qui manquait.
            let zone = charge
                .get("zone")
                .expect("sans la cle `zone`, le client ignore l'evenement");
            assert_eq!(zone.get("id").and_then(|v| v.as_i64()), Some(zid));
            assert_eq!(charge.get("id").and_then(|v| v.as_i64()), Some(zid));

            // La contre-epreuve de JP Robbe : la charge utile doit porter le
            // CONTRAT client, pas la ligne de base. Le volume y passe de 0..100
            // a 0..1 — un `to_value(&zone)` brut rendait 50.0 la ou le client
            // attend 0.5, et le curseur se collait au maximum.
            assert_eq!(
                zone.get("volume").and_then(|v| v.as_f64()),
                Some(0.5),
                "volume en contrat client (0..1), pas la valeur de la base"
            );
            // Et l'etat de lecture est POSE, pas omis : le client fusionne sans
            // refetch, un champ absent y laisserait la valeur d'une autre zone.
            for champ in [
                "state",
                "current_track",
                "position_ms",
                "queue_length",
                "shuffle",
                "repeat",
            ] {
                assert!(
                    zone.get(champ).is_some(),
                    "{champ} absent : le client garderait la valeur precedente"
                );
            }

            // Et ce que les autres consommateurs lisaient deja : rien n'est retire.
            assert_eq!(
                charge.get("zone_id").and_then(|v| v.as_i64()),
                Some(zid),
                "les champs plats restent : plugins et developer_api les lisent"
            );
            assert_eq!(
                charge.get("device_id").and_then(|v| v.as_str()),
                Some("uuid:abcd")
            );
        }

        /// Si la relecture echoue, on emet la forme plate seule plutot que
        /// rien : l'ancien comportement, jamais pire.
        #[test]
        fn une_zone_introuvable_ne_fait_pas_perdre_l_evenement() {
            let db = base();
            let repo = ZoneRepo::with_backend(db.clone());
            let charge = charge_utile_zone_creee(
                &repo,
                4242,
                serde_json::json!({ "zone_id": 4242, "name": "Fantome" }),
            );
            assert!(charge.get("zone").is_none());
            assert_eq!(charge.get("zone_id").and_then(|v| v.as_i64()), Some(4242));
            assert_eq!(charge.get("id").and_then(|v| v.as_i64()), Some(4242));
        }
    }

    mod auto_exclusion_par_udn {
        use super::super::est_un_de_nos_udn_de_facade;
        use std::sync::Arc;
        use tune_core::db::backend::DbBackend;

        fn memory_backend() -> Arc<dyn DbBackend> {
            let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
            db.init_schema().unwrap();
            tune_core::db::migrations::run_migrations(&db).unwrap();
            Arc::new(db)
        }

        // La greffe du 25/08 sur .18 : la façade de la zone 10 revient par
        // SSDP avec l'UDN que nous avons nous-mêmes persisté, et devient la
        // zone fantôme « Eversolo DMP-A8 (Tune) ». L'annonce doit être
        // reconnue comme notre reflet — et l'appareil physique, lui, passer.
        #[test]
        fn notre_udn_persiste_est_reconnu_l_appareil_physique_passe() {
            let backend = memory_backend();
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
            settings
                .set(
                    "upnp_renderer_udn_10",
                    "uuid:558dcf82-5868-4126-951a-570149376da6",
                )
                .unwrap();

            assert!(est_un_de_nos_udn_de_facade(
                &backend,
                "uuid:558dcf82-5868-4126-951a-570149376da6"
            ));
            assert!(!est_un_de_nos_udn_de_facade(
                &backend,
                "uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE"
            ));
        }

        // Un réglage voisin (`upnp_renderer_10`, opt-in booléen) ne doit pas
        // faire écarter un appareil, et un device_id vide non plus.
        #[test]
        fn un_reglage_voisin_ou_un_id_vide_n_excluent_rien() {
            let backend = memory_backend();
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
            settings.set("upnp_renderer_10", "true").unwrap();
            settings.set("upnp_renderer_udn_10", "").unwrap();

            assert!(!est_un_de_nos_udn_de_facade(&backend, "true"));
            assert!(!est_un_de_nos_udn_de_facade(&backend, ""));
        }
    }

    mod known_renderers_par_location {
        use super::super::{KnownRenderer, dedup_renderers_by_location, persist_known_renderer};
        use std::sync::Arc;
        use tune_core::db::backend::DbBackend;

        const AIOS: &str = "http://192.168.1.11:60006/upnp/desc/aios_device/aios_device.xml";

        fn memory_backend() -> Arc<dyn DbBackend> {
            let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
            db.init_schema().unwrap();
            // La table `settings` naît d'une migration : sans elle le magasin
            // relit toujours vide et le test ne prouve rien.
            tune_core::db::migrations::run_migrations(&db).unwrap();
            Arc::new(db)
        }

        #[test]
        fn les_udn_freres_d_un_heos_ne_font_qu_une_entree() {
            // Le ND8006 de Jean Valjean : cinq `uuid:` annoncés, une seule
            // description racine. Sans le correctif, cinq entrées persistées
            // et cinq re-sondages au démarrage suivant.
            let backend = memory_backend();
            for i in 0..5 {
                persist_known_renderer(&backend, &format!("uuid:aios-{i}"), AIOS, "Marantz ND8006");
            }
            let stored = super::super::load_known_renderers(&backend);
            assert_eq!(
                stored.len(),
                1,
                "un appareil physique = une entrée, pas {:?}",
                stored.iter().map(|r| &r.device_id).collect::<Vec<_>>()
            );
        }

        #[test]
        fn deux_locations_restent_deux_appareils() {
            // Garde-fou : un ampli multi-zone expose deux descriptions
            // distinctes derrière la même adresse — on ne doit en perdre
            // aucune.
            let backend = memory_backend();
            persist_known_renderer(
                &backend,
                "uuid:z1",
                "http://192.168.1.11:8080/desc.xml",
                "Zone 1",
            );
            persist_known_renderer(
                &backend,
                "uuid:z2",
                "http://192.168.1.11:8081/desc.xml",
                "Zone 2",
            );
            assert_eq!(super::super::load_known_renderers(&backend).len(), 2);
        }

        #[test]
        fn un_magasin_deja_dedouble_se_replie() {
            let stored: Vec<KnownRenderer> = (0..5)
                .map(|i| KnownRenderer {
                    device_id: format!("uuid:aios-{i}"),
                    location: AIOS.to_string(),
                    name: "Marantz ND8006".into(),
                })
                .collect();
            let collapsed = dedup_renderers_by_location(stored);
            assert_eq!(collapsed.len(), 1);
            assert_eq!(collapsed[0].device_id, "uuid:aios-0");
        }
    }

    // --- Ne pas se découvrir soi-même ---

    mod auto_decouverte {
        use super::super::est_notre_propre_renderer;

        const NOUS: &[&str] = &["192.168.1.10", "127.0.0.1", "localhost"];

        fn nos() -> Vec<String> {
            NOUS.iter().map(|s| s.to_string()).collect()
        }

        /// Le cas exact des journaux de Jean Valjean : trois de ces adresses
        /// étaient sondées à chaque démarrage, pour des zones supprimées.
        #[test]
        fn notre_propre_zone_est_reconnue() {
            for zone in [3, 10, 13] {
                let loc = format!("http://192.168.1.10:8888/upnp/renderer/{zone}/description.xml");
                assert!(est_notre_propre_renderer(&loc, 8888, 8888, &nos()), "{loc}");
            }
        }

        /// Le défaut que ce lot corrige, testé LÀ OÙ IL ÉTAIT : dans
        /// l'assemblage de nos adresses, pas dans la comparaison.
        ///
        /// L'ancien code ne retenait que l'adresse élue. Une annonce arrivant
        /// par une AUTRE de nos interfaces n'était donc pas reconnue comme
        /// nôtre, et Tune adoptait son propre renderer comme un appareil du
        /// réseau. Ce test échoue sur l'ancienne version : elle ne rendait que
        /// la loopback et l'élue.
        #[test]
        fn toutes_nos_interfaces_comptent_pour_nous() {
            use std::net::Ipv4Addr;
            // Ethernet, Wi-Fi, pont Docker — une seule machine, trois adresses.
            let interfaces = [
                Ipv4Addr::new(192, 168, 1, 10),
                Ipv4Addr::new(192, 168, 4, 22),
                Ipv4Addr::new(172, 17, 0, 1),
            ];
            // L'élue est l'une d'elles : c'est le cas nominal.
            let nous = super::super::nos_adresses_depuis(&interfaces, Some(interfaces[0]));

            for ip in &interfaces {
                let loc = format!("http://{ip}:8888/upnp/renderer/1/description.xml");
                assert!(
                    est_notre_propre_renderer(&loc, 8888, 8888, &nous),
                    "annonce reçue par {ip} : c'est nous, et on ne se voyait pas"
                );
            }
            // Les formes locales restent portées.
            assert!(nous.iter().any(|a| a == "127.0.0.1"));
            assert!(nous.iter().any(|a| a == "localhost"));
            // Et aucune adresse en double.
            let mut tri = nous.clone();
            tri.sort();
            tri.dedup();
            assert_eq!(tri.len(), nous.len(), "doublons dans nos adresses");
        }

        /// Ceinture et bretelles : énumération vide (conteneur sans droits sur
        /// les interfaces), l'élue reste une réponse valable.
        #[test]
        fn sans_enumeration_lelue_suffit_encore() {
            use std::net::Ipv4Addr;
            let nous = super::super::nos_adresses_depuis(&[], Some(Ipv4Addr::new(192, 168, 1, 10)));
            let loc = "http://192.168.1.10:8888/upnp/renderer/1/description.xml";
            assert!(est_notre_propre_renderer(loc, 8888, 8888, &nous));
        }

        /// Contre-épreuve : élargir la liste ne doit pas nous faire avaler les
        /// voisins. Un autre serveur Tune du réseau reste pilotable.
        #[test]
        fn plusieurs_adresses_nexcluent_pas_un_autre_serveur() {
            use std::net::Ipv4Addr;
            let nous = super::super::nos_adresses_depuis(
                &[Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(172, 17, 0, 1)],
                None,
            );
            let loc = "http://192.168.1.77:8888/upnp/renderer/2/description.xml";
            assert!(!est_notre_propre_renderer(loc, 8888, 8888, &nous));
        }

        /// La règle décisive : un AUTRE serveur Tune du réseau publie ses zones
        /// au même port et au même chemin. Les siennes sont pilotables — les
        /// écarter serait perdre une fonction, pas corriger un défaut.
        #[test]
        fn un_autre_serveur_tune_reste_visible() {
            let loc = "http://192.168.1.77:8888/upnp/renderer/2/description.xml";
            assert!(!est_notre_propre_renderer(loc, 8888, 8888, &nos()));
        }

        #[test]
        fn un_vrai_appareil_nest_jamais_confondu() {
            // Le Marantz de Jean Valjean, et un Sonos : ni le chemin ni le
            // port ne correspondent.
            assert!(!est_notre_propre_renderer(
                "http://192.168.1.11:60006/upnp/desc/aios_device/aios_device.xml",
                60006,
                8888,
                &nos()
            ));
            assert!(!est_notre_propre_renderer(
                "http://192.168.1.50:1400/xml/device_description.xml",
                1400,
                8888,
                &nos()
            ));
        }

        #[test]
        fn le_chemin_seul_ne_suffit_pas() {
            // Un appareil tiers qui servirait par hasard un chemin semblable
            // sur un autre port n'est pas nous.
            let loc = "http://192.168.1.10:9999/upnp/renderer/1/description.xml";
            assert!(!est_notre_propre_renderer(loc, 9999, 8888, &nos()));
        }

        #[test]
        fn le_port_seul_ne_suffit_pas() {
            // Notre propre MediaServer — même hôte, même port, autre chemin :
            // il ne s'agit pas d'un renderer de zone.
            let loc = "http://192.168.1.10:8888/upnp/server/description.xml";
            assert!(!est_notre_propre_renderer(loc, 8888, 8888, &nos()));
        }

        #[test]
        fn les_formes_locales_comptent_pour_nous() {
            // Un M-SEARCH émis depuis la machine elle-même peut nous revenir
            // sous la boucle locale.
            for hote in ["127.0.0.1", "localhost"] {
                let loc = format!("http://{hote}:8888/upnp/renderer/1/description.xml");
                assert!(est_notre_propre_renderer(&loc, 8888, 8888, &nos()), "{loc}");
            }
        }

        #[test]
        fn une_adresse_illisible_ne_nous_designe_pas() {
            // Mieux vaut laisser passer un inconnu que de jeter un appareil
            // réel sur une adresse qu'on n'a pas su lire.
            for loc in [
                "",
                "pas-une-url",
                "http://",
                "http:///upnp/renderer/1/x.xml",
            ] {
                assert!(!est_notre_propre_renderer(loc, 8888, 8888, &nos()), "{loc}");
            }
        }
    }

    /// #2392, le cas du bêta-testeur Diretta : le fournisseur EST compilé et
    /// enregistré, son droit est acheté, mais aucun compte n'est lié — donc
    /// `licensed_modules` est vide et le fournisseur se retire sans un mot.
    ///
    /// Le diagnostic doit nommer la raison ET le geste qui la lève. Sans ça,
    /// « pas de droit » est indiscernable de « rien sur le réseau », et c'est
    /// une réinstallation complète de système d'exploitation qui se déclenche.
    #[test]
    fn un_fournisseur_paye_sans_compte_lie_dit_pourquoi_il_est_inerte() {
        let statut = statut_du_fournisseur("diretta", Some("diretta"), 0, &[], false);

        assert_eq!(statut["provider"], "diretta");
        assert_eq!(statut["required_module"], "diretta");
        assert_eq!(
            statut["refusal"]["code"], "module_account_not_linked",
            "le diagnostic doit nommer la raison, pas la taire : {statut}"
        );
        assert_eq!(statut["refusal"]["action"], "link_account");
        assert_eq!(statut["refusal"]["module"], "diretta");
    }

    /// Compte bien lié, module non acheté : même écran vide, autre geste. Les
    /// deux causes doivent rester distinctes côté client.
    #[test]
    fn un_fournisseur_paye_avec_compte_lie_mais_non_achete_dit_autre_chose() {
        let statut = statut_du_fournisseur("diretta", Some("diretta"), 0, &[], true);
        assert_eq!(statut["refusal"]["code"], "module_not_owned");
        assert_eq!(statut["refusal"]["action"], "purchase_module");
    }

    /// Droit présent : aucun refus, et le compte d'appareils devient le seul
    /// fait à lire — c'est ce qui sépare « pas de droit » de « rien trouvé ».
    #[test]
    fn un_fournisseur_licencie_ne_refuse_rien_et_rend_compte_de_sa_recherche() {
        let licencies = vec!["diretta".to_string()];
        let vide = statut_du_fournisseur("diretta", Some("diretta"), 0, &licencies, true);
        assert!(
            vide["refusal"].is_null(),
            "un module possede ne doit pas etre presente comme refuse : {vide}"
        );
        assert_eq!(vide["devices"], 0);

        let trouve = statut_du_fournisseur("diretta", Some("diretta"), 2, &licencies, true);
        assert!(trouve["refusal"].is_null());
        assert_eq!(trouve["devices"], 2);
    }

    /// Un fournisseur libre (le défaut du contrat : `required_module() == None`)
    /// n'est jamais refusé, même sans compte lié — sinon le correctif
    /// inventerait un refus là où il n'y en a pas.
    #[test]
    fn un_fournisseur_libre_nest_jamais_refuse() {
        let statut = statut_du_fournisseur("snapcast", None, 0, &[], false);
        assert!(statut["required_module"].is_null());
        assert!(
            statut["refusal"].is_null(),
            "aucun droit exige, donc aucun refus : {statut}"
        );
    }

    /// Le journal n'est réémis que lorsque l'ensemble des refus CHANGE — la
    /// boucle repasse toutes les soixante secondes, et un avertissement répété
    /// mille fois par nuit se fait filtrer comme du bruit, c'est-à-dire
    /// redevient le silence qu'on corrige. C'est `refus_nommes` qui tranche.
    #[test]
    fn les_refus_se_comparent_pour_ne_pas_rejournaliser_a_chaque_passe() {
        let passe = |compte_lie| {
            serde_json::json!({
                "providers": [
                    statut_du_fournisseur("diretta", Some("diretta"), 0, &[], compte_lie),
                    statut_du_fournisseur("snapcast", None, 1, &[], compte_lie),
                ]
            })
        };

        let refus = refus_nommes(&passe(false));
        assert_eq!(
            refus.len(),
            1,
            "seul le fournisseur paye est refuse : {refus:?}"
        );
        assert_eq!(refus[0].0, "diretta");
        assert_eq!(refus[0].1, "module_account_not_linked");

        // Deux passes identiques : rien de neuf à dire.
        assert_eq!(refus_nommes(&passe(false)), refus);
        // Le compte se lie : la raison change, donc on reparle.
        assert_ne!(refus_nommes(&passe(true)), refus);
        // Et un instantané vide (aucune passe encore) ne refuse rien.
        assert!(refus_nommes(&serde_json::Value::Null).is_empty());
    }
}
