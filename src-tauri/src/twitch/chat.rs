use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub enum Event {
    Joined,
    Raid,
    Message {
        chatter: String,
        text: String,
        emotes: usize,
        sent_ms: Option<i64>,
    },
}

fn parse(line: &str, channel: &str) -> Option<Event> {
    let (tags, message) = if let Some(tagged) = line.strip_prefix('@') {
        tagged.split_once(' ')?
    } else {
        ("", line)
    };
    let (_, message) = message.split_once(' ')?;
    let (verb, arguments) = message.split_once(' ')?;
    let (target, body) = arguments.split_once(" :").unwrap_or((arguments, ""));
    if target.strip_prefix('#')? != channel {
        return None;
    }
    let tag = |name: &str| {
        tags.split(';')
            .filter_map(|field| field.split_once('='))
            .find_map(|(key, value)| (key == name).then_some(value))
    };
    match verb {
        "ROOMSTATE" => Some(Event::Joined),
        "USERNOTICE" if tag("msg-id") == Some("raid") => Some(Event::Raid),
        "PRIVMSG" if !body.is_empty() => Some(Event::Message {
            chatter: tag("user-id")?.to_owned(),
            text: body.to_owned(),
            emotes: tag("emotes")
                .unwrap_or_default()
                .split('/')
                .filter_map(|item| item.split_once(':'))
                .map(|(_, ranges)| ranges.split(',').count())
                .sum(),
            sent_ms: tag("tmi-sent-ts").and_then(|value| value.parse().ok()),
        }),
        _ => None,
    }
}

pub async fn listen(
    channel: &str,
    login: &str,
    access_token: &str,
    events: mpsc::Sender<Event>,
) -> Result<(), String> {
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(15),
        connect_async("wss://irc-ws.chat.twitch.tv:443"),
    )
    .await
    .map_err(|_| "Twitch chat connection timed out.")?
    .map_err(|_| "Could not connect to Twitch chat.")?;
    for command in [
        format!("PASS oauth:{access_token}"),
        format!("NICK {login}"),
        "CAP REQ :twitch.tv/tags twitch.tv/commands".into(),
        format!("JOIN #{channel}"),
    ] {
        socket
            .send(Message::text(format!("{command}\r\n")))
            .await
            .map_err(|_| "Twitch disconnected during chat sign-in.")?;
    }
    let join_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut joined = false;
    loop {
        let message = if joined {
            tokio::time::timeout(Duration::from_secs(420), socket.next()).await
        } else {
            tokio::time::timeout_at(join_deadline, socket.next()).await
        }
        .map_err(|_| "Twitch chat stopped responding.")?
        .ok_or("Twitch chat closed the connection.")?
        .map_err(|_| "Twitch chat connection failed.")?;
        match message {
            Message::Text(batch) => {
                for line in batch.lines() {
                    if let Some(payload) = line.strip_prefix("PING ") {
                        socket
                            .send(Message::text(format!("PONG {payload}\r\n")))
                            .await
                            .map_err(|_| "Twitch chat keepalive failed.")?;
                    } else if line == ":tmi.twitch.tv RECONNECT" {
                        return Err("Twitch requested chat reconnection.".into());
                    } else if line.contains(" NOTICE * :Login authentication failed") {
                        return Err(
                            "Twitch chat rejected the session. Check Twitch sign-in.".into()
                        );
                    } else if let Some(event) = parse(line, channel) {
                        joined |= matches!(&event, Event::Joined);
                        if events.send(event).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .map_err(|_| "Twitch chat keepalive failed.")?,
            Message::Close(_) => return Err("Twitch chat closed the connection.".into()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_messages_with_real_twitch_tags() {
        let line = "@user-id=902;emotes=25:0-4,6-10;tmi-sent-ts=123 :user!nick@server PRIVMSG #clipforge :Kappa Kappa";
        assert!(matches!(parse(line, "clipforge"), Some(Event::Message {
            chatter, text, emotes: 2, sent_ms: Some(123)
        }) if chatter == "902" && text == "Kappa Kappa"));
        assert!(parse(line, "other").is_none());
    }

    #[test]
    fn separates_raids_from_chat() {
        let line = "@msg-id=raid :tmi.twitch.tv USERNOTICE #clipforge :raid";
        assert!(matches!(parse(line, "clipforge"), Some(Event::Raid)));
    }
}
