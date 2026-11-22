use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time};

pub struct Runtime {
    directory: PathBuf,
}

impl Runtime {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn python(&self) -> PathBuf {
        let local = self
            .directory
            .join("runtime")
            .join("Scripts")
            .join("python.exe");
        if local.is_file() {
            return local;
        }
        locate_python().unwrap_or_else(|| "python".into())
    }

    pub fn ffmpeg(&self, name: &str) -> PathBuf {
        find_in_path(name)
            .or_else(|| {
                let root = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
                let tools = root
                    .join("Microsoft/WinGet/Links")
                    .join(format!("{name}.exe"));
                if tools.is_file() {
                    return Some(tools);
                }
                let mut installs: Vec<_> =
                    std::fs::read_dir(root.join("Microsoft/WinGet/Packages"))
                        .ok()?
                        .filter_map(Result::ok)
                        .filter(|entry| {
                            entry
                                .file_name()
                                .to_string_lossy()
                                .starts_with("Gyan.FFmpeg_")
                        })
                        .flat_map(|entry| {
                            std::fs::read_dir(entry.path())
                                .into_iter()
                                .flatten()
                                .filter_map(Result::ok)
                        })
                        .map(|entry| entry.path().join("bin").join(format!("{name}.exe")))
                        .filter(|path| path.is_file())
                        .collect();
                installs.sort();
                installs.pop()
            })
            .unwrap_or_else(|| name.into())
    }

    pub fn media_path(&self) -> Result<OsString, String> {
        let ffmpeg = self.ffmpeg("ffmpeg");
        let paths = std::env::var_os("PATH").unwrap_or_default();
        let directory = ffmpeg.parent().filter(|path| path.is_absolute());
        std::env::join_paths(
            directory
                .map(Path::to_path_buf)
                .into_iter()
                .chain(std::env::split_paths(&paths)),
        )
        .map_err(|error| format!("Cannot add FFmpeg to the media worker environment: {error}"))
    }

    pub async fn setup(&self, requirements: &Path, log: &Path) -> Result<(), String> {
        if locate_python().is_none() {
            install("Python.Python.3.12", log).await?;
        }
        if !self.ffmpeg("ffmpeg").is_file() || !self.ffmpeg("ffprobe").is_file() {
            install("Gyan.FFmpeg", log).await?;
        }
        if !self.ffmpeg("ffmpeg").is_file() || !self.ffmpeg("ffprobe").is_file() {
            return Err("FFmpeg or FFprobe is unavailable after installation. Open ClipForge again and retry setup.".into());
        }
        let python = locate_python().ok_or(
            "Python is unavailable after installation. Open ClipForge again and retry setup.",
        )?;
        let environment = self.directory.join("runtime");
        let local = environment.join("Scripts").join("python.exe");
        if !local.is_file() {
            run(
                &python,
                &[
                    OsStr::new("-m"),
                    OsStr::new("venv"),
                    environment.as_os_str(),
                ],
                log,
            )
            .await?;
        }
        run(
            &local,
            &[
                OsStr::new("-m"),
                OsStr::new("pip"),
                OsStr::new("install"),
                OsStr::new("--disable-pip-version-check"),
                OsStr::new("-r"),
                requirements.as_os_str(),
            ],
            log,
        )
        .await
    }
}

fn locate_python() -> Option<PathBuf> {
    let in_path = find_in_path("python").filter(|path| {
        !path
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("microsoft\\windowsapps")
    });
    if in_path.is_some() {
        return in_path;
    }
    let programs = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Programs/Python");
    let mut choices: Vec<_> = std::fs::read_dir(programs)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("python.exe"))
        .filter(|path| path.is_file())
        .collect();
    choices.sort();
    choices.pop()
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|path| path.join(format!("{name}.exe")))
        .find(|path| path.is_file())
}

async fn install(id: &str, log: &Path) -> Result<(), String> {
    run(
        Path::new("winget"),
        &[
            OsStr::new("install"),
            OsStr::new("--id"),
            OsStr::new(id),
            OsStr::new("--exact"),
            OsStr::new("--silent"),
            OsStr::new("--scope"),
            OsStr::new("user"),
            OsStr::new("--accept-package-agreements"),
            OsStr::new("--accept-source-agreements"),
        ],
        log,
    )
    .await
}

async fn run(program: &Path, args: &[&OsStr], log: &Path) -> Result<(), String> {
    let stdout = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log)
        .map_err(|error| error.to_string())?;
    let stderr = stdout.try_clone().map_err(|error| error.to_string())?;
    let mut command = Command::new(program);
    command
        .kill_on_drop(true)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Cannot start {}: {error}", program.display()))?;
    let status = time::timeout(Duration::from_secs(1800), child.wait())
        .await
        .map_err(|_| format!("Dependency setup timed out. See {}.", log.display()))?
        .map_err(|error| error.to_string())?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "{} exited with {status}. See {}.",
        program.display(),
        log.display()
    ))
}
