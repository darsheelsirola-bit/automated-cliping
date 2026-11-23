mod cleanup;
mod upload;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::Mutex,
    time,
};
use url::Url;

const TOKENS: &str = "https://oauth2.googleapis.com/token";
const SCOPES: &str = "https://www.googleapis.com/auth/youtube.upload https://www.googleapis.com/auth/youtube.readonly";

#[derive(Default)]
pub struct YouTube(Mutex<()>);

#[derive(Deserialize)]
struct DesktopFile {
    installed: DesktopClient,
}

#[derive(Deserialize, Serialize)]
struct DesktopClient {
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize, Serialize)]
struct Account {
    desktop: DesktopClient,
    refresh_token: String,
    channel_id: String,
    channel_title: String,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    refresh_token: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    connected: bool,
    channel_title: Option<String>,
    upload_retry_at: Option<i64>,
}

fn credential() -> Result<keyring::Entry, String> {
    keyring::Entry::new("ClipForge Live / YouTube", "account").map_err(|error| error.to_string())
}

fn account() -> Result<Option<Account>, String> {
    match credential()?.get_password() {
        Ok(json) => serde_json::from_str(&json).map(Some).map_err(|_| {
            "Saved YouTube credentials are unreadable. Reconnect your account.".into()
        }),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!(
            "Could not access Windows Credential Manager: {error}"
        )),
    }
}

#[tauri::command]
pub fn youtube_status(db: State<'_, crate::db::Database>) -> Result<Status, String> {
    let account = account()?;
    let upload_retry_at = if let Some(account) = &account {
        db.connection()
            .and_then(|conn| upload::channel_retry_at(&conn, &account.channel_id))
            .map_err(|error| error.to_string())?
    } else {
        None
    };
    Ok(Status {
        upload_retry_at,
        connected: account.is_some(),
        channel_title: account.map(|value| value.channel_title),
    })
}

fn random_code() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| "Windows could not generate OAuth randomness.")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn http() -> Result<Client, String> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn youtube_connect(
    app: AppHandle,
    state: State<'_, YouTube>,
    client_json: String,
) -> Result<(), String> {
    let _lock = state
        .0
        .try_lock()
        .map_err(|_| "A YouTube sign-in is already running.")?;
    let DesktopFile { installed: desktop } = serde_json::from_str(&client_json)
        .map_err(|_| "Import the JSON downloaded for a Google OAuth Desktop app client.")?;
    if !desktop.client_id.ends_with(".apps.googleusercontent.com")
        || desktop.client_secret.is_empty()
    {
        return Err("This file does not contain a usable Google Desktop app client.".into());
    }
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| format!("Cannot open the local OAuth callback: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth2/callback");
    let state = random_code()?;
    let verifier = random_code()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut authorization =
        Url::parse("https://accounts.google.com/o/oauth2/v2/auth").expect("static OAuth URL");
    authorization.query_pairs_mut().extend_pairs([
        ("client_id", desktop.client_id.as_str()),
        ("redirect_uri", &redirect_uri),
        ("response_type", "code"),
        ("scope", SCOPES),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("state", &state),
        ("code_challenge_method", "S256"),
        ("code_challenge", &challenge),
    ]);
    app.opener()
        .open_url(authorization.as_str(), None::<&str>)
        .map_err(|error| format!("Cannot open browser for Google sign-in: {error}"))?;
    let code = time::timeout(Duration::from_secs(300), callback(listener, &state))
        .await
        .map_err(|_| "Google sign-in timed out. Connect YouTube again.")??;

    let client = http()?;
    let token = token_request(
        &client,
        &desktop,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    let refresh_token = token
        .refresh_token
        .ok_or("Google did not grant offline access. Connect YouTube again.")?;
    let (channel_id, channel_title) = channel(&client, &token.access_token).await?;
    let payload = Account {
        desktop,
        refresh_token,
        channel_id,
        channel_title,
    };
    let json = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
    credential()?
        .set_password(&json)
        .map_err(|error| format!("Could not save YouTube credentials: {error}"))
}

async fn callback(listener: TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(&mut stream);
        let mut line = Vec::new();
        use tokio::io::AsyncReadExt;
        let read = time::timeout(
            Duration::from_secs(3),
            (&mut reader).take(8192).read_until(b'\n', &mut line),
        )
        .await;
        if !matches!(read, Ok(Ok(1..=8191))) {
            continue;
        }
        let Ok(line) = std::str::from_utf8(&line) else {
            continue;
        };
        let Some(path) = line
            .strip_prefix("GET ")
            .and_then(|value| value.split_whitespace().next())
        else {
            continue;
        };
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{path}")) else {
            continue;
        };
        if url.path() != "/oauth2/callback" {
            continue;
        }
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        if pairs.get("state").map(String::as_str) != Some(state) {
            continue;
        }
        let body = "Return to ClipForge Live to see your YouTube connection status.";
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let _ = time::timeout(
            Duration::from_secs(2),
            stream.write_all(response.as_bytes()),
        )
        .await;
        return pairs
            .get("code")
            .cloned()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "Google sign-in was declined or did not return a code.".into());
    }
}

async fn token_request(
    client: &Client,
    desktop: &DesktopClient,
    extra: &[(&str, &str)],
) -> Result<Token, String> {
    let mut form = vec![
        ("client_id", desktop.client_id.as_str()),
        ("client_secret", desktop.client_secret.as_str()),
    ];
    form.extend_from_slice(extra);
    let response = client
        .post(TOKENS)
        .form(&form)
        .send()
        .await
        .map_err(|_| "Google sign-in service is unreachable.")?;
    let code = response.status();
    response
        .error_for_status()
        .map_err(|_| {
            format!("Google token exchange failed ({code}). Reconnect YouTube if access expired.")
        })?
        .json()
        .await
        .map_err(|_| "Google returned an invalid token response.".into())
}

async fn channel(client: &Client, token: &str) -> Result<(String, String), String> {
    let response: serde_json::Value = client
        .get("https://www.googleapis.com/youtube/v3/channels")
        .query(&[("part", "snippet"), ("mine", "true")])
        .bearer_auth(token)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| {
            "Cannot read your YouTube channel. Enable YouTube Data API v3 and check channel access."
        })?
        .json()
        .await
        .map_err(|_| "YouTube returned an invalid channel response.")?;
    let channels = response["items"]
        .as_array()
        .ok_or("No YouTube channel was returned.")?;
    if channels.len() != 1 {
        return Err("Connect the Google identity for exactly one YouTube channel.".into());
    }
    let id = channels[0]["id"]
        .as_str()
        .ok_or("YouTube did not return the channel ID.")?;
    let title = channels[0]["snippet"]["title"]
        .as_str()
        .ok_or("YouTube did not return the channel title.")?;
    Ok((id.to_owned(), title.to_owned()))
}

pub use upload::supervise;

pub(crate) fn metadata_path(output: &Path) -> Result<std::path::PathBuf, String> {
    Ok(output
        .parent()
        .ok_or("The finished file has no storage folder.")?
        .join(if output.file_name().is_some_and(|name| name == "short-reedited.mp4") {
            "metadata-reedited.json"
        } else {
            "metadata.json"
        }))
}
