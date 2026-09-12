use serde::{Deserialize, Serialize};

/// Est-ce que ce nom part vraiment sur le fil, ou n'est-il que reserve ?
///
/// L'enumeration declarait trente noms dont la majorite n'avait aucun emetteur,
/// et rien ne le disait : quatre ecrans du client attendaient des evenements qui
/// ne partaient jamais (#2870). Chaque variante porte donc desormais son statut,
/// et le test `chaque_variante_emise_a_au_moins_un_emetteur` le VERIFIE contre
/// l'arbre des sources — un nom declare emis sans emetteur fait tomber la suite,
/// et un nom declare reserve qu'on se met a emettre aussi.
///
/// ⚠️ On n'ELAGUE PAS les variantes reservees : les clients iOS et Flutter
/// vivent dans d'autres depots, et on ne sait pas ce qu'ils ecoutent. Retirer un
/// nom casserait un contrat inter-depots sans qu'aucune CI ne le voie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatutEmission {
    /// Au moins un site d'emission existe dans l'arbre — le client peut compter
    /// dessus.
    Emis,
    /// Nom declare, jamais produit. Un client qui l'ecoute attend pour rien.
    Reserve,
}

/// Declare l'enumeration, le nom de fil et le statut d'emission **en un seul
/// endroit**.
///
/// Les trois listes vivaient separement (l'enum, le `match` de `as_str`, et des
/// tableaux recopies a la main dans les tests) : elles derivaient en silence.
/// Ici le compilateur les tient ensemble, et `TOUTES` ne peut plus oublier une
/// variante.
macro_rules! declarer_evenements {
    ($( $(#[$meta:meta])* $variante:ident => $fil:literal, $statut:ident );* $(;)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum EventType {
            $( $(#[$meta])* $variante, )*
        }

        impl EventType {
            /// Toutes les variantes declarees, dans l'ordre de declaration.
            /// Genere par la macro : impossible d'en oublier une.
            pub const TOUTES: &'static [EventType] = &[ $( EventType::$variante, )* ];

            /// Canonical dotted name used on the wire (event_bus `event_type` and
            /// the WebSocket `type` field). These strings are part of the client
            /// contract — keep them stable. New events should be added here so
            /// emitters reference the enum (compile-checked) instead of free-form
            /// strings.
            pub fn as_str(self) -> &'static str {
                match self { $( EventType::$variante => $fil, )* }
            }

            /// Ce que ce depot PRETEND faire de ce nom. Verifie par le test
            /// `chaque_variante_emise_a_au_moins_un_emetteur`.
            pub fn statut(self) -> StatutEmission {
                match self { $( EventType::$variante => StatutEmission::$statut, )* }
            }

            /// Le nom de la variante, pour les messages du garde-fou.
            pub fn nom_variante(self) -> &'static str {
                match self { $( EventType::$variante => stringify!($variante), )* }
            }
        }
    };
}

declarer_evenements! {
    // ── Lecture ────────────────────────────────────────────────────────────
    // Ces noms-la ne passent PAS par le bus d'evenements : `playback/mod.rs`
    // publie un `PlaybackEvent { event: "started", … }` sur le canal de lecture,
    // et `routes/ws.rs` le prefixe par `playback.`. Le garde-fou connait ce
    // second mecanisme.
    PlaybackStarted => "playback.started", Emis;
    PlaybackStopped => "playback.stopped", Emis;
    PlaybackPaused => "playback.paused", Emis;
    PlaybackResumed => "playback.resumed", Emis;
    TrackChanged => "playback.track_changed", Emis;
    VolumeChanged => "playback.volume", Emis;
    /// RESERVE : la file se rafraichit par `playback.queue.*` cote canal de
    /// lecture, jamais sous ce nom-ci.
    QueueChanged => "playback.queue.changed", Reserve;
    SeekChanged => "playback.seek", Emis;
    ShuffleChanged => "playback.shuffle", Emis;
    RepeatChanged => "playback.repeat", Emis;

    // ── Appareils ──────────────────────────────────────────────────────────
    DeviceDiscovered => "device.discovered", Emis;
    /// Un appareil DEJA connu vient d'etre re-resolu avec des informations
    /// differentes (adresse reparee en IPv4, port, nom). `OnboardingView.svelte`
    /// ecoute `device.discovered` OU `device.updated` pour recharger sa liste,
    /// et `App.svelte` recharge sur tout le prefixe `device.` — mais
    /// `MdnsEvent::DeviceUpdated` restait INTERNE : la mise a jour partait sous
    /// le nom `device.discovered`, ce qui annonce une decouverte pour un
    /// appareil deja la (#2870).
    DeviceUpdated => "device.updated", Emis;
    DeviceLost => "device.lost", Emis;

    // ── Bibliotheque ───────────────────────────────────────────────────────
    ScanStarted => "library.scan.started", Emis;
    ScanProgress => "library.scan.progress", Emis;
    ScanComplete => "library.scan.completed", Emis;
    /// Avancement d'une passe d'enrichissement MusicBrainz complete
    /// (`/library/enrich-all`). `SettingsView.svelte` y lit `processed` et
    /// `total` pour sa barre ; sans emetteur elle restait figee sur les chiffres
    /// du sondage a 10 s (#2870).
    EnrichProgress => "library.enrich.progress", Emis;
    /// Une passe d'enrichissement MusicBrainz vient de se terminer. Le client
    /// l'ecoute depuis la v0.8 — `MetadataView.svelte` et `SettingsView.svelte`
    /// y raccrochent leur rafraichissement — mais aucun emetteur ne l'a jamais
    /// produite cote serveur : l'ecran restait fige jusqu'au rechargement de la
    /// page (#2259, fil forum 788).
    EnrichComplete => "library.enrich.completed", Emis;
    /// Avancement de la reprise des pochettes (`POST /library/artwork/rescan`).
    /// `SettingsView.svelte` en lit `current`, `total` et `found` — les trois
    /// champs de `settings.coversProgress` (#2870).
    ArtworkProgress => "library.artwork.progress", Emis;
    /// Fin de la reprise des pochettes. C'est le SEUL evenement qui fasse
    /// retomber `artworkScanning` cote client : sans lui le bouton restait
    /// desactive jusqu'au rechargement de la page (#2870).
    ArtworkComplete => "library.artwork.completed", Emis;
    /// RESERVE : le detail piste par piste n'est jamais annonce ; un scan
    /// resume tout dans `library.scan.*`, et le surveillant dans
    /// `library.updated`.
    LibraryTrackAdded => "library.track.added", Reserve;
    /// RESERVE — voir `LibraryTrackAdded`.
    LibraryTrackRemoved => "library.track.removed", Reserve;
    /// RESERVE — voir `LibraryTrackAdded`.
    LibraryTrackUpdated => "library.track.updated", Reserve;
    /// La bibliotheque a change hors d'un scan — le surveillant de fichiers a
    /// importe ou retire quelque chose. Le client recharge ses listes SANS
    /// afficher de banniere, contrairement a `ScanComplete`.
    LibraryUpdated => "library.updated", Emis;

    // ── Zones et groupes ───────────────────────────────────────────────────
    ZoneCreated => "zone.created", Emis;
    ZoneDeleted => "zone.deleted", Emis;
    ZoneUpdated => "zone.updated", Emis;
    GroupCreated => "group.created", Emis;
    GroupUpdated => "group.updated", Emis;
    GroupDeleted => "group.deleted", Emis;

    // ── Sous-systemes ──────────────────────────────────────────────────────
    /// Le serveur SlimProto n'a pas pu prendre son port TCP (3483 par defaut) :
    /// aucune platine Squeezebox ne pourra se connecter de toute la session.
    /// Cinq testeurs, deux systemes, et rien hors du journal ne le disait
    /// (#2938). Le bind ayant lieu au DEMARRAGE, cet evenement ne trouve
    /// generalement aucun abonne : le porteur qui survit est
    /// `slimproto::etat_ecoute()`, servi par `/system/diagnostics/network`.
    SlimprotoListenFailed => "slimproto.listen_failed", Emis;

    // ── Reserves ───────────────────────────────────────────────────────────
    /// RESERVE : le changement de profil se fait par requete, sans diffusion.
    ProfileSwitched => "profile.switched", Reserve;
    /// RESERVE : le mode soiree expose son etat par `/party/*`, pas par le bus.
    PartyTrackAdded => "party.track_added", Reserve;
    /// RESERVE — voir `PartyTrackAdded`.
    PartyVote => "party.vote", Reserve;
    /// RESERVE : la connexion d'un service repond en HTTP ; le bus ne porte que
    /// `streaming.auth.failed`, qui n'est pas ce nom-ci.
    ServiceConnected => "service.connected", Reserve;
    /// RESERVE — voir `ServiceConnected`.
    ServiceDisconnected => "service.disconnected", Reserve;
    /// RESERVE : les erreurs partent en journal et en reponse HTTP, jamais sur
    /// le bus sous ce nom.
    Error => "error", Reserve;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedEvent {
    pub event_type: EventType,
    pub source: String,
    pub data: EventData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventData {
    PlaybackStarted(PlaybackStartedData),
    PlaybackStopped(PlaybackStoppedData),
    TrackChanged(TrackChangedData),
    VolumeChanged(VolumeChangedData),
    DeviceDiscovered(DeviceDiscoveredData),
    ScanProgress(ScanProgressData),
    Generic(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStartedData {
    pub zone_id: i64,
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStoppedData {
    pub zone_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackChangedData {
    pub zone_id: i64,
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeChangedData {
    pub zone_id: i64,
    pub volume: f64,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceDiscoveredData {
    pub device_id: String,
    pub name: String,
    pub device_type: String,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgressData {
    pub scanned: usize,
    pub total: usize,
    pub current_path: Option<String>,
}

impl TypedEvent {
    pub fn playback_started(
        zone_id: i64,
        title: &str,
        artist: Option<&str>,
        track_id: Option<i64>,
    ) -> Self {
        Self {
            event_type: EventType::PlaybackStarted,
            source: "playback".into(),
            data: EventData::PlaybackStarted(PlaybackStartedData {
                zone_id,
                track_id,
                title: title.to_string(),
                artist_name: artist.map(String::from),
            }),
        }
    }

    pub fn playback_stopped(zone_id: i64) -> Self {
        Self {
            event_type: EventType::PlaybackStopped,
            source: "playback".into(),
            data: EventData::PlaybackStopped(PlaybackStoppedData { zone_id }),
        }
    }

    pub fn track_changed(_zone_id: i64, data: TrackChangedData) -> Self {
        Self {
            event_type: EventType::TrackChanged,
            source: "playback".into(),
            data: EventData::TrackChanged(data),
        }
    }

    pub fn volume_changed(zone_id: i64, volume: f64, muted: bool) -> Self {
        Self {
            event_type: EventType::VolumeChanged,
            source: "playback".into(),
            data: EventData::VolumeChanged(VolumeChangedData {
                zone_id,
                volume,
                muted,
            }),
        }
    }

    pub fn scan_progress(scanned: usize, total: usize, path: Option<&str>) -> Self {
        Self {
            event_type: EventType::ScanProgress,
            source: "scanner".into(),
            data: EventData::ScanProgress(ScanProgressData {
                scanned,
                total,
                current_path: path.map(String::from),
            }),
        }
    }

    pub fn generic(event_type: EventType, source: &str, data: serde_json::Value) -> Self {
        Self {
            event_type,
            source: source.to_string(),
            data: EventData::Generic(data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_serialize() {
        let json = serde_json::to_value(EventType::PlaybackStarted).unwrap();
        assert_eq!(json, "playback_started");
    }

    #[test]
    fn event_type_deserialize() {
        let et: EventType = serde_json::from_str("\"track_changed\"").unwrap();
        assert_eq!(et, EventType::TrackChanged);
    }

    #[test]
    fn playback_started_event() {
        let evt = TypedEvent::playback_started(1, "Time", Some("Pink Floyd"), Some(42));
        assert_eq!(evt.event_type, EventType::PlaybackStarted);
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["event_type"], "playback_started");
    }

    #[test]
    fn volume_changed_event() {
        let evt = TypedEvent::volume_changed(1, 0.75, false);
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["data"]["volume"], 0.75);
        assert_eq!(json["data"]["muted"], false);
    }

    #[test]
    fn scan_progress_event() {
        let evt = TypedEvent::scan_progress(50, 100, Some("/music/album"));
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["data"]["scanned"], 50);
        assert_eq!(json["data"]["total"], 100);
    }

    #[test]
    fn generic_event() {
        let evt = TypedEvent::generic(
            EventType::Error,
            "system",
            serde_json::json!({"message": "disk full"}),
        );
        assert_eq!(evt.event_type, EventType::Error);
    }

    /// `TOUTES` est genere par la macro a partir de la declaration : il ne peut
    /// plus manquer une variante, contrairement au tableau recopie a la main
    /// qu'il remplace (il en oubliait deja quatre — les `Playback*` ajoutees
    /// apres coup).
    #[test]
    fn toutes_couvre_l_enumeration_entiere() {
        assert_eq!(
            EventType::TOUTES.len(),
            37,
            "une variante a ete ajoutee ou retiree : mettre ce compte a jour APRES \
             avoir verifie son statut d'emission"
        );
        for et in EventType::TOUTES {
            assert!(
                !et.as_str().is_empty() && !et.nom_variante().is_empty(),
                "{et:?} sans nom de fil"
            );
        }
    }

    #[test]
    fn as_str_matches_wire_contract() {
        // These strings are consumed by existing clients — they must not drift.
        assert_eq!(EventType::ZoneDeleted.as_str(), "zone.deleted");
        assert_eq!(EventType::ScanComplete.as_str(), "library.scan.completed");
        // Contrat ENTRE DEUX DEPOTS, dans l'autre sens : c'est le CLIENT qui
        // ecoutait cette chaine depuis la v0.8 (`MetadataView.svelte`,
        // `SettingsView.svelte`) et le serveur qui ne la produisait nulle part.
        // La renommer ici, c'est re-eteindre le rafraichissement (#2259).
        assert_eq!(
            EventType::EnrichComplete.as_str(),
            "library.enrich.completed"
        );
        // Contrat ENTRE DEUX DEPOTS : `LibraryView.svelte` ecoute cette chaine
        // exacte. La renommer ici sans toucher au client rendrait le
        // surveillant muet a nouveau, et en silence (#1517).
        assert_eq!(EventType::LibraryUpdated.as_str(), "library.updated");
        // Et ce n'est PAS `ScanComplete` : celui-la fait afficher une banniere
        // « prete », qui n'a aucun sens a chaque fichier depose.
        assert_ne!(
            EventType::LibraryUpdated.as_str(),
            EventType::ScanComplete.as_str()
        );
        assert_eq!(EventType::ScanProgress.as_str(), "library.scan.progress");
        assert_eq!(EventType::DeviceLost.as_str(), "device.lost");
        assert_eq!(EventType::VolumeChanged.as_str(), "playback.volume");

        // Contrat ENTRE DEUX DEPOTS (#2870). Ces quatre chaines-la sont ecrites
        // en toutes lettres dans `tune-web-client` — `SettingsView.svelte` pour
        // les trois premieres, `OnboardingView.svelte` pour la derniere. En
        // renommer une ici, c'est refiger l'ecran sans qu'aucune CI ne le voie.
        assert_eq!(
            EventType::EnrichProgress.as_str(),
            "library.enrich.progress"
        );
        assert_eq!(
            EventType::ArtworkProgress.as_str(),
            "library.artwork.progress"
        );
        assert_eq!(
            EventType::ArtworkComplete.as_str(),
            "library.artwork.completed"
        );
        assert_eq!(EventType::DeviceUpdated.as_str(), "device.updated");
        // Et `device.updated` n'est PAS `device.discovered` : annoncer une
        // decouverte pour un appareil deja connu, c'est ce que faisait le
        // gestionnaire mDNS avant #2870.
        assert_ne!(
            EventType::DeviceUpdated.as_str(),
            EventType::DeviceDiscovered.as_str()
        );
    }

    #[test]
    fn as_str_is_unique_per_variant() {
        let mut names: Vec<&str> = EventType::TOUTES.iter().map(|e| e.as_str()).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate wire name in EventType::as_str");
    }
}

/// Garde-fou du lot 2 de #2870 : **un nom declare emis DOIT avoir un emetteur**.
///
/// L'enumeration annoncait trente noms dont la majorite n'etait produite nulle
/// part. Rien ne pouvait le dire : ni le compilateur (une variante non
/// construite n'est pas une erreur), ni la CI. Ce module lit l'arbre des
/// sources et confronte `EventType::statut()` a la realite, dans les DEUX sens.
///
/// **Regle imposee aux emetteurs** : le nom doit etre le PREMIER argument de
/// l'`emit`, ecrit en toutes lettres (`emit_typed(EventType::ZoneUpdated, …)` ou
/// `emit("library.scan.progress", …)`). Relayer un `EventType` recu en
/// parametre ne compte pas — et c'est voulu : une emission qu'un `git grep` ne
/// retrouve pas est une emission qu'on perdra de vue. Le garde-fou a d'ailleurs
/// attrape ce cas exact pendant l'ecriture de #2870.
#[cfg(test)]
mod garde_fou_emetteurs {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    const MARQUEUR_TEST: &str = "#[cfg(test)]";

    /// Racine de l'espace de travail (le dossier qui contient `tune-core/`).
    fn racine() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tune-core a toujours un parent")
            .to_path_buf()
    }

    /// Tous les `.rs` sous un dossier `src/` d'un membre de l'espace de travail.
    ///
    /// On ecarte `target/` (artefacts) et `event_types.rs` lui-meme : y NOMMER
    /// une variante n'est pas l'emettre, c'est la declarer. Les `tests/`
    /// d'integration sont ecartes aussi — un test qui emet pour s'observer
    /// lui-meme ne fait vivre aucun ecran.
    fn fichiers_sources(dossier: &Path, dans_src: bool, sortie: &mut Vec<PathBuf>) {
        let Ok(entrees) = std::fs::read_dir(dossier) else {
            return;
        };
        for entree in entrees.flatten() {
            let chemin = entree.path();
            let nom = entree.file_name().to_string_lossy().to_string();
            if chemin.is_dir() {
                if nom == "target" || nom == ".git" || nom == "node_modules" {
                    continue;
                }
                fichiers_sources(&chemin, dans_src || nom == "src", sortie);
            } else if dans_src && nom.ends_with(".rs") && nom != "event_types.rs" {
                sortie.push(chemin);
            }
        }
    }

    /// Retire les modules `#[cfg(test)] mod … { … }`.
    ///
    /// **C'est le coeur de la morsure du garde-fou.** Sans cela, un test qui se
    /// contente de NOMMER un evenement pour l'observer passerait pour un
    /// emetteur, et le garde-fou declarerait « emis » un nom que la production
    /// ne produit jamais — exactement le faux garde-fou qu'on veut eviter.
    ///
    /// `#[cfg(test)]` pose sur autre chose qu'un `mod` (un `use`, une fonction
    /// d'aide) est laisse en place : il n'ouvre pas de bloc a sauter.
    fn sans_modules_de_test(source: &str) -> String {
        let mut sortie = String::with_capacity(source.len());
        let mut reste = source;
        while let Some(pos) = reste.find(MARQUEUR_TEST) {
            let apres = &reste[pos + MARQUEUR_TEST.len()..];
            let accolade = apres.find('{');
            let entete = accolade.map(|f| &apres[..f]).unwrap_or("");
            let mots: Vec<&str> = entete.split_whitespace().collect();
            let est_module = accolade.is_some()
                && (mots.first() == Some(&"mod")
                    || (mots.first() == Some(&"pub") && mots.get(1) == Some(&"mod")));
            if !est_module {
                sortie.push_str(&reste[..pos + MARQUEUR_TEST.len()]);
                reste = apres;
                continue;
            }
            sortie.push_str(&reste[..pos]);
            let corps = &apres[accolade.expect("teste juste au-dessus")..];
            let mut profondeur = 0usize;
            let mut fin = corps.len();
            for (idx, c) in corps.char_indices() {
                match c {
                    '{' => profondeur += 1,
                    '}' => {
                        profondeur -= 1;
                        if profondeur == 0 {
                            fin = idx + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            reste = &corps[fin..];
        }
        sortie.push_str(reste);
        sortie
    }

    /// Ce que designe le PREMIER argument d'un `emit…(`.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum PremierArgument {
        /// `emit("library.scan.progress", …)`
        Fil(String),
        /// `emit_typed(EventType::ZoneUpdated, …)`, chemin qualifie compris.
        Variante(String),
    }

    /// Lit le premier argument, en sautant espaces et commentaires de ligne.
    ///
    /// On exige que le nom soit le PREMIER argument, et pas simplement present
    /// quelque part dans l'appel : sans cela, la cle `"error"` d'une charge
    /// utile (`json!({"error": …})`) faisait passer `EventType::Error` pour
    /// emis. C'est precisement le genre de faux positif qui rend un garde-fou
    /// inutile.
    fn premier_argument(apres_parenthese: &str) -> Option<PremierArgument> {
        let mut s = apres_parenthese;
        loop {
            let t = s.trim_start();
            if let Some(r) = t.strip_prefix("//") {
                s = r.split_once('\n').map(|(_, q)| q).unwrap_or("");
                continue;
            }
            s = t;
            break;
        }
        if let Some(r) = s.strip_prefix('"') {
            let fin = r.find('"')?;
            return Some(PremierArgument::Fil(r[..fin].to_string()));
        }
        let idx = s.find("EventType::")?;
        // Ce qui precede doit etre un chemin de module et rien d'autre.
        if !s[..idx]
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
        {
            return None;
        }
        let reste = &s[idx + "EventType::".len()..];
        let fin = reste
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(reste.len());
        Some(PremierArgument::Variante(reste[..fin].to_string()))
    }

    /// Les noms (fil ou variante) reellement emis par ce fichier.
    fn emissions(source: &str) -> BTreeSet<PremierArgument> {
        let mut trouves = BTreeSet::new();
        let hors_tests = sans_modules_de_test(source);
        for motif in ["emit(", "emit_typed("] {
            let mut depuis = 0usize;
            while let Some(rel) = hors_tests[depuis..].find(motif) {
                let debut = depuis + rel;
                depuis = debut + motif.len();
                // `submit(` / `transmit(` ne sont pas des emissions.
                let precedent = hors_tests[..debut].chars().next_back();
                if precedent.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                if let Some(arg) = premier_argument(&hors_tests[depuis..]) {
                    trouves.insert(arg);
                }
            }
        }
        // Second mecanisme : le canal de lecture. `playback/mod.rs` publie
        // `PlaybackEvent { event: "started", … }` et `routes/ws.rs` prefixe par
        // `playback.` — la chaine complete n'apparait donc nulle part.
        for et in EventType::TOUTES {
            if let Some(suffixe) = et.as_str().strip_prefix("playback.")
                && hors_tests.contains(&format!("event: \"{suffixe}\""))
            {
                trouves.insert(PremierArgument::Fil(et.as_str().to_string()));
            }
        }
        trouves
    }

    /// L'inventaire mesure : pour chaque variante, les fichiers qui l'emettent.
    fn inventaire() -> Vec<(EventType, Vec<String>)> {
        let racine = racine();
        let mut fichiers = Vec::new();
        fichiers_sources(&racine, false, &mut fichiers);
        assert!(
            fichiers.len() > 100,
            "l'arbre des sources n'a pas ete trouve depuis {} — le garde-fou \
             passerait au vert en ne lisant RIEN, ce qui est pire que pas de \
             garde-fou du tout (fichiers vus : {})",
            racine.display(),
            fichiers.len()
        );

        let mut par_fichier: Vec<(String, BTreeSet<PremierArgument>)> = Vec::new();
        for f in &fichiers {
            let Ok(source) = std::fs::read_to_string(f) else {
                continue;
            };
            let e = emissions(&source);
            if !e.is_empty() {
                let court = f
                    .strip_prefix(&racine)
                    .unwrap_or(f)
                    .to_string_lossy()
                    .to_string();
                par_fichier.push((court, e));
            }
        }

        EventType::TOUTES
            .iter()
            .map(|et| {
                let attendu_fil = PremierArgument::Fil(et.as_str().to_string());
                let attendu_var = PremierArgument::Variante(et.nom_variante().to_string());
                let sites: Vec<String> = par_fichier
                    .iter()
                    .filter(|(_, e)| e.contains(&attendu_fil) || e.contains(&attendu_var))
                    .map(|(f, _)| f.clone())
                    .collect();
                (*et, sites)
            })
            .collect()
    }

    /// LE test du lot 2 (#2870), dans les deux sens.
    ///
    /// Il tombe si quelqu'un declare un nom `Emis` sans l'emettre, et il tombe
    /// aussi si le DERNIER emetteur d'un nom declare `Emis` disparait : le
    /// garde-fou ne se contente pas de compter, il pointe le fichier manquant.
    #[test]
    fn chaque_variante_emise_a_au_moins_un_emetteur() {
        let mut sans_emetteur = Vec::new();
        for (et, sites) in inventaire() {
            if et.statut() == StatutEmission::Emis && sites.is_empty() {
                sans_emetteur.push(format!(
                    "  {} (« {} ») — declare Emis, AUCUN site d'emission",
                    et.nom_variante(),
                    et.as_str()
                ));
            }
        }
        assert!(
            sans_emetteur.is_empty(),
            "des noms sont annonces au client sans que rien ne les produise \
             (#2870) :\n{}\n\nSoit on branche un emetteur, soit on passe la \
             variante a `Reserve` dans `declarer_evenements!` — mais on ne LAISSE \
             PAS un ecran attendre un evenement qui ne partira jamais.",
            sans_emetteur.join("\n")
        );
    }

    /// Le sens inverse : un nom declare `Reserve` que l'on se met a emettre doit
    /// changer de statut, sinon la table ment a nouveau.
    #[test]
    fn aucune_variante_reservee_n_est_emise() {
        let mut emises_a_tort = Vec::new();
        for (et, sites) in inventaire() {
            if et.statut() == StatutEmission::Reserve && !sites.is_empty() {
                emises_a_tort.push(format!(
                    "  {} (« {} ») — declare Reserve, emis par {}",
                    et.nom_variante(),
                    et.as_str(),
                    sites.join(", ")
                ));
            }
        }
        assert!(
            emises_a_tort.is_empty(),
            "ces noms sont produits alors que la table les dit reserves — passer \
             leur statut a `Emis` dans `declarer_evenements!` :\n{}",
            emises_a_tort.join("\n")
        );
    }

    /// Contre-epreuve du detecteur lui-meme.
    ///
    /// Un garde-fou qui ne sait pas distinguer un emetteur d'une mention est un
    /// garde-fou de facade. On lui donne donc les cas exacts qui l'ont fait
    /// mentir pendant la mise au point.
    #[test]
    fn le_detecteur_ne_confond_pas_mention_et_emission() {
        // 1. Une cle `"error"` DANS la charge utile n'est pas une emission de
        //    `EventType::Error` — c'etait le faux positif d'origine.
        let charge = r#"
            state.event_bus.emit(
                "streaming.auth.failed",
                json!({ "service": &service, "error": &err_msg }),
            );
        "#;
        assert!(!emissions(charge).contains(&PremierArgument::Fil("error".into())));
        assert!(emissions(charge).contains(&PremierArgument::Fil("streaming.auth.failed".into())));

        // 2. Un module de test qui NOMME l'evenement ne l'emet pas.
        let dans_un_test = r#"
            fn production() {}
            #[cfg(test)]
            mod tests {
                #[test]
                fn t() {
                    bus.emit_typed(EventType::PartyVote, json!({}));
                    assert_eq!(ev.event_type, "party.vote");
                }
            }
        "#;
        assert!(emissions(dans_un_test).is_empty());

        // 3. Le chemin qualifie et le `.as_str()` sont bien reconnus — sans
        //    quoi `LibraryUpdated`, emis par `auto_scan.rs` sous cette forme
        //    exacte, passerait pour orphelin.
        let qualifie = r#"
            event_bus.emit(
                tune_core::event_types::EventType::LibraryUpdated.as_str(),
                serde_json::json!({ "source": "watcher" }),
            );
        "#;
        assert!(
            emissions(qualifie).contains(&PremierArgument::Variante("LibraryUpdated".into())),
            "chemin qualifie + .as_str() non reconnu"
        );

        // 4. Un commentaire entre la parenthese et l'argument ne masque rien.
        let commente = r#"
            event_bus.emit_typed(
                // Contrat inter-depots, voir #2870.
                EventType::DeviceUpdated,
                json!({}),
            );
        "#;
        assert!(emissions(commente).contains(&PremierArgument::Variante("DeviceUpdated".into())));

        // 5. `submit(` / `transmit(` ne sont pas des emissions.
        let faux_ami = r#" let _ = form.submit("zone.created"); "#;
        assert!(emissions(faux_ami).is_empty());

        // 6. Le canal de lecture est reconnu par sa forme propre.
        let canal = r#" let _ = tx.send(PlaybackEvent { event: "shuffle".into(), zone_id }); "#;
        assert!(emissions(canal).contains(&PremierArgument::Fil("playback.shuffle".into())));

        // 7. Un `EventType` RELAYE par une variable ne compte pas : le nom
        //    n'apparait plus au site d'emission, et un `git grep` ne le
        //    retrouve plus. Le garde-fou a attrape ce cas pendant l'ecriture de
        //    #2870 — c'est ce qui a fait ecrire les deux variantes en toutes
        //    lettres dans `register_discovered_output`.
        let relaye = r#"
            let annonce = EventType::DeviceUpdated;
            event_bus.emit_typed(annonce, charge);
        "#;
        assert!(
            !emissions(relaye).contains(&PremierArgument::Variante("DeviceUpdated".into())),
            "une emission indirecte ne doit pas passer pour un emetteur nomme"
        );
    }
}
