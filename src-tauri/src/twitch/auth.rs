use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const DEVICE: &str = "https://id.twitch.tv/oauth2/device";
const TOKEN: &str = "https://id.twitch.tv/oauth2/token";
const VALIDATE: &str = "https://id.twitch.tv/oauth2/validate";
const SCOPE: &str = "chat:read";

#[derive(Deserialize, Serialize, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Deserialize, Serialize)]
pub struct DeviceCode {
    #[serde(skip_serializing)]
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
pub struct Identity {
    pub login: String,
    client_id: String,
    scopes: Vec<String>,
}

pub struct Authorization {
    pub client_id: String,
    pub identity: Identity,
    pub tokens: Tokens,
}

pub fn credential(client_id: &str) -> Result<keyring::Entry, String> {
    if client_id.is_empty() || !client_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("Enter the Twitch client ID from your public Twitch application.".into());
    }
    keyring::Entry::new("ClipForge Live / Twitch", client_id).map_err(|error| error.to_string())
}

fn store(client_id: &str, tokens: &Tokens) -> Result<(), String> {
    let serialized = serde_json::to_string(tokens).map_err(|error| error.to_string())?;
    credential(client_id)?
        .set_password(&serialized)
        .map_err(|error| {
            format!("Could not save Twitch credentials in Windows Credential Manager: {error}")
        })
}

pub async fn begin(client: &Client, client_id: &str) -> Result<DeviceCode, String> {
    credential(client_id)?;
    client
        .post(DEVICE)
        .form(&[("client_id", client_id), ("scopes", SCOPE)])
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| format!("Could not start Twitch sign-in: {error}"))?
        .json()
        .await
        .map_err(|_| "Twitch returned an invalid device authorization.".into())
}

pub async fn finish(client: &Client, client_id: &str, device: DeviceCode) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = Duration::from_secs(device.interval.max(1));
    while Instant::now() + interval < deadline {
        tokio::time::sleep(interval).await;
        let response = client
            .post(TOKEN)
            .form(&[
                ("client_id", client_id),
                ("scope", SCOPE),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|_| "Twitch sign-in was interrupted. Start sign-in again.")?;
        if response.status().is_success() {
            let tokens: Tokens = response
                .json()
                .await
                .map_err(|_| "Twitch sent invalid credentials.")?;
            if validate(client, client_id, &tokens).await?.is_none() {
                return Err("Twitch rejected the issued credentials.".into());
            }
            return store(client_id, &tokens);
        }
        let status = response.status();
        let problem: serde_json::Value = response.json().await.unwrap_or_default();
        match problem["message"]
            .as_str()
            .or_else(|| problem["error"].as_str())
        {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            _ => {
                return Err(format!(
                    "Twitch did not authorize the application ({status}). Start sign-in again."
                ))
            }
        }
    }
    Err("The Twitch sign-in code expired. Start sign-in again.".into())
}

async fn validate(
    client: &Client,
    client_id: &str,
    tokens: &Tokens,
) -> Result<Option<Identity>, String> {
    let response = client
        .get(VALIDATE)
        .bearer_auth(&tokens.access_token)
        .send()
        .await
        .map_err(|_| "Cannot reach Twitch to validate the connection.")?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    let identity: Identity = response
        .error_for_status()
        .map_err(|_| "Twitch token validation failed.")?
        .json()
        .await
        .map_err(|_| "Twitch returned an invalid identity.")?;
    if identity.client_id != client_id || !identity.scopes.iter().any(|scope| scope == SCOPE) {
        return Err("Twitch authorization does not grant chat access for this client ID.".into());
    }
    Ok(Some(identity))
}

// The monitor serializes refresh so a rotating refresh token is consumed only once.
pub async fn authorize(client: &Client, client_id: &str) -> Result<Authorization, String> {
    let password = credential(client_id)?
        .get_password()
        .map_err(|_| "Connect a Twitch account in Settings.")?;
    let mut tokens =
        serde_json::from_str(&password).map_err(|_| "Reconnect your Twitch account.")?;
    let identity = match validate(client, client_id, &tokens).await? {
        Some(identity) => identity,
        None => {
            tokens = client
                .post(TOKEN)
                .form(&[
                    ("client_id", client_id),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", tokens.refresh_token.as_str()),
                ])
                .send()
                .await
                .map_err(|_| "Cannot reach Twitch to refresh your session.")?
                .error_for_status()
                .map_err(|_| "Twitch session expired. Reconnect your Twitch account.")?
                .json()
                .await
                .map_err(|_| "Twitch sent an invalid refresh response.")?;
            store(client_id, &tokens)?;
            validate(client, client_id, &tokens)
                .await?
                .ok_or("Twitch rejected the refreshed session.")?
        }
    };
    Ok(Authorization {
        client_id: client_id.into(),
        identity,
        tokens,
    })
}
