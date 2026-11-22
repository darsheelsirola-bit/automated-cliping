use crate::db::Database;
use std::time::Duration;
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

pub fn set_startup(enabled: bool) -> Result<(), String> {
    let user = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = user
        .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map_err(|error| format!("Cannot access Windows startup settings: {error}"))?;
    if enabled {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        return run
            .set_value("ClipForgeLive", &format!("\"{}\" --tray", exe.display()))
            .map_err(|error| format!("Cannot enable ClipForge startup: {error}"));
    }
    match run.delete_value("ClipForgeLive") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot disable ClipForge startup: {error}")),
    }
}

pub fn watch_recording(db: Database) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("clipforge-windows-power".into())
        .spawn(move || {
            let mut previous = false;
            loop {
                let keep_awake = db.settings().is_ok_and(|settings| settings.keep_awake)
                    && db
                        .connection()
                        .and_then(|connection| {
                            connection.query_row(
                    "SELECT EXISTS (SELECT 1 FROM creators WHERE enabled=1 AND status='recording')",
                    [], |row| row.get(0),
                )
                        })
                        .unwrap_or(false);
                if keep_awake != previous {
                    let flags = ES_CONTINUOUS | if keep_awake { ES_SYSTEM_REQUIRED } else { 0 };
                    // The execution state is scoped to this Windows thread.
                    let applied = unsafe { SetThreadExecutionState(flags) };
                    if applied != 0 {
                        previous = keep_awake;
                    } else {
                        eprintln!("ClipForge could not update the Windows execution state.");
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        })
        .map(|_| ())
}
