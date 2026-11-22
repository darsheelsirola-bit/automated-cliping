# ClipForge Live

ClipForge Live is a local Windows 11 desktop application for monitoring Twitch, recording stream footage, transcribing highlights with Whisper Large-v3, rendering vertical previews and Shorts, and uploading eligible clips with the YouTube Data API.

## Install and set up

1. Run the x64 NSIS installer. The installer uses WebView2 for the desktop interface.
2. Open **Settings**. Install Whisper Large-v3. Setup uses a local Python environment and provisions Python or FFmpeg through Windows Package Manager when needed. Allow space for the 3.09 GB model weights, the Python runtime, and temporary recordings. Setup verifies the SHA-256 of the pinned model weights before marking the model ready.
3. Register a public Twitch application in the [Twitch Developer Console](https://dev.twitch.tv/console/apps). Enter its client ID in **Settings**, open Twitch from the device-code prompt, and grant chat read access. No Twitch app secret is required. [Twitch authentication documentation](https://dev.twitch.tv/docs/authentication/getting-tokens-oauth/#device-code-grant-flow)
4. In a Google Cloud project, enable **YouTube Data API v3**, configure the consent screen, and create a **Desktop app** OAuth client. Add your account as a test user if the consent project requires it. Download its JSON and select the file under **Settings → YouTube**. Finish consent in your browser using the Google identity for the destination channel. [Google desktop OAuth documentation](https://developers.google.com/identity/protocols/oauth2/native-app)
5. Choose the storage directory, clip duration range and thresholds. Add creator names or Twitch channel URLs in **Creators**. Enable **Automatically publish eligible clips** when ready to send new and queued eligible clips to the connected channel. Click **Start monitoring**.
6. Close the window to keep working in the tray. Use **Quit** from the tray to exit. Windows startup and keeping the computer awake during recording are opt-in settings.

ClipForge combines the live trigger score and the strength of a sustained, voice-like reaction in the source audio into a separate publishing confidence score. Automatic uploads also require clearly transcribed speech overlapping the reaction. Wordless reactions are available as local clips. Automatic publishing requires at least 80 out of 100 or the higher minimum in Settings. Earlier clips without the current evidence cannot start new upload sessions. Captions sit higher in the vertical frame to leave room for the Shorts interface. Titles can quote speech near the reaction, and descriptions include creator and clip hashtags. For new uploads, ClipForge removes the local master and rendered files after YouTube reports that the matching video on the connected channel is public and processed. Publishing History retains the video ID.

Uploads request **Public** visibility. YouTube controls account eligibility, quotas, processing, and actual visibility. A final upload whose outcome cannot be established is held in the publishing history so ClipForge will not start a second upload for the same clip. [YouTube resumable uploads](https://developers.google.com/youtube/data/guides/using_resumable_upload_protocol)

## Build on Windows

Install Node.js and npm, Rust stable with the MSVC toolchain, Visual Studio 2022 Build Tools with Desktop development with C++ and a Windows SDK, and WebView2. Use PowerShell from the repository root.

```powershell
npm.cmd ci
npm.cmd run build
cargo test --manifest-path src-tauri/Cargo.toml
python -m unittest discover -s src-tauri/worker -p 'test_*.py'
npm.cmd run tauri -- build --bundles nsis
```

The installer appears under `src-tauri/target/release/bundle/nsis`. To run the development app, use `npm.cmd run tauri -- dev`. If Windows Package Manager is unavailable on an installed machine, install Python 3.10–3.12 and FFmpeg/FFprobe, then run the model setup again. The app installs its Python worker dependencies into a per-user environment. The installer is unsigned.

## Components and data

- The Tauri/React desktop app reads and writes SQLite through Rust commands. The database uses WAL mode and versioned migrations. OAuth credentials are stored in Windows Credential Manager.
- Twitch Helix provides live status. The monitor receives IRC over TLS and tracks chat activity, repeated messages, unique participants and emotes. The detector tracks sustained, voice-like rises above the recent audio level. A strong reaction can trigger a clip even in quiet chat. Raid and reconnect windows suppress triggers. Its score ranks candidates.
- Streamlink and FFmpeg record the best accessible stream into a rolling segmented buffer without a video re-encode. The queue checks for a contiguous lead-in and keeps the original source tracks in a separate master. The saved budget defaults to 20 GB. Pending or failed clips can retain temporary segments because they may still contain the only source footage.
- A single local processing worker runs faster-whisper with the pinned Large-v3 model on CPU. It produces word timestamps, SRT and ASS captions, a thumbnail, and a 1080 × 1920 H.264/AAC MP4. It selects both clip boundaries from audio activity, pauses and word timing around the trigger. The default export range is 10 to 45 seconds. A face that stays in a consistent position near the edge of the stream can get an enlarged panel above the gameplay. The fallback keeps the full source frame. FFprobe and a decode check validate the export before queuing it for upload.
- SQLite stores resumable upload state, server-acknowledged progress, recovery state, and the YouTube video ID. An in-progress upload continues from its recorded session after restart. Background and media processes require the computer to be awake and online.

## End-to-end acceptance

1. Install the installer on Windows and complete both OAuth connections. Confirm model setup and the destination channel shown in Settings.
2. Add three Twitch creators with permission to use their footage. Start monitoring while all three are live. Confirm joined chat, growing chart samples, and active recording for each.
3. Use a known event and verify the highlight against the source. Confirm the original master, lead-in, audio, subtitles, and 1080 × 1920 export. Compare the video timing against Twitch independently because chat and stream delivery can have different latency.
4. Enable automatic publishing with a channel approved for the run. Confirm a video ID appears in Publishing History, open it in YouTube Studio, and verify its processing and visibility.
5. Repeat with interrupted network, app restart, a low saved budget, and a stopped stream. Confirm jobs and footage survive and that a resumed upload yields a single video ID.

## Known limits

Capture indexes timestamps relative to when video begins arriving locally. Twitch delivery latency can offset the resulting clip from the moment viewers reacted to. Frame-accurate Twitch wall-clock alignment has not been established. On restart, segment rescan covers the configured recording directory. Keep that directory selected until pending clips are processed. Boundary selection does not currently model individual scene transitions or merge separate nearby highlights. Color, caption styling, and layout are currently built in. This build has not established three-stream throughput on the target hardware or live Twitch and YouTube acceptance against an authorized account.

