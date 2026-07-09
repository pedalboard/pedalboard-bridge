use std::path::Path;
use std::process::Command;
use std::time::Duration;

use axum::{http::StatusCode, response::IntoResponse};
use axum_extra::extract::Multipart;
use tracing::info;

/// Handler for POST /flash — accepts UF2 firmware upload.
pub async fn handle_flash(mut multipart: Multipart) -> impl IntoResponse {
    // Extract firmware file from multipart form.
    let mut firmware_data: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("firmware") {
            match field.bytes().await {
                Ok(data) => firmware_data = Some(data.to_vec()),
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, format!("read error: {e}"));
                }
            }
        }
    }

    let data = match firmware_data {
        Some(d) => d,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing 'firmware' file field".to_string(),
            );
        }
    };

    info!("Flash: received {} bytes", data.len());

    // Wait for UF2 drive to appear.
    let uf2_dev = match wait_for_uf2_drive().await {
        Some(dev) => dev,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "UF2 drive not found (is device in bootloader mode?)".to_string(),
            );
        }
    };

    // Mount the drive.
    let mount_path = "/mnt/uf2";
    let _ = std::fs::create_dir_all(mount_path);

    let mount_result = Command::new("sudo")
        .args(["mount", "-o", "uid=1000,gid=1000", &uf2_dev, mount_path])
        .output();

    if let Err(e) = mount_result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mount failed: {e}"),
        );
    }
    let mount_output = mount_result.unwrap();
    if !mount_output.status.success() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "mount failed: {}",
                String::from_utf8_lossy(&mount_output.stderr)
            ),
        );
    }

    // Write the UF2 file.
    let uf2_path = Path::new(mount_path).join("firmware.uf2");
    if let Err(e) = std::fs::write(&uf2_path, &data) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write failed: {e}"),
        );
    }

    // Sync.
    let _ = Command::new("sync").status();

    info!("Flash: written {} bytes to {:?}", data.len(), uf2_path);
    (StatusCode::OK, format!("OK: flashed {} bytes\n", data.len()))
}

/// Wait up to 15 seconds for the RPI-RP2 UF2 drive to appear.
async fn wait_for_uf2_drive() -> Option<String> {
    for _ in 0..30 {
        // Look for RP2040 boot drive by label.
        if let Ok(entries) = glob::glob("/dev/disk/by-label/RPI-RP2*") {
            for entry in entries.flatten() {
                if let Ok(resolved) = std::fs::canonicalize(&entry) {
                    return Some(resolved.to_string_lossy().to_string());
                }
            }
        }

        // Fallback: common device paths.
        for dev in &["/dev/sda1", "/dev/sdb1"] {
            if Path::new(dev).exists() {
                return Some(dev.to_string());
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

/// Glob helper (inline, avoids adding a dependency).
mod glob {
    use std::path::PathBuf;

    pub fn glob(pattern: &str) -> Result<impl Iterator<Item = Result<PathBuf, ()>>, ()> {
        // Simple glob for /dev/disk/by-label/RPI-RP2*
        let dir = std::path::Path::new(pattern)
            .parent()
            .ok_or(())?;
        let prefix = std::path::Path::new(pattern)
            .file_name()
            .ok_or(())?
            .to_string_lossy()
            .trim_end_matches('*')
            .to_string();

        let entries = std::fs::read_dir(dir).map_err(|_| ())?;
        Ok(entries.filter_map(move |e| {
            let entry = e.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) {
                Some(Ok(entry.path()))
            } else {
                None
            }
        }))
    }
}
