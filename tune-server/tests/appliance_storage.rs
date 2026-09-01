//! Relocation des données appliance (docs/DATA-RELOCATION.md).
//! Une seule fonction : env vars process-wide (voir tests/appliance.rs).
#![cfg(unix)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(json!(null)))
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(json!(null)))
}

fn write_stub(dir: &std::path::Path, name: &str, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[tokio::test]
async fn relocation_full_flow() {
    let _environment = crate::lock_environment();
    let tmp = tune_core::test_scratch::scratch_dir("tune-reloc-test");

    // Source : une vraie petite base SQLite + un cache pochettes.
    let src = tmp.join("source");
    std::fs::create_dir_all(src.join("artwork_cache")).unwrap();
    std::fs::write(src.join("artwork_cache/cover1.jpg"), b"jpegdata").unwrap();
    let src_db = src.join("tune.db");
    {
        let conn = rusqlite::Connection::open(&src_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE probe (id INTEGER PRIMARY KEY, v TEXT); \
             INSERT INTO probe (v) VALUES ('relocated');",
        )
        .unwrap();
    }

    // « Volume » cible déjà monté (le stub systemctl ne monte rien).
    let srv = tmp.join("srv");
    std::fs::create_dir_all(&srv).unwrap();
    let units = tmp.join("units");
    std::fs::create_dir_all(&units).unwrap();

    // tune.toml que la bascule doit réécrire.
    let cfg = tmp.join("tune.toml");
    std::fs::write(
        &cfg,
        format!(
            "port = 8888\ndb_path = \"{}\"\nartwork_dir = \"{}\"\n",
            src_db.display(),
            src.join("artwork_cache").display()
        ),
    )
    .unwrap();

    // Faux /proc/mounts (avec racine → disque système sdy) + stubs.
    let mounts = tmp.join("mounts");
    std::fs::write(
        &mounts,
        "/dev/sdy2 / ext4 rw 0 0\n/dev/sdz1 /media/sdz1 exfat rw 0 0\n",
    )
    .unwrap();
    let blkid = write_stub(
        &tmp,
        "blkid.sh",
        "#!/bin/bash\necho DEVNAME=/dev/sdz1\necho UUID=TEST-UUID\necho LABEL=DSD2TO\necho TYPE=exfat\n",
    );
    let df = write_stub(
        &tmp,
        "df.sh",
        "#!/bin/bash\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\necho '/dev/sdz1 1953480700 100 1953480600 1% /media/sdz1'\n",
    );
    let systemctl = write_stub(&tmp, "systemctl.sh", "#!/bin/bash\nexit 0\n");
    // Inventaire lsblk : clé système sdy, SATA interne sda avec partition ntfs
    // NON montée (le cas Gil), disque USB sdz monté.
    let lsblk = write_stub(
        &tmp,
        "lsblk.sh",
        concat!(
            "#!/bin/bash\n",
            "cat << 'EOT'\n",
            "NAME=\"sdy\" TYPE=\"disk\" FSTYPE=\"\" UUID=\"\" SIZE=\"14,5G\" TRAN=\"usb\" MOUNTPOINT=\"\" LABEL=\"\" MODEL=\"USB Key\" PKNAME=\"\"\n",
            "NAME=\"sdy2\" TYPE=\"part\" FSTYPE=\"ext4\" UUID=\"ROOT-UUID\" SIZE=\"14G\" TRAN=\"usb\" MOUNTPOINT=\"/\" LABEL=\"tuneroot\" MODEL=\"\" PKNAME=\"sdy\"\n",
            "NAME=\"sda\" TYPE=\"disk\" FSTYPE=\"\" UUID=\"\" SIZE=\"931,5G\" TRAN=\"sata\" MOUNTPOINT=\"\" LABEL=\"\" MODEL=\"WDC WD10EZEX\" PKNAME=\"\"\n",
            "NAME=\"sda1\" TYPE=\"part\" FSTYPE=\"ntfs\" UUID=\"MUSIC-UUID\" SIZE=\"931,5G\" TRAN=\"sata\" MOUNTPOINT=\"\" LABEL=\"MUSIQUE\" MODEL=\"\" PKNAME=\"sda\"\n",
            "NAME=\"sdz\" TYPE=\"disk\" FSTYPE=\"\" UUID=\"\" SIZE=\"931,5G\" TRAN=\"usb\" MOUNTPOINT=\"\" LABEL=\"\" MODEL=\"Ext USB\" PKNAME=\"\"\n",
            "NAME=\"sdz1\" TYPE=\"part\" FSTYPE=\"exfat\" UUID=\"TEST-UUID\" SIZE=\"931,5G\" TRAN=\"usb\" MOUNTPOINT=\"/media/sdz1\" LABEL=\"DSD2TO\" MODEL=\"\" PKNAME=\"sdz\"\n",
            "EOT\n",
        ),
    );

    unsafe {
        std::env::set_var("TUNE_APPLIANCE", "1");
        std::env::set_var("TUNE_PROC_MOUNTS", &mounts);
        std::env::set_var("TUNE_BLKID_BIN", &blkid);
        std::env::set_var("TUNE_DF_BIN", &df);
        std::env::set_var("TUNE_SYSTEMCTL_BIN", &systemctl);
        std::env::set_var("TUNE_MOUNT_UNIT_DIR", &units);
        std::env::set_var("TUNE_DATA_MOUNT_POINT", &srv);
        std::env::set_var("TUNE_CONFIG_PATH", &cfg);
        std::env::set_var("TUNE_LSBLK_BIN", &lsblk);
        std::env::set_var("TUNE_MUSIC_MOUNT_BASE", tmp.join("music-mounts"));
        std::env::set_var("TUNE_DEV_DIR", tmp.join("dev"));
    }
    std::fs::create_dir_all(tmp.join("dev")).unwrap();

    // App dont la config pointe sur la source réelle.
    let config = tune_server::config::TuneConfig {
        db_path: src_db.to_string_lossy().into_owned(),
        artwork_dir: src.join("artwork_cache").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let state = tune_server::state::AppState::new(&config.db_path.clone(), 0, config).unwrap();
    let app = tune_server::routes::router(state);

    // Volumes candidats visibles.
    let (status, body) = get(&app, "/api/v1/appliance/storage").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let vols = body["volumes"].as_array().unwrap();
    assert_eq!(vols.len(), 1, "{body}");
    assert_eq!(vols[0]["uuid"], "TEST-UUID");
    assert_eq!(vols[0]["fs"], "exfat");
    assert!(vols[0]["free_bytes"].as_u64().unwrap() > 1_000_000_000_000);

    // UUID inconnu → 400.
    let (status, _) = post_json(
        &app,
        "/api/v1/appliance/data/relocate",
        json!({"uuid": "NOPE"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Relocalisation réelle vers le tmpdir.
    let (status, body) = post_json(
        &app,
        "/api/v1/appliance/data/relocate",
        json!({"uuid": "TEST-UUID"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Poll jusqu'à done/failed.
    let mut phase = String::new();
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, st) = get(&app, "/api/v1/appliance/data/status").await;
        phase = st["job"]["phase"].as_str().unwrap_or("").to_string();
        if phase == "done" || phase == "failed" {
            if phase == "failed" {
                panic!("job failed: {st}");
            }
            break;
        }
    }
    assert_eq!(phase, "done");

    // Unité systemd écrite avec l'UUID.
    let unit_files: Vec<_> = std::fs::read_dir(&units).unwrap().flatten().collect();
    assert_eq!(unit_files.len(), 1);
    let unit_body = std::fs::read_to_string(unit_files[0].path()).unwrap();
    assert!(
        unit_body.contains("What=/dev/disk/by-uuid/TEST-UUID"),
        "{unit_body}"
    );
    assert!(unit_body.contains("Options=nofail"));

    // Données copiées et intègres.
    let target_db = srv.join("TuneData/tune.db");
    assert!(target_db.exists());
    {
        let conn = rusqlite::Connection::open(&target_db).unwrap();
        let v: String = conn
            .query_row("SELECT v FROM probe LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "relocated");
    }
    assert!(srv.join("TuneData/artwork_cache/cover1.jpg").exists());

    // tune.toml réécrit vers les nouveaux chemins, port préservé.
    let new_cfg = std::fs::read_to_string(&cfg).unwrap();
    assert!(new_cfg.contains("TuneData/tune.db"), "{new_cfg}");
    assert!(new_cfg.contains("TuneData/artwork_cache"), "{new_cfg}");
    assert!(new_cfg.contains("port = 8888"));
    assert!(!new_cfg.contains("source/tune.db"));

    // ---- Inventaire disques + partitions non montées (cas Gil, SATA) ----
    let (status, body) = get(&app, "/api/v1/appliance/storage").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let disks = body["disks"].as_array().unwrap();
    assert_eq!(disks.len(), 3, "{body}");
    let sdy = disks.iter().find(|d| d["name"] == "sdy").unwrap();
    assert_eq!(sdy["is_boot"], json!(true));
    let sda = disks.iter().find(|d| d["name"] == "sda").unwrap();
    assert_eq!(sda["is_boot"], json!(false));
    assert_eq!(sda["tran"], "sata");
    let unmounted = body["unmounted_partitions"].as_array().unwrap();
    assert_eq!(unmounted.len(), 1, "{body}");
    assert_eq!(unmounted[0]["uuid"], "MUSIC-UUID");
    assert_eq!(unmounted[0]["label"], "MUSIQUE");
    assert_eq!(unmounted[0]["disk_model"], "WDC WD10EZEX");

    // Montage de la partition musique par UUID.
    let (status, body) = post_json(
        &app,
        "/api/v1/appliance/storage/mount",
        json!({"uuid": "MUSIC-UUID"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mount_path = body["mount_path"].as_str().unwrap().to_string();
    assert!(mount_path.contains("music-mounts"), "{mount_path}");
    let unit_bodies: Vec<String> = std::fs::read_dir(&units)
        .unwrap()
        .flatten()
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect();
    assert!(
        unit_bodies.iter().any(|u| u.contains("by-uuid/MUSIC-UUID")),
        "unit musique manquante"
    );
    // UUID inconnu ou déjà monté → 400.
    let (status, _) = post_json(
        &app,
        "/api/v1/appliance/storage/mount",
        json!({"uuid": "TEST-UUID"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // ---- Installation sur disque (gardes puis happy path stubé) ----
    let (status, _) = post_json(
        &app,
        "/api/v1/appliance/install-to-disk",
        json!({"device": "sda", "confirm": "oui"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "confirm invalide accepté");
    let (status, _) = post_json(
        &app,
        "/api/v1/appliance/install-to-disk",
        json!({"device": "sdy", "confirm": "EFFACER"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "disque système accepté !");
    let (status, _) = post_json(
        &app,
        "/api/v1/appliance/install-to-disk",
        json!({"device": "nope", "confirm": "EFFACER"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Happy path : pipeline stub qui écrit la cible + simule la progression dd.
    unsafe {
        std::env::set_var("TUNE_IMAGE_URL", "file:///unused");
        std::env::set_var(
            "TUNE_INSTALL_PIPELINE",
            "printf 'tune-os-image' > \"$TUNE_TARGET\"; printf '4194304 bytes copied\\r8388608 bytes copied\\r' >&2",
        );
    }
    let (status, body) = post_json(
        &app,
        "/api/v1/appliance/install-to-disk",
        json!({"device": "sda", "confirm": "EFFACER"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut phase = String::new();
    let mut written = 0u64;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, st) = get(&app, "/api/v1/appliance/install-to-disk/status").await;
        phase = st["phase"].as_str().unwrap_or("").to_string();
        written = st["written_bytes"].as_u64().unwrap_or(0);
        if phase == "done" || phase == "failed" {
            if phase == "failed" {
                panic!("install failed: {st}");
            }
            break;
        }
    }
    assert_eq!(phase, "done");
    assert_eq!(written, 8388608, "progression dd non relayée");
    assert_eq!(
        std::fs::read_to_string(tmp.join("dev/sda")).unwrap(),
        "tune-os-image"
    );

    unsafe {
        for v in [
            "TUNE_APPLIANCE",
            "TUNE_PROC_MOUNTS",
            "TUNE_BLKID_BIN",
            "TUNE_DF_BIN",
            "TUNE_SYSTEMCTL_BIN",
            "TUNE_MOUNT_UNIT_DIR",
            "TUNE_DATA_MOUNT_POINT",
            "TUNE_CONFIG_PATH",
            "TUNE_LSBLK_BIN",
            "TUNE_MUSIC_MOUNT_BASE",
            "TUNE_DEV_DIR",
            "TUNE_IMAGE_URL",
            "TUNE_INSTALL_PIPELINE",
        ] {
            std::env::remove_var(v);
        }
    }
}
