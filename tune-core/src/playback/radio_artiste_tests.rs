//! Témoins de la radio artiste (#5395), hors réseau : des sources factices
//! pour la composition, une base en mémoire et un service simulé pour les
//! sources réelles et la reprise.

use super::*;
use crate::db::sqlite::SqliteDb;
use crate::error::TuneError;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamUrl,
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn piste(id: &str, artiste: &str, titre: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: None,
        album_id: None,
        duration_ms: 200_000,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

fn titre_de(source: &str, artiste: &str, n: usize) -> Candidat {
    Candidat::Service {
        source: source.into(),
        piste: piste(
            &format!("{source}-{artiste}-{n}"),
            artiste,
            &format!("{artiste} titre {n}"),
        ),
    }
}

/// Une source factice : des voisins pour la graine, `n` titres par artiste
/// qu'elle connaît. Compte les demandes de voisins.
struct Factice {
    nom: &'static str,
    voisins: Vec<String>,
    connus: Vec<String>,
    titres_par_artiste: usize,
    demandes_de_voisins: Arc<AtomicUsize>,
}

impl Factice {
    fn new(nom: &'static str, voisins: &[&str], connus: &[&str], n: usize) -> Self {
        Factice {
            nom,
            voisins: voisins.iter().map(|s| s.to_string()).collect(),
            connus: connus.iter().map(|s| s.to_string()).collect(),
            titres_par_artiste: n,
            demandes_de_voisins: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl SourceRadio for Factice {
    fn nom(&self) -> String {
        self.nom.into()
    }
    async fn artistes_similaires(&self, _artiste: &str, max: usize) -> Vec<String> {
        self.demandes_de_voisins.fetch_add(1, Ordering::SeqCst);
        self.voisins.iter().take(max).cloned().collect()
    }
    async fn titres_de(&self, artiste: &str, max: usize) -> Vec<Candidat> {
        if !self.connus.iter().any(|c| c.eq_ignore_ascii_case(artiste)) {
            return Vec::new();
        }
        (0..self.titres_par_artiste.min(max))
            .map(|n| titre_de(self.nom, artiste, n))
            .collect()
    }
}

fn voisins(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("Voisin {i}")).collect()
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn source_de(c: &Candidat) -> &str {
    match c {
        Candidat::Local { .. } => "local",
        Candidat::Service { source, .. } => source,
    }
}

async fn lot(sources: Vec<Box<dyn SourceRadio>>, graine: u64) -> Lot {
    composer_lot(
        "Graine",
        &sources,
        TAILLE_LOT,
        &HashSet::new(),
        None,
        true,
        &mut Alea::fixe(graine),
    )
    .await
}

// ── Proportion ───────────────────────────────────────────────────────────

#[tokio::test]
async fn un_lot_porte_environ_vingt_pour_cent_de_l_artiste_de_depart() {
    let v = voisins(12);
    let mut connus = refs(&v);
    connus.push("Graine");
    for graine in 1..=20u64 {
        let l = lot(
            vec![Box::new(Factice::new("qobuz", &refs(&v), &connus, 10))],
            graine,
        )
        .await;
        assert_eq!(TAILLE_LOT, 50, "décision du 29/09 : 50 titres par lot");
        assert_eq!(l.candidats.len(), TAILLE_LOT, "graine {graine}");
        let de_la_graine = l
            .candidats
            .iter()
            .filter(|c| c.artiste() == "Graine")
            .count();
        assert_eq!(
            de_la_graine, 10,
            "graine {graine} : 10 titres sur 50 (20 %) doivent venir de l'artiste de départ"
        );
        assert_eq!(l.titres_graine, 10);
        assert_eq!(
            l.candidats[0].artiste(),
            "Graine",
            "le premier lot s'ouvre sur l'artiste demandé"
        );
    }
}

// ── Ordre des sources ────────────────────────────────────────────────────

#[tokio::test]
async fn le_service_de_la_fiche_passe_d_abord_et_les_autres_ne_font_que_completer() {
    // La fiche a assez de voisins : le second service n'est même pas
    // interrogé sur les siens, et les titres viennent de la fiche.
    let v = voisins(30);
    let mut connus = refs(&v);
    connus.push("Graine");
    let fiche = Factice::new("qobuz", &refs(&v), &connus, 12);
    let autre = Factice::new("tidal", &["Autre voisin"], &connus, 12);
    let demandes_autre = autre.demandes_de_voisins.clone();
    let l = lot(vec![Box::new(fiche), Box::new(autre)], 7).await;
    assert_eq!(demandes_autre.load(Ordering::SeqCst), 0);
    assert!(l.candidats.iter().all(|c| source_de(c) == "qobuz"));
    assert_eq!(
        l.voisins_par_source,
        vec![("qobuz".to_string(), 30)],
        "une seule source consultée pour les voisins"
    );
}

#[tokio::test]
async fn un_voisin_absent_de_la_fiche_est_cherche_dans_le_service_suivant() {
    // La fiche ne connaît que 2 voisins sur 3 ; le troisième n'existe que
    // chez le second service.
    let fiche = Factice::new("qobuz", &["A", "B", "C"], &["Graine", "A", "B"], 4);
    let autre = Factice::new("tidal", &[], &["Graine", "A", "B", "C"], 4);
    let l = lot(vec![Box::new(fiche), Box::new(autre)], 3).await;
    // (Les titres de la graine peuvent venir des deux : il en faut plus que
    // la fiche n'en a pour combler les voisins trop courts.)
    for c in l.candidats.iter().filter(|c| c.artiste() != "Graine") {
        let attendu = if c.artiste() == "C" { "tidal" } else { "qobuz" };
        assert_eq!(
            source_de(c),
            attendu,
            "{} vient de {}",
            c.titre(),
            source_de(c)
        );
    }
    assert!(l.candidats.iter().any(|c| c.artiste() == "C"));
}

// ── Service sans similaires ──────────────────────────────────────────────

#[tokio::test]
async fn un_service_sans_similaires_est_complete_par_les_sources_suivantes() {
    // Le service de la fiche ne rend aucun voisin (Spotify, YouTube…) : les
    // voisins viennent de la source suivante, les titres restent ceux de la
    // fiche quand elle les a.
    let fiche = Factice::new("spotify", &[], &["Graine", "X", "Y"], 5);
    let suivante = Factice::new("local", &["X", "Y"], &["X", "Y"], 5);
    let l = lot(vec![Box::new(fiche), Box::new(suivante)], 11).await;
    assert_eq!(
        l.voisins_par_source,
        vec![("spotify".to_string(), 0), ("local".to_string(), 2)]
    );
    assert!(l.candidats.iter().any(|c| c.artiste() == "X"));
    assert!(l.candidats.iter().any(|c| c.artiste() == "Y"));
    assert!(l.candidats.iter().all(|c| source_de(c) == "spotify"));
}

#[tokio::test]
async fn sans_aucun_voisin_la_radio_joue_l_artiste_seul_plutot_que_rien() {
    let fiche = Factice::new("deezer", &[], &["Graine"], 8);
    let l = lot(vec![Box::new(fiche)], 5).await;
    assert_eq!(l.candidats.len(), 8);
    assert!(l.candidats.iter().all(|c| c.artiste() == "Graine"));
}

// ── Doublons et enchaînement ─────────────────────────────────────────────

#[tokio::test]
async fn le_meme_morceau_sur_deux_services_n_entre_qu_une_fois() {
    // Deux services rendent les MÊMES morceaux (même artiste, même titre,
    // identifiants différents) ; un titre « (Remastered) » est le même.
    struct Doublons;
    #[async_trait::async_trait]
    impl SourceRadio for Doublons {
        fn nom(&self) -> String {
            "tidal".into()
        }
        async fn artistes_similaires(&self, _a: &str, _m: usize) -> Vec<String> {
            vec!["A".into(), "B".into()]
        }
        async fn titres_de(&self, artiste: &str, _m: usize) -> Vec<Candidat> {
            (0..3)
                .map(|n| Candidat::Service {
                    source: "tidal".into(),
                    piste: piste(
                        &format!("t{artiste}{n}"),
                        artiste,
                        &format!("{artiste} titre {n} (Remastered 2011)"),
                    ),
                })
                .collect()
        }
    }
    let fiche = Factice::new("qobuz", &["A", "B"], &["Graine", "A", "B"], 3);
    let sources: Vec<Box<dyn SourceRadio>> = vec![Box::new(fiche), Box::new(Doublons)];
    let l = composer_lot(
        "Graine",
        &sources,
        TAILLE_LOT,
        &HashSet::new(),
        None,
        true,
        &mut Alea::fixe(9),
    )
    .await;
    let cles: Vec<String> = l.candidats.iter().map(Candidat::cle).collect();
    let uniques: HashSet<&String> = cles.iter().collect();
    assert_eq!(cles.len(), uniques.len(), "doublon dans {cles:?}");
    assert!(!l.candidats.is_empty());
}

#[tokio::test]
async fn deux_titres_du_meme_artiste_ne_se_suivent_pas() {
    let v = voisins(6);
    let mut connus = refs(&v);
    connus.push("Graine");
    for graine in 1..=30u64 {
        let l = composer_lot(
            "Graine",
            &[Box::new(Factice::new("qobuz", &refs(&v), &connus, 6)) as Box<dyn SourceRadio>],
            TAILLE_LOT,
            &HashSet::new(),
            Some("Voisin 0"),
            false,
            &mut Alea::fixe(graine),
        )
        .await;
        assert_ne!(
            l.candidats[0].artiste(),
            "Voisin 0",
            "graine {graine} : le lot ne répète pas le dernier artiste du lot précédent"
        );
        for paire in l.candidats.windows(2) {
            assert_ne!(
                paire[0].artiste(),
                paire[1].artiste(),
                "graine {graine} : deux titres de {} d'affilée",
                paire[0].artiste()
            );
        }
    }
}

#[tokio::test]
async fn les_titres_exclus_ne_reviennent_pas() {
    let fiche = Factice::new("qobuz", &["A"], &["Graine", "A"], 4);
    let exclus: HashSet<String> = [titre_de("qobuz", "A", 0), titre_de("qobuz", "Graine", 0)]
        .iter()
        .map(Candidat::cle)
        .collect();
    let l = composer_lot(
        "Graine",
        &[Box::new(fiche) as Box<dyn SourceRadio>],
        TAILLE_LOT,
        &exclus,
        None,
        true,
        &mut Alea::fixe(2),
    )
    .await;
    assert!(!l.candidats.is_empty());
    assert!(l.candidats.iter().all(|c| !exclus.contains(&c.cle())));
}

#[test]
fn normaliser_rapproche_les_variantes_d_un_meme_titre() {
    assert_eq!(
        normaliser("Wish You Were Here (Remastered 2011)"),
        "wish you were here"
    );
    assert_eq!(
        normaliser("  WISH you were   here [Live] "),
        "wish you were here"
    );
    assert_eq!(normaliser("AC/DC"), "ac dc");
}

// ── Contexte, reprise et redémarrage ─────────────────────────────────────

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    // L'API d'enrichissement sur un port fermé : aucun appel ne sort.
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    db
}

/// Une bibliothèque : la graine et quatre artistes du même genre, huit
/// titres chacun, plus un artiste d'un autre genre.
fn bibliotheque(db: &Arc<dyn DbBackend>) {
    let artistes = [
        (1i64, "Graine", "Jazz"),
        (2, "Voisin A", "Jazz"),
        (3, "Voisin B", "Jazz"),
        (4, "Voisin C", "Jazz"),
        (5, "Voisin D", "Jazz"),
        (6, "Hors genre", "Metal"),
    ];
    let mut id = 1i64;
    for (aid, nom, genre) in artistes {
        db.execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&aid, &nom],
        )
        .unwrap();
        for n in 0..20 {
            let titre = format!("{nom} {n}");
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, genre, duration_ms) VALUES (?, ?, ?, ?, 200000)",
                &[&id, &titre.as_str(), &aid, &genre],
            )
            .unwrap();
            id += 1;
        }
    }
}

fn registre_vide() -> Registre {
    Arc::new(tokio::sync::Mutex::new(
        crate::streaming::registry::ServiceRegistry::new(),
    ))
}

#[tokio::test]
async fn la_bibliotheque_fournit_des_voisins_du_meme_genre() {
    let db = base();
    bibliotheque(&db);
    let src = SourceBibliotheque::new(db.clone());
    let mut v = src.artistes_similaires("Graine", 10).await;
    v.sort();
    assert_eq!(v, vec!["Voisin A", "Voisin B", "Voisin C", "Voisin D"]);
    assert_eq!(src.titres_de("voisin a", 3).await.len(), 3);
}

#[tokio::test]
async fn la_radio_se_recharge_sans_redire_ce_qu_elle_a_propose() {
    let db = base();
    bibliotheque(&db);
    let services = registre_vide();
    let zone = 1;
    let premier = demarrer(&db, &services, zone, "Graine", None, None).await;
    assert!(!premier.candidats.is_empty());
    // Le contexte est dans les réglages : un redémarrage le relit tel quel.
    let ctx = lire_contexte(&db, zone).expect("contexte écrit par le départ");
    assert_eq!(ctx.artiste, "Graine");
    assert_eq!(ctx.lots, 1);
    let fin = premier.candidats.last().unwrap().identite_file();
    assert!(ctx.continue_sur(&fin));

    let second = continuer(&db, &services, zone, std::slice::from_ref(&fin))
        .await
        .expect("la file s'achève sur la radio : elle continue");
    let deja: HashSet<String> = premier.candidats.iter().map(Candidat::cle).collect();
    assert!(!second.candidats.is_empty());
    for c in &second.candidats {
        assert!(
            !deja.contains(&c.cle()),
            "{} repris d'un lot à l'autre",
            c.titre()
        );
    }
    let ctx = lire_contexte(&db, zone).unwrap();
    assert_eq!(ctx.lots, 2);
    assert_eq!(
        ctx.deja_proposes.len(),
        premier.candidats.len() + second.candidats.len()
    );
}

#[tokio::test]
async fn une_file_qui_ne_finit_pas_sur_la_radio_rend_la_main_a_l_autoplay() {
    let db = base();
    bibliotheque(&db);
    let services = registre_vide();
    demarrer(&db, &services, 1, "Graine", None, None).await;
    assert!(
        continuer(&db, &services, 1, &["local:99999".to_string()])
            .await
            .is_none(),
        "un titre étranger à la radio ne la continue pas"
    );
    assert!(
        lire_contexte(&db, 1).is_none(),
        "le contexte d'une radio quittée est effacé"
    );
    // Et une zone sans radio ne change rien à l'auto-lecture.
    assert!(
        continuer(&db, &services, 2, &["local:1".to_string()])
            .await
            .is_none()
    );
}

#[test]
fn la_fenetre_anti_doublon_est_bornee() {
    let mut ctx = ContexteRadioArtiste::nouveau("Graine", Some("qobuz"), Some("42"));
    for n in 0..(FENETRE_ANTI_DOUBLON + 40) {
        ctx.consigner(&[titre_de("qobuz", "A", n)]);
    }
    assert_eq!(ctx.deja_proposes.len(), FENETRE_ANTI_DOUBLON);
    assert_eq!(
        ctx.deja_proposes.last(),
        Some(&titre_de("qobuz", "A", FENETRE_ANTI_DOUBLON + 39).cle())
    );
    assert_eq!(ctx.dernier_lot, vec!["qobuz:qobuz-A-339".to_string()]);
}

// ── Un service réel simulé : résolution, bannis, ordre des sources ───────

struct ServiceSimule {
    nom: &'static str,
    connecte: bool,
    voisins: Vec<StreamArtist>,
    recherches: Arc<AtomicUsize>,
    albums_ouverts: Arc<AtomicUsize>,
}

fn artiste(id: &str, nom: &str) -> StreamArtist {
    StreamArtist {
        id: id.into(),
        name: nom.into(),
        image_path: None,
        bio: None,
    }
}

#[async_trait::async_trait]
impl StreamingService for ServiceSimule {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _e: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.connecte,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    /// « Graine » rend aussi un groupe hommage : seul le nom exact compte.
    async fn search(&self, q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        self.recherches.fetch_add(1, Ordering::SeqCst);
        let artists = if q == "Graine" {
            vec![artiste("hommage", "Graine Tribute"), artiste("g", "Graine")]
        } else {
            Vec::new()
        };
        Ok(SearchResults {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists,
            playlists: Vec::new(),
        })
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    /// Trois titres de Graine par album — y compris sur l'album où il n'est
    /// qu'invité : c'est l'ALBUM qui doit être écarté.
    async fn get_album_tracks(&self, album: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.albums_ouverts.fetch_add(1, Ordering::SeqCst);
        Ok((0..3)
            .map(|n| piste(&format!("{album}-{n}"), "Graine", &format!("{album} {n}")))
            .collect())
    }
    /// 30 albums de Graine, un album d'un autre groupe où il est invité, un
    /// album sans artiste déclaré.
    async fn get_artist_albums(&self, id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        if id != "g" {
            return Ok(Vec::new());
        }
        let album = |id: &str, artiste: &str, artiste_id: Option<&str>| StreamAlbum {
            id: id.into(),
            title: format!("Titre {id}"),
            artist: artiste.into(),
            artist_id: artiste_id.map(str::to_owned),
            cover_path: None,
            year: None,
            track_count: 3,
            quality: None,
            released_at: None,
            release_type: None,
        };
        let mut v: Vec<StreamAlbum> = (0..30)
            .map(|n| album(&format!("al{n}"), "Graine", Some("g")))
            .collect();
        v.push(album("invite", "Autre Groupe", Some("autre")));
        v.push(album("anonyme", "", None));
        Ok(v)
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist_top_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let nom = match id {
            "g" => "Graine",
            "hommage" => "Graine Tribute",
            "v1" => "Voisin Un",
            "v2" => "Voisin Deux",
            _ => return Ok(Vec::new()),
        };
        let mut pistes: Vec<StreamTrack> = (0..6)
            .map(|n| piste(&format!("{id}-{n}"), nom, &format!("{nom} {n}")))
            .collect();
        pistes[5].disponible = Some(false);
        Ok(pistes)
    }
    async fn get_similar_artists(
        &self,
        id: &str,
        _limit: usize,
    ) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(if id == "g" {
            self.voisins.clone()
        } else {
            Vec::new()
        })
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn les_sources_suivent_la_fiche_puis_les_services_connectes_puis_la_bibliotheque() {
    let db = base();
    let services = registre_vide();
    {
        let mut reg = services.lock().await;
        for (nom, connecte) in [
            ("amazon", true),
            ("qobuz", true),
            ("tidal", false),
            ("deezer", true),
        ] {
            reg.register(Box::new(ServiceSimule {
                nom,
                connecte,
                voisins: Vec::new(),
                recherches: Arc::new(AtomicUsize::new(0)),
                albums_ouverts: Arc::new(AtomicUsize::new(0)),
            }));
        }
    }
    let ctx = ContexteRadioArtiste::nouveau("Graine", Some("qobuz"), None);
    let noms: Vec<String> = sources_de_la_radio(&db, &services, &ctx)
        .await
        .iter()
        .map(|s| s.nom())
        .collect();
    assert_eq!(
        noms,
        vec!["qobuz", "amazon", "deezer", "enrichissement", "local"],
        "la fiche d'abord, les services connectés ensuite (tidal est déconnecté)"
    );
}

#[tokio::test]
async fn un_service_resout_l_artiste_au_nom_exact_et_ecarte_bannis_et_indisponibles() {
    let recherches = Arc::new(AtomicUsize::new(0));
    let svc: Box<dyn StreamingService> = Box::new(ServiceSimule {
        nom: "qobuz",
        connecte: true,
        voisins: vec![artiste("v1", "Voisin Un"), artiste("v2", "Voisin Deux")],
        recherches: recherches.clone(),
        albums_ouverts: Arc::new(AtomicUsize::new(0)),
    });
    let src = SourceService::new(
        "qobuz",
        Arc::new(tokio::sync::RwLock::new(svc)),
        HashSet::from(["g-0".to_string()]),
        None,
    );
    let titres = src.titres_de("Graine", 20).await;
    let ids: Vec<String> = titres
        .iter()
        .map(|c| match c {
            Candidat::Service { piste, .. } => piste.id.clone(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        ids,
        vec!["g-1", "g-2", "g-3", "g-4"],
        "pas le groupe hommage, pas le titre banni g-0, pas l'indisponible g-5"
    );
    assert_eq!(
        src.artistes_similaires("Graine", 10).await,
        vec!["Voisin Un", "Voisin Deux"]
    );
    // Les voisins sont connus par leur identifiant : leurs titres ne coûtent
    // aucune recherche de plus.
    let avant = recherches.load(Ordering::SeqCst);
    assert_eq!(src.titres_de("Voisin Un", 3).await.len(), 3);
    assert_eq!(recherches.load(Ordering::SeqCst), avant);
}

// ── #5395, fil 2037 point 6 : discographie et diversité par album ────────

/// Une source avec une DISCOGRAPHIE : l'artiste de départ a `albums`
/// albums de `par_album` titres, et des titres phares (« Phare n ») qui ne
/// doivent servir qu'en complément. Les voisins ont des titres répartis sur
/// deux albums, dont une compilation commune à tous.
struct FacticeDisco {
    albums: usize,
    par_album: usize,
    voisins: Vec<String>,
    albums_ouverts: Arc<AtomicUsize>,
}

fn piste_album(source: &str, artiste: &str, album: &str, n: usize) -> Candidat {
    let mut p = piste(
        &format!("{source}-{artiste}-{album}-{n}"),
        artiste,
        &format!("{artiste} {album} {n}"),
    );
    p.album = Some(album.to_string());
    Candidat::Service {
        source: source.into(),
        piste: p,
    }
}

#[async_trait::async_trait]
impl SourceRadio for FacticeDisco {
    fn nom(&self) -> String {
        "qobuz".into()
    }
    async fn artistes_similaires(&self, _a: &str, _m: usize) -> Vec<String> {
        self.voisins.clone()
    }
    async fn titres_de(&self, artiste: &str, _m: usize) -> Vec<Candidat> {
        if artiste == "Graine" {
            return (0..10)
                .map(|n| piste_album("qobuz", "Graine", "Phares", n))
                .collect();
        }
        (0..6)
            .map(|n| {
                let album = if n % 2 == 0 {
                    "Compilation commune".to_string()
                } else {
                    format!("Album de {artiste}")
                };
                piste_album("qobuz", artiste, &album, n)
            })
            .collect()
    }
    async fn albums_de(&self, artiste: &str) -> Vec<AlbumRadio> {
        if artiste != "Graine" {
            return Vec::new();
        }
        (0..self.albums)
            .map(|n| AlbumRadio {
                id: format!("al{n}"),
                titre: format!("Album {n}"),
            })
            .collect()
    }
    async fn titres_album(&self, artiste: &str, album: &AlbumRadio) -> Vec<Candidat> {
        self.albums_ouverts.fetch_add(1, Ordering::SeqCst);
        (0..self.par_album)
            .map(|n| piste_album("qobuz", artiste, &album.titre, n))
            .collect()
    }
}

fn disco(albums: usize, par_album: usize) -> (FacticeDisco, Arc<AtomicUsize>) {
    let ouverts = Arc::new(AtomicUsize::new(0));
    (
        FacticeDisco {
            albums,
            par_album,
            voisins: voisins(12),
            albums_ouverts: ouverts.clone(),
        },
        ouverts,
    )
}

fn albums_de_la_graine(l: &Lot) -> Vec<String> {
    l.candidats
        .iter()
        .filter(|c| c.artiste() == "Graine")
        .filter_map(Candidat::album_cle)
        .collect()
}

#[tokio::test]
async fn la_part_de_l_artiste_puise_dans_toute_sa_discographie_un_titre_par_album() {
    for graine in 1..=10u64 {
        let (src, ouverts) = disco(20, 8);
        let l = lot(vec![Box::new(src)], graine).await;
        assert_eq!(l.candidats.len(), 50);
        let albums = albums_de_la_graine(&l);
        assert_eq!(albums.len(), 10, "graine {graine} : 10 titres de l'artiste");
        let distincts: HashSet<&String> = albums.iter().collect();
        assert_eq!(
            distincts.len(),
            10,
            "graine {graine} : 20 albums disponibles, un titre par album : {albums:?}"
        );
        assert!(
            albums.iter().all(|a| a != "phares"),
            "graine {graine} : les titres phares ne servent qu'à compléter"
        );
        let n = ouverts.load(Ordering::SeqCst);
        assert!(
            n <= MAX_ALBUMS_PAR_LOT,
            "graine {graine} : {n} albums ouverts, borne {MAX_ALBUMS_PAR_LOT}"
        );
    }
}

#[tokio::test]
async fn avec_moins_d_albums_que_de_titres_chaque_album_sert_au_moins_une_fois() {
    let (src, ouverts) = disco(4, 8);
    let l = lot(vec![Box::new(src)], 5).await;
    let albums = albums_de_la_graine(&l);
    assert_eq!(albums.len(), 10);
    let distincts: HashSet<&String> = albums.iter().collect();
    assert_eq!(
        distincts.len(),
        4,
        "les 4 albums sont tous représentés : {albums:?}"
    );
    assert_eq!(ouverts.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn deux_titres_du_meme_album_ne_se_suivent_pas() {
    for graine in 1..=30u64 {
        let (src, _) = disco(20, 8);
        let l = lot(vec![Box::new(src)], graine).await;
        for paire in l.candidats.windows(2) {
            if let (Some(a), Some(b)) = (paire[0].album_cle(), paire[1].album_cle()) {
                assert_ne!(
                    a,
                    b,
                    "graine {graine} : deux titres de « {a} » d'affilée ({} puis {})",
                    paire[0].titre(),
                    paire[1].titre()
                );
            }
        }
    }
}

#[tokio::test]
async fn le_deuxieme_titre_d_un_voisin_vient_d_un_autre_album() {
    let (src, _) = disco(20, 8);
    let l = lot(vec![Box::new(src)], 3).await;
    let mut par_voisin: HashMap<String, Vec<String>> = HashMap::new();
    for c in l.candidats.iter().filter(|c| c.artiste() != "Graine") {
        par_voisin
            .entry(c.artiste().to_string())
            .or_default()
            .push(c.album_cle().unwrap());
    }
    for (v, albums) in par_voisin {
        if albums.len() == 2 {
            assert_ne!(albums[0], albums[1], "{v} : deux titres du même album");
        }
    }
}

#[tokio::test]
async fn la_bibliotheque_ne_prend_que_les_albums_de_l_artiste_pas_ceux_ou_il_est_invite() {
    let db = base();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Graine'), (2, 'Various Artists');
         INSERT INTO albums (id, title, artist_id) VALUES (10, 'A lui', 1), (11, 'Compilation', 2);
         INSERT INTO tracks (id, title, artist_id, album_id, duration_ms) VALUES
           (1, 'Sien 1', 1, 10, 1000), (2, 'Sien 2', 1, 10, 1000),
           (3, 'Invité', 1, 11, 1000), (4, 'Autre', 2, 11, 1000);",
    )
    .unwrap();
    let src = SourceBibliotheque::new(db.clone());
    let albums = src.albums_de("graine").await;
    assert_eq!(
        albums,
        vec![AlbumRadio {
            id: "10".into(),
            titre: "A lui".into()
        }]
    );
    let titres: Vec<String> = src
        .titres_album("Graine", &albums[0])
        .await
        .iter()
        .map(|c| c.titre().to_string())
        .collect();
    assert_eq!(titres.len(), 2);
    assert!(titres.iter().all(|t| t.starts_with("Sien")));
}

#[tokio::test]
async fn la_fiche_ouvre_les_albums_de_l_artiste_pas_ceux_ou_il_est_invite_et_borne_ses_appels() {
    let ouverts = Arc::new(AtomicUsize::new(0));
    let svc: Box<dyn StreamingService> = Box::new(ServiceSimule {
        nom: "qobuz",
        connecte: true,
        voisins: (0..12)
            .map(|n| artiste(&format!("v{}", n % 2 + 1), &format!("Voisin {n}")))
            .collect(),
        recherches: Arc::new(AtomicUsize::new(0)),
        albums_ouverts: ouverts.clone(),
    });
    let src = SourceService::new(
        "qobuz",
        Arc::new(tokio::sync::RwLock::new(svc)),
        HashSet::new(),
        Some(("Graine", "g")),
    )
    .avec_discographie();
    let albums = src.albums_de("Graine").await;
    assert_eq!(
        albums.len(),
        31,
        "30 albums à lui + l'album sans artiste déclaré"
    );
    assert!(
        albums.iter().all(|a| a.id != "invite"),
        "l'album où il est invité est écarté"
    );

    let sources: Vec<Box<dyn SourceRadio>> = vec![Box::new(src)];
    let l = composer_lot(
        "Graine",
        &sources,
        TAILLE_LOT,
        &HashSet::new(),
        None,
        true,
        &mut Alea::fixe(4),
    )
    .await;
    let n = ouverts.load(Ordering::SeqCst);
    assert!(
        n <= MAX_ALBUMS_PAR_LOT,
        "{n} appels get_album_tracks pour un lot, borne {MAX_ALBUMS_PAR_LOT}"
    );
    let de_la_graine: Vec<&Candidat> = l
        .candidats
        .iter()
        .filter(|c| c.artiste() == "Graine")
        .collect();
    // Peu de voisins ici : la part de l'artiste s'élargit, la borne tient.
    assert!(de_la_graine.len() >= 10);
    assert!(
        de_la_graine
            .iter()
            .all(|c| !c.titre().starts_with("invite"))
    );

    // Un service qui n'est PAS la fiche ne fournit pas de discographie.
    let autre: Box<dyn StreamingService> = Box::new(ServiceSimule {
        nom: "tidal",
        connecte: true,
        voisins: Vec::new(),
        recherches: Arc::new(AtomicUsize::new(0)),
        albums_ouverts: Arc::new(AtomicUsize::new(0)),
    });
    let autre = SourceService::new(
        "tidal",
        Arc::new(tokio::sync::RwLock::new(autre)),
        HashSet::new(),
        Some(("Graine", "g")),
    );
    assert!(autre.albums_de("Graine").await.is_empty());
}
