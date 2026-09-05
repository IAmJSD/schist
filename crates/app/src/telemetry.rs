use rand::RngExt;
use std::{
    fs,
    path::{Path, PathBuf},
    thread, time,
};

/// Where the daily ping goes.
const TELEMETRY_URL: &str = "https://telemetry.schist.app/v1/ping";

/// The opt-out marker, in Schist's config folder. Its presence is the
/// whole preference: no contents are read.
const NO_TELEMETRY_FILE: &str = "no_telemetry";

/// Whether the ping is on, as far as the marker file is concerned. The
/// SCHIST_NO_TELEMETRY variable can still switch it off on top of this.
pub fn enabled(schist_folder: &Path) -> bool {
    !matches!(fs::exists(schist_folder.join(NO_TELEMETRY_FILE)), Ok(true))
}

/// Create or remove the marker file. Takes effect at the next turn --
/// within a day, or at the next launch.
pub fn set_enabled(schist_folder: &PathBuf, enabled: bool) -> std::io::Result<()> {
    let path = schist_folder.join(NO_TELEMETRY_FILE);
    if enabled {
        match fs::remove_file(path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    } else {
        fs::create_dir_all(schist_folder)?;
        fs::write(path, "")
    }
}

/// What one ping says: the dedup ID, which build this is, and the
/// hardware it runs on -- CPU model and core count, the GPU adapter the
/// compositor opened (null when it is on the CPU) and how much RAM there
/// is. Nothing about the user, the machine's name or what is open.
fn payload(telemetry_id: &str) -> serde_json::Value {
    use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
    let system = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::new())
            .with_memory(MemoryRefreshKind::new().with_ram()),
    );
    let cpu = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().trim().to_string());
    let gpu = crate::workspace::gpu_info().map(|gpu| {
        serde_json::json!({
            "name": gpu.name,
            "backend": gpu.backend,
            "driver": gpu.driver,
        })
    });
    serde_json::json!({
        "id": telemetry_id,
        "version": crate::update::current_version(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu": cpu,
        "cores": system.cpus().len(),
        "gpu": gpu,
        "ram_mib": system.total_memory() / (1024 * 1024),
    })
}

/// One ping.
fn send_telemetry(telemetry_id: &str) -> bool {
    let body = payload(telemetry_id);
    ureq::post(TELEMETRY_URL)
        .config()
        .timeout_global(Some(time::Duration::from_secs(30)))
        .build()
        .header("User-Agent", "schist-telemetry")
        .send_json(&body)
        .is_ok()
}

fn telemetry_turn(schist_folder: &Path) {
    // Check if the magic file exists that nukes telemetry.
    if !enabled(schist_folder) {
        return;
    }

    // This ID links to NOTHING else within the app and is just used for deduplication
    let id_file_path = schist_folder.join("telemetry_id.txt");
    let telemetry_id = match fs::read_to_string(id_file_path.clone()) {
        Ok(v) => v,
        Err(_) => {
            let id: String = rand::rng()
                .sample_iter(&rand::distr::Alphanumeric)
                .take(32)
                .map(char::from)
                .collect();
            match fs::write(id_file_path, id.clone()) {
                Ok(_) => id,
                Err(_) => {
                    // We are cooked
                    return;
                }
            }
        }
    };

    // Try 3 times
    for _ in 0..3 {
        if send_telemetry(&telemetry_id) {
            // This worked!
            return;
        }

        // Sleep 5 minutes
        thread::sleep(time::Duration::from_mins(5));
    }
}

fn telemetry_loop(schist_folder: PathBuf) {
    // Bail if SCHIST_NO_TELEMETRY is in the env vars
    if let Ok(no_telemetry_var) = std::env::var("SCHIST_NO_TELEMETRY") {
        let no_telemetry_var = no_telemetry_var.to_lowercase();
        if no_telemetry_var == "yes"
            || no_telemetry_var == "y"
            || no_telemetry_var == "true"
            || no_telemetry_var == "1"
        {
            // Bail immediately
            return;
        }
    }

    loop {
        // Take a telemetry turn
        telemetry_turn(&schist_folder);

        // Sleep for 24 hours
        thread::sleep(time::Duration::from_hours(24));
    }
}

pub fn start(schist_folder: Option<PathBuf>) {
    if let Some(schist_folder) = schist_folder {
        let _ = thread::Builder::new()
            .name("telemetry".into())
            .spawn(|| telemetry_loop(schist_folder));
    }
}
