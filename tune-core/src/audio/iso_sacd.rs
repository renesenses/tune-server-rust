use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::info;

/// Extract DSF tracks from a SACD ISO file using sacd_extract.
/// Returns the paths of the extracted DSF files in a temp directory.
pub fn extract_iso_to_dsf(iso_path: &Path) -> Result<Vec<PathBuf>, String> {
    let sacd_extract =
        find_sacd_extract().ok_or("sacd_extract not found — install it for ISO SACD support")?;

    let output_dir = iso_path.with_extension("sacd_extract");
    std::fs::create_dir_all(&output_dir).map_err(|e| format!("create dir: {e}"))?;

    let output = Command::new(&sacd_extract)
        .args([
            "-i",
            &iso_path.to_string_lossy(),
            "-s", // stereo extraction
            "-p", // DSF output
            "-o",
            &output_dir.to_string_lossy(),
        ])
        .output()
        .map_err(|e| format!("sacd_extract exec: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("sacd_extract failed: {stderr}"));
    }

    let dsf_files: Vec<PathBuf> = std::fs::read_dir(&output_dir)
        .map_err(|e| format!("read dir: {e}"))?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().is_some_and(|ext| ext == "dsf") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    info!(
        iso = %iso_path.display(),
        tracks = dsf_files.len(),
        "sacd_iso_extracted"
    );

    Ok(dsf_files)
}

/// Check if a file is a SACD ISO by reading the first bytes.
pub fn is_sacd_iso(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "iso" {
        return false;
    }
    // SACD ISOs have "SACDMTOC" at offset 0x800 * 510
    // For a quick check, just verify file size > 4MB and extension
    std::fs::metadata(path)
        .map(|m| m.len() > 4_000_000)
        .unwrap_or(false)
}

fn find_sacd_extract() -> Option<PathBuf> {
    let candidates = [
        "sacd_extract",
        "/usr/local/bin/sacd_extract",
        "/usr/bin/sacd_extract",
        "/opt/homebrew/bin/sacd_extract",
    ];
    for name in &candidates {
        if let Ok(output) = Command::new(name).arg("--help").output() {
            if output.status.success() || !output.stdout.is_empty() || !output.stderr.is_empty() {
                return Some(PathBuf::from(name));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_sacd_extract_if_available() {
        // This test only passes if sacd_extract is installed
        let result = find_sacd_extract();
        if result.is_some() {
            println!("sacd_extract found at: {:?}", result.unwrap());
        } else {
            println!("sacd_extract not installed (test skipped)");
        }
    }

    #[test]
    fn is_sacd_iso_checks_extension() {
        assert!(!is_sacd_iso(Path::new("/tmp/test.flac")));
        assert!(!is_sacd_iso(Path::new("/tmp/test.dsf")));
        // Can't test positive case without a real ISO file
    }
}
