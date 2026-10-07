//! Témoins de la sauvegarde cloud des personnalisations (#5654).
//!
//! Chacun nomme la propriété qu'il garde ; la contre-épreuve de chacun est
//! écrite dans la PR (sabotage du code, pas du test).

use std::io::Read;
use std::sync::{Arc, Mutex};

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};

use super::*;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

const PHRASE: &str = "une phrase de passe longue";

fn base_neuve() -> Arc<dyn DbBackend> {
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn reglages(b: &Arc<dyn DbBackend>) -> SettingsRepo {
    SettingsRepo::with_backend(b.clone())
}

fn exec(b: &Arc<dyn DbBackend>, sql: &str) {
    b.execute(sql, &[]).unwrap();
}

fn un_entier(b: &Arc<dyn DbBackend>, sql: &str) -> i64 {
    b.query_one(sql, &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

/// Une bibliothèque minimale : un artiste, un album, deux pistes.
fn bibliotheque(b: &Arc<dyn DbBackend>, prefixe: &str) {
    exec(
        b,
        &format!("INSERT INTO artists (name) VALUES ('{prefixe}Artiste')"),
    );
    let ar = un_entier(b, "SELECT MAX(id) FROM artists");
    exec(
        b,
        &format!("INSERT INTO albums (title, artist_id) VALUES ('{prefixe}Album', {ar})"),
    );
    let al = un_entier(b, "SELECT MAX(id) FROM albums");
    for n in 1..=2 {
        exec(
            b,
            &format!(
                "INSERT INTO tracks (title, album_id, artist_id, file_path) \
                 VALUES ('{prefixe}Piste {n}', {al}, {ar}, '/musique/{prefixe}{n}.flac')"
            ),
        );
    }
}

/// Une machine configurée, avec des secrets de toutes les sortes.
fn machine_configuree() -> Arc<dyn DbBackend> {
    let b = base_neuve();
    let s = reglages(&b);
    bibliotheque(&b, "");
    for (k, v) in [
        ("theme", "sombre"),
        ("eq_presets", r#"[{"name":"HE R10P","bands":31}]"#),
        // Secrets, chacun avec un marqueur qui ne doit apparaître nulle part.
        ("discogs_token", "SECRET-discogs"),
        ("auth_tokens_qobuz", r#"{"user_auth_token":"SECRET-qobuz"}"#),
        ("auth_tokens_tidal", "SECRET-tidal"),
        ("mozaik_access_token", "SECRET-acces"),
        ("mozaik_refresh_token", "SECRET-rafraichissement"),
        ("cloud_server_link_token", "SECRET-liaison"),
        ("jwt_secret", "SECRET-jwt"),
        (
            "credentials_vault",
            r#"{"qobuz":{"password":"SECRET-coffre"}}"#,
        ),
        ("config_backup_envelope", "SECRET-enveloppe-des-jetons"),
        ("lastfm_session_key", "SECRET-lastfm"),
        ("license_key", "SECRET-licence"),
        ("server_id", "SECRET-identite-machine"),
        // Un secret IMBRIQUÉ dans une préférence d'interface.
        (
            "ui_preferences:1",
            r#"{"accent":"violet","shortcuts":{"play":"space"},"api_token":"SECRET-imbrique"}"#,
        ),
        // La bibliothèque.
        ("music_dirs", r#"["/musique"]"#),
        ("library_scan_on_startup", "true"),
    ] {
        s.set(k, v).unwrap();
    }
    exec(
        &b,
        "UPDATE profiles SET password_hash = 'SECRET-hash-de-mot-de-passe' WHERE id = 1",
    );
    exec(
        &b,
        "INSERT INTO zones (name, output_type, output_device_id, volume) \
         VALUES ('Salon', 'local', 'local:DAC', 42)",
    );
    exec(
        &b,
        "INSERT INTO playlists (name, description) VALUES ('Du soir', 'calme')",
    );
    let pl = un_entier(&b, "SELECT MAX(id) FROM playlists");
    let t = un_entier(&b, "SELECT MIN(id) FROM tracks");
    exec(
        &b,
        &format!(
            "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES ({pl}, {t}, 0)"
        ),
    );
    let al = un_entier(&b, "SELECT MIN(id) FROM albums");
    exec(
        &b,
        &format!(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', {al})"
        ),
    );
    exec(
        &b,
        "INSERT INTO radio_stations (name, url, is_favorite) VALUES ('Radio X', 'http://radio.example/x', 1)",
    );
    b
}

// ── Aucun secret ────────────────────────────────────────────────────

/// Le contenu, AVANT chiffrement, ne porte aucun secret : ni jeton de
/// service, ni jeton de compte, ni clé, ni hash de mot de passe, ni l'identité
/// de la machine, ni un secret imbriqué dans une préférence.
#[test]
fn le_contenu_ne_porte_aucun_secret() {
    let b = machine_configuree();
    let contenu = construire(&b).unwrap();
    let texte = contenu.to_string();
    assert!(
        !texte.contains("SECRET-"),
        "un secret part dans la sauvegarde : {}",
        texte
            .match_indices("SECRET-")
            .map(|(i, _)| &texte[i..(i + 40).min(texte.len())])
            .collect::<Vec<_>>()
            .join(" | ")
    );
    // …et ce qui doit partir part.
    assert_eq!(contenu["settings"]["theme"], json!("sombre"));
    assert!(contenu["settings"].get("eq_presets").is_some());
    assert_eq!(
        contenu["profiles"][0]["prefs"]["ui_preferences"]["accent"],
        json!("violet")
    );
    assert_eq!(
        contenu["profiles"][0]["prefs"]["ui_preferences"]["shortcuts"]["play"],
        json!("space")
    );
    assert_eq!(contenu["zones"][0]["name"], json!("Salon"));
    assert_eq!(contenu["playlists"][0]["name"], json!("Du soir"));
    assert_eq!(contenu["favorites"][0]["name"], json!("Album"));
    assert!(
        contenu["radio_stations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == json!("Radio X"))
    );
}

/// Ni la bibliothèque ni ce qui la pilote ne partent.
#[test]
fn la_bibliotheque_ne_part_pas() {
    let b = machine_configuree();
    let contenu = construire(&b).unwrap();
    assert!(contenu["settings"].get("music_dirs").is_none());
    assert!(contenu["settings"].get("library_scan_on_startup").is_none());
    assert!(contenu.get("tracks").is_none());
    assert!(contenu.get("albums").is_none());
}

#[test]
fn les_cles_de_la_machine_sont_reconnues() {
    for k in [
        "discogs_token",
        "mozaik_user",
        "cloud_library_last_sync",
        "config_backup_envelope",
        "sauvegarde_cloud_cle_secrete",
        "sauvegarde_cloud_trousseau",
        "server_id",
        "music_dirs",
        "library_scan_on_startup",
        "crossfade_enabled:7",
    ] {
        assert!(reste_sur_la_machine(k), "{k} devrait rester sur la machine");
    }
    for k in [
        "theme",
        "eq_presets",
        "ui_preferences:3",
        "default_zone_id",
        "zone_groups",
    ] {
        assert!(!reste_sur_la_machine(k), "{k} devrait voyager");
    }
}

// ── Chiffrement ─────────────────────────────────────────────────────

/// Le texte déposé au site est chiffré : ni le clair, ni même le clair
/// compressé n'y sont lisibles.
#[test]
fn le_blob_est_chiffre() {
    let b = machine_configuree();
    let s = reglages(&b);
    creer_la_cle(&s, PHRASE).unwrap();
    let cle = cle_locale(&s).unwrap().unwrap();
    let contenu = construire(&b).unwrap();
    let blob = chiffrer(&cle, &contenu).unwrap();

    for clair in ["sombre", "Du soir", "Salon", "HE R10P", "violet"] {
        assert!(!blob.contains(clair), "le blob porte en clair : {clair}");
    }
    let v: Value = serde_json::from_str(&blob).unwrap();
    let chiffre = hex_decode(v["envelope"]["ciphertext"].as_str().unwrap()).unwrap();
    let mut sortie = Vec::new();
    let lu = flate2::read::GzDecoder::new(&chiffre[..]).read_to_end(&mut sortie);
    assert!(
        lu.is_err(),
        "le blob n'est que compressé : {} octets se décompressent sans clé",
        sortie.len()
    );
    // Aucune clé en clair non plus.
    let hex_dek = s.get(CLE_SECRETE).unwrap().unwrap();
    assert!(
        !blob.contains(&hex_dek),
        "la clé de données part avec le blob"
    );

    // La clé locale l'ouvre.
    let (rouvert, _) = dechiffrer(&blob, Some(&cle), None).unwrap();
    assert_eq!(rouvert, contenu);
}

/// Sur une machine neuve : sans secret, rien ; avec un mauvais, rien ; avec
/// la phrase de passe ou la clé de secours, le contenu.
#[test]
fn une_machine_neuve_ouvre_avec_la_phrase_ou_la_cle_de_secours() {
    let b = machine_configuree();
    let s = reglages(&b);
    let (_, secours) = creer_la_cle(&s, PHRASE).unwrap();
    let cle = cle_locale(&s).unwrap().unwrap();
    let contenu = construire(&b).unwrap();
    let blob = chiffrer(&cle, &contenu).unwrap();

    // Une AUTRE clé locale (autre machine) n'ouvre pas.
    let neuve = base_neuve();
    let sn = reglages(&neuve);
    creer_la_cle(&sn, "une autre phrase de passe").unwrap();
    let autre = cle_locale(&sn).unwrap().unwrap();
    assert_eq!(
        dechiffrer(&blob, Some(&autre), None).err(),
        Some(ErreurOuverture::SecretRequis)
    );
    assert_eq!(
        dechiffrer(&blob, None, Some("pas la bonne phrase")).err(),
        Some(ErreurOuverture::MauvaisSecret)
    );
    let (c1, _) = dechiffrer(&blob, None, Some(PHRASE)).unwrap();
    assert_eq!(c1, contenu);
    let (c2, _) = dechiffrer(&blob, None, Some(secours.display())).unwrap();
    assert_eq!(c2, contenu);
}

#[test]
fn une_alteration_est_detectee() {
    let b = machine_configuree();
    let s = reglages(&b);
    creer_la_cle(&s, PHRASE).unwrap();
    let cle = cle_locale(&s).unwrap().unwrap();
    let blob = chiffrer(&cle, &construire(&b).unwrap()).unwrap();
    let mut v: Value = serde_json::from_str(&blob).unwrap();
    let mut octets = hex_decode(v["envelope"]["ciphertext"].as_str().unwrap()).unwrap();
    let n = octets.len() - 1;
    octets[n] ^= 1;
    v["envelope"]["ciphertext"] = json!(hex_encode(&octets));
    assert!(matches!(
        dechiffrer(&v.to_string(), Some(&cle), None),
        Err(ErreurOuverture::Illisible(_))
    ));
}

#[test]
fn la_phrase_de_passe_est_trop_courte_ou_la_cle_existe_deja() {
    let b = base_neuve();
    let s = reglages(&b);
    assert!(creer_la_cle(&s, "court").is_err());
    creer_la_cle(&s, PHRASE).unwrap();
    assert!(
        creer_la_cle(&s, PHRASE).is_err(),
        "la clé a été remplacée en silence"
    );
    // La clé locale elle-même ne part dans AUCUN export.
    let export = crate::config_export::exporter(&b, false)
        .unwrap()
        .to_string();
    let dek = s.get(CLE_SECRETE).unwrap().unwrap();
    assert!(
        !export.contains(&dek),
        "la clé de données sort par l'export gratuit"
    );
}

/// Après une ouverture par secret, la machine neuve adopte la clé : ses
/// sauvegardes suivantes s'ouvrent avec la MÊME phrase de passe.
#[test]
fn la_cle_est_adoptee_apres_une_ouverture_par_secret() {
    let b = machine_configuree();
    let s = reglages(&b);
    creer_la_cle(&s, PHRASE).unwrap();
    let cle = cle_locale(&s).unwrap().unwrap();
    let blob = chiffrer(&cle, &construire(&b).unwrap()).unwrap();

    let neuve = base_neuve();
    let sn = reglages(&neuve);
    let (_, retrouvee) = dechiffrer(&blob, None, Some(PHRASE)).unwrap();
    assert!(adopter(&sn, &retrouvee.unwrap()).unwrap());
    let adoptee = cle_locale(&sn).unwrap().unwrap();
    assert_eq!(adoptee.key_id, cle.key_id);
    let suivant = chiffrer(&adoptee, &construire(&neuve).unwrap()).unwrap();
    assert!(dechiffrer(&suivant, None, Some(PHRASE)).is_ok());
}

// ── Restauration ────────────────────────────────────────────────────

/// Empreinte de la bibliothèque : nombres et contenu des trois tables.
fn empreinte_bibliotheque(b: &Arc<dyn DbBackend>) -> String {
    let mut sortie = String::new();
    for sql in [
        "SELECT id, name FROM artists ORDER BY id",
        "SELECT id, title, artist_id FROM albums ORDER BY id",
        "SELECT id, title, album_id, artist_id, file_path FROM tracks ORDER BY id",
    ] {
        for l in b.query_many(sql, &[]).unwrap() {
            sortie.push_str(&format!("{l:?}\n"));
        }
    }
    sortie
}

/// Une restauration — même en REMPLACER, même d'un instantané piégé qui
/// porterait des dossiers de musique et un scan au démarrage — ne touche pas
/// la bibliothèque de la machine d'arrivée.
#[test]
fn une_restauration_ne_touche_pas_la_bibliotheque() {
    let source = machine_configuree();
    let mut contenu = construire(&source).unwrap();
    contenu["settings"]["music_dirs"] = json!(["/ailleurs"]);
    contenu["settings"]["library_scan_on_startup"] = json!("false");
    contenu["settings"]["scan_interval_hours"] = json!("1");

    let cible = base_neuve();
    bibliotheque(&cible, "Autre ");
    let sc = reglages(&cible);
    sc.set("music_dirs", r#"["/musique-ici"]"#).unwrap();
    sc.set("library_scan_on_startup", "true").unwrap();
    let avant = empreinte_bibliotheque(&cible);

    restaurer(&cible, &contenu, Mode::Replace).unwrap();

    assert_eq!(
        empreinte_bibliotheque(&cible),
        avant,
        "la restauration a modifié les tables de la bibliothèque"
    );
    assert_eq!(
        sc.get("music_dirs").unwrap().as_deref(),
        Some(r#"["/musique-ici"]"#),
        "la restauration a réécrit les dossiers de musique"
    );
    assert_eq!(
        sc.get("library_scan_on_startup").unwrap().as_deref(),
        Some("true"),
        "la restauration a réécrit un réglage de scan"
    );
    assert!(sc.get("scan_interval_hours").unwrap().is_none());
}

/// Un instantané piégé ne réécrit pas non plus un secret de la machine.
#[test]
fn une_restauration_n_ecrit_aucun_secret() {
    let source = machine_configuree();
    let mut contenu = construire(&source).unwrap();
    contenu["settings"]["jwt_secret"] = json!("PIEGE");
    contenu["settings"]["mozaik_access_token"] = json!("PIEGE");
    contenu["settings"]["sauvegarde_cloud_cle_secrete"] = json!("PIEGE");
    let cible = base_neuve();
    let sc = reglages(&cible);
    sc.set("jwt_secret", "le-vrai").unwrap();
    restaurer(&cible, &contenu, Mode::Replace).unwrap();
    assert_eq!(sc.get("jwt_secret").unwrap().as_deref(), Some("le-vrai"));
    assert!(sc.get("mozaik_access_token").unwrap().is_none());
    assert!(sc.get(CLE_SECRETE).unwrap().is_none());
}

/// Machine neuve, mode remplacer : tout revient — réglages, zone, profil et
/// ses préférences, playlist, favori retrouvé par identité, radio.
#[test]
fn une_machine_neuve_reprend_ses_personnalisations() {
    let source = machine_configuree();
    exec(
        &source,
        "INSERT INTO profiles (username, display_name) VALUES ('ana', 'Ana')",
    );
    let pid = un_entier(&source, "SELECT id FROM profiles WHERE username = 'ana'");
    reglages(&source)
        .set(&format!("ui_preferences:{pid}"), r#"{"accent":"vert"}"#)
        .unwrap();
    let contenu = construire(&source).unwrap();

    let cible = base_neuve();
    bibliotheque(&cible, "");
    // Un profil intermédiaire : « ana » n'aura pas le même numéro ici.
    exec(&cible, "INSERT INTO profiles (username) VALUES ('zoe')");
    let r = restaurer(&cible, &contenu, Mode::Replace).unwrap();
    let sc = reglages(&cible);

    assert_eq!(sc.get("theme").unwrap().as_deref(), Some("sombre"));
    assert!(sc.get("eq_presets").unwrap().unwrap().contains("HE R10P"));
    assert_eq!(r.zones_created, 1);
    assert_eq!(
        un_entier(
            &cible,
            "SELECT COUNT(*) FROM zones WHERE output_device_id = 'local:DAC'"
        ),
        1
    );
    assert_eq!(r.profiles_created, 1);
    let pid_ici = un_entier(&cible, "SELECT id FROM profiles WHERE username = 'ana'");
    assert_ne!(
        pid_ici, pid,
        "le témoin ne prouve rien si les numéros coïncident"
    );
    assert_eq!(
        sc.get(&format!("ui_preferences:{pid_ici}"))
            .unwrap()
            .as_deref(),
        Some(r#"{"accent":"vert"}"#)
    );
    assert_eq!(
        un_entier(
            &cible,
            "SELECT COUNT(*) FROM profiles WHERE username = 'ana' AND is_admin = 1"
        ),
        0,
        "un profil restauré ne doit jamais être administrateur"
    );
    assert_eq!(r.playlists_restored, 1);
    assert_eq!(
        un_entier(
            &cible,
            "SELECT COUNT(*) FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
             WHERE p.name = 'Du soir'"
        ),
        1
    );
    assert_eq!(r.favorites_restored, 1);
    assert_eq!(r.radios_restored, 1);
}

/// Fusionner garde l'existant ; remplacer fait gagner l'instantané. Ni l'un
/// ni l'autre ne supprime.
#[test]
fn fusionner_garde_l_existant_remplacer_le_remplace() {
    let source = machine_configuree();
    let contenu = construire(&source).unwrap();

    let cible = base_neuve();
    let sc = reglages(&cible);
    sc.set("theme", "clair").unwrap();
    sc.set("reglage_local", "garde").unwrap();
    exec(&cible, "INSERT INTO playlists (name) VALUES ('Du soir')");
    let pl = un_entier(&cible, "SELECT id FROM playlists WHERE name = 'Du soir'");

    let r = restaurer(&cible, &contenu, Mode::Merge).unwrap();
    assert_eq!(sc.get("theme").unwrap().as_deref(), Some("clair"));
    assert!(
        sc.get("eq_presets").unwrap().is_some(),
        "fusionner n'ajoute pas ce qui manque"
    );
    assert_eq!(r.playlists_replaced, 0);

    restaurer(&cible, &contenu, Mode::Replace).unwrap();
    assert_eq!(sc.get("theme").unwrap().as_deref(), Some("sombre"));
    assert_eq!(sc.get("reglage_local").unwrap().as_deref(), Some("garde"));
    assert_eq!(
        un_entier(&cible, "SELECT id FROM playlists WHERE name = 'Du soir'"),
        pl,
        "remplacer une playlist doit garder son identifiant"
    );
}

// ── Planification et rotation ───────────────────────────────────────

fn t(h: u32, m: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 7, h, m, 0).unwrap()
}

#[test]
fn rien_ne_part_si_rien_n_a_change() {
    let etat = Etat {
        derniere_empreinte: Some("a".into()),
        ..Default::default()
    };
    assert_eq!(decider(&etat, "a", t(12, 0)), Decision::Rien);
}

/// Le délai d'attente : un changement attend dix minutes de stabilité.
#[test]
fn un_changement_attend_le_delai() {
    let mut etat = Etat {
        derniere_empreinte: Some("a".into()),
        ..Default::default()
    };
    assert_eq!(decider(&etat, "b", t(12, 0)), Decision::Noter);
    etat.attente_empreinte = Some("b".into());
    etat.attente_depuis = Some(t(12, 0));
    etat.attente_premiere = Some(t(12, 0));
    assert_eq!(decider(&etat, "b", t(12, 5)), Decision::Attendre);
    assert_eq!(
        decider(&etat, "b", t(12, 10)),
        Decision::Envoyer { remplace: None }
    );
    // Encore changé avant la fin du délai : on repart pour un tour…
    assert_eq!(decider(&etat, "c", t(12, 8)), Decision::Noter);
    // …mais pas indéfiniment.
    assert_eq!(
        decider(&etat, "c", t(13, 0)),
        Decision::Envoyer { remplace: None }
    );
}

/// La rotation côté serveur : dans la journée, l'instantané de ce serveur est
/// REMPLACÉ ; au-delà, un nouveau est créé (et le site élague le plus ancien).
#[test]
fn un_instantane_par_jour_au_plus() {
    let mut etat = Etat {
        derniere_empreinte: Some("a".into()),
        derniere: Some(t(8, 0)),
        dernier_id: Some(41),
        attente_empreinte: Some("b".into()),
        attente_depuis: Some(t(12, 0)),
        attente_premiere: Some(t(12, 0)),
    };
    assert_eq!(
        decider(&etat, "b", t(12, 30)),
        Decision::Envoyer { remplace: Some(41) },
        "un second changement le même jour doit remplacer l'instantané du jour"
    );
    etat.derniere = Some(t(8, 0) - Duration::hours(25));
    assert_eq!(
        decider(&etat, "b", t(12, 30)),
        Decision::Envoyer { remplace: None },
        "le lendemain, un nouvel instantané"
    );
}

// ── Bout en bout, contre un faux site ──────────────────────────────

#[derive(Default)]
struct FauxSite {
    suivant: i64,
    /// (meta, payload), du plus ancien au plus récent.
    depots: Vec<(Value, String)>,
    corps_recus: Vec<Value>,
}

async fn faux_site() -> (String, Arc<Mutex<FauxSite>>) {
    use axum::extract::{Path, State};
    use axum::routing::get;
    use axum::{Json, Router};

    type S = Arc<Mutex<FauxSite>>;
    async fn liste(State(s): State<S>) -> Json<Value> {
        let s = s.lock().unwrap();
        let metas: Vec<Value> = s.depots.iter().rev().map(|(m, _)| m.clone()).collect();
        Json(json!({"backups": metas, "max": 3, "max_bytes": 4194304}))
    }
    async fn depot(
        State(s): State<S>,
        Json(corps): Json<Value>,
    ) -> (axum::http::StatusCode, Json<Value>) {
        let mut s = s.lock().unwrap();
        s.corps_recus.push(corps.clone());
        if let Some(r) = corps["replaces"].as_i64() {
            s.depots.retain(|(m, _)| m["id"].as_i64() != Some(r));
        }
        s.suivant += 1;
        let payload = corps["payload"].as_str().unwrap().to_string();
        let meta = json!({
            "id": s.suivant, "server_id": corps["server_id"], "server_label": corps["server_label"],
            "key_id": corps["key_id"], "format_version": corps["format_version"],
            "size_bytes": payload.len(), "created_at": "2026-10-07T12:00:00Z",
        });
        s.depots.push((meta.clone(), payload));
        while s.depots.len() > 3 {
            s.depots.remove(0);
        }
        (
            axum::http::StatusCode::CREATED,
            Json(json!({"backup": meta, "pruned": []})),
        )
    }
    async fn un(State(s): State<S>, Path(id): Path<i64>) -> Json<Value> {
        let s = s.lock().unwrap();
        let (m, p) = s
            .depots
            .iter()
            .find(|(m, _)| m["id"].as_i64() == Some(id))
            .unwrap();
        let mut b = m.clone();
        b["payload"] = json!(p);
        Json(json!({"backup": b}))
    }
    let etat: S = Arc::new(Mutex::new(FauxSite::default()));
    let app = Router::new()
        .route("/api/v1/config-backups", get(liste).post(depot))
        .route("/api/v1/config-backups/{id}", get(un))
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}"), etat)
}

fn relier(b: &Arc<dyn DbBackend>, base: &str, server_id: &str) {
    let s = reglages(b);
    s.set("mozaik_base_url", base).unwrap();
    s.set("mozaik_access_token", "jeton-de-test").unwrap();
    s.set("server_id", server_id).unwrap();
    s.set(
        crate::cloud::library_sync::CLE_JETON_DE_LIAISON,
        "liaison-de-test",
    )
    .unwrap();
}

/// La passe automatique dépose un blob chiffré, ne renvoie rien d'inchangé,
/// remplace dans la journée ; une machine neuve reliée au compte le liste,
/// l'ouvre avec la phrase de passe et le restaure.
#[tokio::test]
async fn bout_en_bout_depot_rotation_et_reprise() {
    let (base, site) = faux_site().await;
    let http = reqwest::Client::new();
    let b = machine_configuree();
    relier(&b, &base, "serveur-a");
    let s = reglages(&b);
    creer_la_cle(&s, PHRASE).unwrap();
    activer(&s, true).unwrap();

    // Premier tour : changement noté, rien ne part encore.
    assert_eq!(
        passe(&b, &http, false, t(12, 0)).await,
        Issue::Rien("pending")
    );
    assert_eq!(
        passe(&b, &http, false, t(12, 5)).await,
        Issue::Rien("pending")
    );
    let Issue::Deposee(m1) = passe(&b, &http, false, t(12, 10)).await else {
        panic!("rien n'a été déposé après le délai d'attente");
    };
    // Inchangé : rien ne repart.
    assert_eq!(
        passe(&b, &http, false, t(12, 15)).await,
        Issue::Rien("unchanged")
    );

    {
        let site = site.lock().unwrap();
        let corps = &site.corps_recus[0];
        let texte = corps.to_string();
        assert!(!texte.contains("SECRET-"), "un secret est parti au site");
        assert!(!texte.contains("sombre"), "le site reçoit du clair");
        assert_eq!(corps["server_id"], json!("serveur-a"));
    }

    // Un changement le même jour REMPLACE l'instantané du jour.
    s.set("theme", "clair").unwrap();
    passe(&b, &http, false, t(13, 0)).await;
    let Issue::Deposee(m2) = passe(&b, &http, false, t(13, 10)).await else {
        panic!("le second changement n'est pas parti");
    };
    {
        let site = site.lock().unwrap();
        assert_eq!(site.corps_recus[1]["replaces"], json!(m1.id));
        assert_eq!(
            site.depots.len(),
            1,
            "la journée doit tenir en un instantané"
        );
    }

    // Machine neuve, même compte.
    let neuve = base_neuve();
    bibliotheque(&neuve, "");
    relier(&neuve, &base, "serveur-b");
    let sn = reglages(&neuve);
    let liste = lister(&sn, &http).await.unwrap();
    assert_eq!(liste.len(), 1);
    assert_eq!(liste[0].id, m2.id);
    let (_, blob) = telecharger(&sn, &http, m2.id).await.unwrap();
    assert_eq!(
        dechiffrer(&blob, None, None).err(),
        Some(ErreurOuverture::SecretRequis)
    );
    let (contenu, retrouvee) = dechiffrer(&blob, None, Some(PHRASE)).unwrap();
    restaurer(&neuve, &contenu, Mode::Replace).unwrap();
    assert!(adopter(&sn, &retrouvee.unwrap()).unwrap());
    assert_eq!(sn.get("theme").unwrap().as_deref(), Some("clair"));
    // Sa propre sauvegarde manuelle s'ouvre avec la même phrase.
    let Issue::Deposee(m3) = passe(&neuve, &http, true, t(14, 0)).await else {
        panic!("la machine neuve ne sauvegarde pas");
    };
    let (_, blob3) = telecharger(&sn, &http, m3.id).await.unwrap();
    assert!(dechiffrer(&blob3, None, Some(PHRASE)).is_ok());
}

/// Désactivée, ou sans compte relié : la passe automatique n'envoie rien.
#[tokio::test]
async fn rien_ne_part_sans_activation_ni_compte() {
    let http = reqwest::Client::new();
    let b = machine_configuree();
    let s = reglages(&b);
    creer_la_cle(&s, PHRASE).unwrap();
    assert_eq!(
        passe(&b, &http, false, t(12, 0)).await,
        Issue::Rien("disabled")
    );
    activer(&s, true).unwrap();
    s.delete("mozaik_access_token").unwrap();
    assert_eq!(
        passe(&b, &http, false, t(12, 0)).await,
        Issue::Rien("account_not_linked")
    );
}
