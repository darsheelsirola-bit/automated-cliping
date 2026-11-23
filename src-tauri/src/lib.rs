mod background;
mod db;
mod model;
mod production;
mod recording;
mod review;
mod runtime;
mod twitch;
mod youtube;

use db::Database;
use model::{Dashboard, Settings};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
async fn dashboard(database: State<'_, Database>) -> Result<Dashboard, String> {
    let database = database.inner().clone();
    tauri::async_runtime::spawn_blocking(move || database.dashboard())
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
fn add_creator(database: State<'_, Database>, value: String) -> Result<(), String> {
    database.add_creator(&value)
}

#[tauri::command]
fn set_creator_enabled(
    database: State<'_, Database>,
    id: i64,
    enabled: bool,
) -> Result<(), String> {
    database.set_creator_enabled(id, enabled)
}

#[tauri::command]
fn remove_creator(database: State<'_, Database>, id: i64) -> Result<(), String> {
    database.remove_creator(id)
}

#[tauri::command]
fn save_settings(database: State<'_, Database>, settings: Settings) -> Result<(), String> {
    let previous = database.settings()?;
    if previous.start_with_windows != settings.start_with_windows {
        background::set_startup(settings.start_with_windows)?;
    }
    if let Err(error) = database.save_settings(&settings) {
        if previous.start_with_windows != settings.start_with_windows {
            background::set_startup(previous.start_with_windows)?;
        }
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
fn show_clip(app: AppHandle, database: State<'_, Database>, id: i64) -> Result<(), String> {
    let output = database.clip_output(id)?;
    if !std::path::Path::new(&output).is_file() {
        return Err("The exported file could not be found on disk.".into());
    }
    app.opener()
        .reveal_item_in_dir(output)
        .map_err(|error| format!("Could not show the export in File Explorer. {error}"))
}

#[tauri::command]
fn clip_thumbnail(database: State<'_, Database>, id: i64) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use rusqlite::OptionalExtension;
    let path: Option<String> = database
        .connection()
        .and_then(|connection| {
            connection.query_row(
        "SELECT thumbnail_path FROM clip_jobs WHERE id=?1 AND thumbnail_path IS NOT NULL", [id],
        |row| row.get(0),
    ).optional()
        })
        .map_err(|error| error.to_string())?;
    let Some(path) = path else {
        return Ok(None);
    };
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    use std::io::Read;
    file.take(256_001)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 256_000 || !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err("The thumbnail is not a valid JPEG preview.".into());
    }
    Ok(Some(format!(
        "data:image/jpeg;base64,{}",
        STANDARD.encode(bytes)
    )))
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let directory = app.path().app_local_data_dir()?;
            std::fs::create_dir_all(&directory)?;
            let database = Database::open(directory.join("clipforge.sqlite3"))?;
            database.connection()?.execute(
                "UPDATE creators SET status = 'idle', stream_title = NULL",
                [],
            )?;
            app.manage(database.clone());
            background::watch_recording(database.clone())?;
            if std::env::args_os().any(|arg| arg == "--tray") {
                if let Some(window) = app.get_webview_window("main") {
                    window.hide()?;
                }
            }
            app.manage(twitch::Twitch::new()?);
            app.manage(production::ModelSetup::default());
            app.manage(production::RenderSlot::default());
            app.manage(youtube::YouTube::default());
            tauri::async_runtime::spawn(youtube::supervise(database.clone()));
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = recording::recover(&database).await {
                    if let Err(problem) = database.connection().and_then(|connection| {
                        connection.execute(
                            "INSERT INTO error_logs (subsystem,message) VALUES ('recording',?1)",
                            [&error],
                        )
                    }) {
                        eprintln!("ClipForge recording recovery: {error}; log error: {problem}");
                    }
                }
                tauri::async_runtime::spawn(production::supervise(
                    app_handle.clone(),
                    database.clone(),
                ));
                twitch::supervise(app_handle, database).await;
            });

            let open =
                tauri::menu::MenuItem::with_id(app, "open", "Open ClipForge", true, None::<&str>)?;
            let quit = tauri::menu::MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&open, &quit])?;
            let mut tray = tauri::tray::TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("ClipForge Live")
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            twitch::stop(&app).await;
                            app.exit(0);
                        });
                    }
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.hide().is_ok() {
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            dashboard,
            add_creator,
            set_creator_enabled,
            remove_creator,
            save_settings,
            show_clip,
            clip_thumbnail,
            review::preview_clip,
            review::set_clip_review,
            review::trim_clip,
            production::reedit_clip,
            twitch::twitch_status,
            twitch::twitch_begin,
            twitch::twitch_finish,
            twitch::set_monitoring,
            production::setup_model,
            production::model_ready,
            production::model_progress,
            youtube::youtube_connect,
            youtube::youtube_status
        ])
        .run(tauri::generate_context!())
        .expect("ClipForge Live failed to start");
}
