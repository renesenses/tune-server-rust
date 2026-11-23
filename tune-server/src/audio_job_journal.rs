//! Durable status tombstones. Jobs are not resumed after a process restart;
//! interrupted work is explicit and published files are never deleted here.
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
fn root() -> PathBuf {
    crate::native_audio::root().join("jobs")
}
fn path(directory: &Path, kind: &str, id: &str) -> Result<PathBuf, String> {
    if !["converter", "declick"].contains(&kind) || uuid::Uuid::parse_str(id).is_err() {
        return Err("invalid job handle".into());
    }
    Ok(directory.join(format!("{kind}-{id}.json")))
}
fn write_at(
    directory: &Path,
    kind: &str,
    id: &str,
    status: &str,
    total: usize,
    completed: usize,
) -> Result<(), String> {
    snapshot_at(
        directory,
        kind,
        id,
        &json!({"job_id":id,"status":status,"total":total,"completed":completed}),
    )
}
fn snapshot_at(directory: &Path, kind: &str, id: &str, value: &Value) -> Result<(), String> {
    let path = path(directory, kind, id)?;
    if value["job_id"] != id {
        return Err("job snapshot id mismatch".into());
    }
    // #9343 — chaque échec nomme le chemin : « Read-only file system (os
    // error 30) » seul ne disait pas que le journal visait `/plugins/audio`.
    let es = |operation: &str, chemin: &Path, e: &std::io::Error| {
        tune_http_types::error::message_d_entree_sortie(operation, chemin, e)
    };
    fs::create_dir_all(directory).map_err(|e| es("create job journal folder", directory, &e))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .map_err(|e| es("write job journal", directory, &e))?;
    serde_json::to_writer(&mut file, value)
        .map_err(|e| format!("write job journal {}: {e}", path.display()))?;
    file.flush()
        .map_err(|e| es("write job journal", &path, &e))?;
    file.as_file()
        .sync_all()
        .map_err(|e| es("write job journal", &path, &e))?;
    file.persist(&path)
        .map_err(|e| es("write job journal", &path, &e.error))?;
    Ok(())
}
pub fn write(
    kind: &str,
    id: &str,
    status: &str,
    total: usize,
    completed: usize,
) -> Result<(), String> {
    write_at(&root(), kind, id, status, total, completed)
}
fn recovered_at(directory: &Path, kind: &str, id: &str) -> Option<Value> {
    let bytes = fs::read(path(directory, kind, id).ok()?).ok()?;
    let mut value: Value = serde_json::from_slice(&bytes).ok()?;
    if value["job_id"] != id
        || !["running", "completed", "failed", "cancelled"]
            .iter()
            .any(|s| value["status"] == *s)
    {
        return None;
    }
    if value["status"] == "running" {
        value["status"] = json!("interrupted");
        value["state"] = json!("error");
        value["error"] =
            json!("Job interrupted: worker no longer available; retry creates a new job.");
    } else {
        value["state"] = json!(if value["status"] == "completed" {
            "done"
        } else {
            "error"
        });
    }
    value["recovered"] = json!(true);
    value["download_available"] = json!(false);
    value["current_file"] = json!("");
    if value.get("errors").is_none() {
        value["errors"] = json!([]);
    }
    Some(value)
}
pub fn write_result(kind: &str, id: &str, value: &Value) -> Result<(), String> {
    snapshot_at(&root(), kind, id, value)
}
pub fn recovered(kind: &str, id: &str) -> Option<Value> {
    recovered_at(&root(), kind, id)
}
#[cfg(test)]
mod tests {
    use super::*;
    /// #9343 — un journal impossible à écrire nomme le dossier visé.
    #[test]
    fn un_echec_d_ecriture_nomme_le_dossier_du_journal() {
        let directory = tempfile::tempdir().unwrap();
        let fichier = directory.path().join("pas-un-dossier");
        fs::write(&fichier, b"x").unwrap();
        let jobs = fichier.join("jobs");
        let id = uuid::Uuid::new_v4().to_string();
        let erreur = write_at(&jobs, "converter", &id, "running", 1, 0).unwrap_err();
        assert!(
            erreur.contains(&jobs.display().to_string()),
            "le chemin manque : {erreur}"
        );
    }
    #[test]
    fn premium_sdk_journal_survives_worker_loss_without_claiming_resumed_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        write_at(directory.path(), "converter", &id, "running", 5, 0).unwrap();
        let recovered = recovered_at(directory.path(), "converter", &id).unwrap();
        assert_eq!(recovered["status"], "interrupted");
        assert_eq!(recovered["state"], "error");
        assert_eq!(recovered["download_available"], false);
        write_at(directory.path(), "converter", &id, "completed", 5, 5).unwrap();
        let recovered = recovered_at(directory.path(), "converter", &id).unwrap();
        assert_eq!(recovered["status"], "completed");
        assert_eq!(recovered["completed"], 5);
        let partial = json!({"job_id":id,"status":"completed","total":5,"completed":5,"errors":[{"file":"unavailable.wav","message":"decoder failed"}]});
        snapshot_at(directory.path(), "converter", &id, &partial).unwrap();
        assert_eq!(
            recovered_at(directory.path(), "converter", &id).unwrap()["errors"],
            partial["errors"]
        );
        assert!(recovered_at(directory.path(), "declick", &id).is_none());
        assert!(write_at(directory.path(), "converter", "../other", "running", 1, 0).is_err());
        assert_eq!(
            fs::read_dir(directory.path()).unwrap().count(),
            1,
            "temporary journal files leaked"
        );
    }
}
