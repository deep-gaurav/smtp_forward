use anyhow::{Context, Result};
use mail_parser::{MessageParser, MimeHeaders};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::schema::{Contact, Content, Message};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mail {
    pub from: String,
    pub to: Vec<String>,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Fresh,
    Greeted,
    ReceivingRcpt(Mail),
    ReceivingData(Mail),
    Received(Mail),
}

struct StateMachine {
    state: State,
    ehlo_greeting: String,
}

/// An state machine capable of handling SMTP commands
/// for receiving mail.
/// Use handle_smtp() to handle a single command.
/// The return value from handle_smtp() is the response
/// that should be sent back to the client.
impl StateMachine {
    const OH_HAI: &[u8] = b"220 edgemail\n";
    const KK: &[u8] = b"250 Ok\n";
    const AUTH_OK: &[u8] = b"235 Ok\n";
    const SEND_DATA_PLZ: &[u8] = b"354 End data with <CR><LF>.<CR><LF>\n";
    const KTHXBYE: &[u8] = b"221 Bye\n";
    const HOLD_YOUR_HORSES: &[u8] = &[];

    pub fn new(domain: impl AsRef<str>) -> Self {
        tracing::trace!("New state machine initialized");
        let domain = domain.as_ref();
        let ehlo_greeting = format!("250-{domain} Hello {domain}\n250 AUTH PLAIN LOGIN\n");
        Self {
            state: State::Fresh,
            ehlo_greeting,
        }
    }

    /// Handles a single SMTP command and returns a proper SMTP response
    pub fn handle_smtp(&mut self, raw_msg: &str) -> Result<&[u8]> {
        tracing::trace!("Received {raw_msg} in state {:?}", self.state);
        let mut msg = raw_msg.split_whitespace();
        let command = msg.next().context("received empty command")?.to_lowercase();
        let state = self.state.clone();
        match (command.as_str(), state) {
            ("ehlo", State::Fresh) => {
                tracing::trace!("Sending AUTH info");
                self.state = State::Greeted;
                Ok(self.ehlo_greeting.as_bytes())
            }
            ("helo", State::Fresh) => {
                self.state = State::Greeted;
                Ok(StateMachine::KK)
            }
            ("noop", _) | ("help", _) | ("info", _) | ("vrfy", _) | ("expn", _) => {
                tracing::trace!("Got {command}");
                Ok(StateMachine::KK)
            }
            ("rset", _) => {
                self.state = State::Fresh;
                Ok(StateMachine::KK)
            }
            ("auth", _) => {
                tracing::trace!("Acknowledging AUTH");
                Ok(StateMachine::AUTH_OK)
            }
            ("mail", State::Greeted) => {
                tracing::trace!("Receiving MAIL");
                let from = msg.next().context("received empty MAIL")?;
                let from = from
                    .strip_prefix("FROM:")
                    .context("received incorrect MAIL")?;
                tracing::debug!("FROM: {from}");
                self.state = State::ReceivingRcpt(Mail {
                    from: from.to_string(),
                    ..Default::default()
                });
                Ok(StateMachine::KK)
            }
            ("rcpt", State::ReceivingRcpt(mut mail)) => {
                tracing::trace!("Receiving rcpt");
                let to = msg.next().context("received empty RCPT")?;
                let to = to.strip_prefix("TO:").context("received incorrect RCPT")?;
                tracing::debug!("TO: {to}");
                if Self::legal_recipient(to) {
                    mail.to.push(to.to_string());
                } else {
                    tracing::warn!("Illegal recipient: {to}")
                }
                self.state = State::ReceivingRcpt(mail);
                Ok(StateMachine::KK)
            }
            ("data", State::ReceivingRcpt(mail)) => {
                tracing::trace!("Receiving data");
                self.state = State::ReceivingData(mail);
                Ok(StateMachine::SEND_DATA_PLZ)
            }
            ("quit", State::ReceivingData(mail)) => {
                tracing::trace!(
                    "Received data: FROM: {} TO:{} DATA:{}",
                    mail.from,
                    mail.to.join(", "),
                    mail.data
                );
                self.state = State::Received(mail);
                Ok(StateMachine::KTHXBYE)
            }
            ("quit", _) => {
                tracing::warn!("Received quit before getting any data");
                Ok(StateMachine::KTHXBYE)
            }
            (_, State::ReceivingData(mut mail)) => {
                tracing::trace!("Receiving data");
                let resp = if raw_msg.ends_with("\r\n.\r\n") {
                    StateMachine::KK
                } else {
                    StateMachine::HOLD_YOUR_HORSES
                };
                mail.data += raw_msg;
                self.state = State::ReceivingData(mail);
                Ok(resp)
            }
            (msg, state) => {
                tracing::trace!(
                    "Bailing out: Unexpected message received in state {state:?}: {msg}"
                );
                anyhow::bail!(
                    "Unexpected message received in state {:?}: {raw_msg}",
                    self.state
                )
            }
        }
    }

    /// Filter out admin, administrator, postmaster and hostmaster
    /// to prevent being able to register certificates for the domain.
    /// The check is over-eager, but it also makes it simpler.
    fn legal_recipient(to: &str) -> bool {
        let to = to.to_lowercase();
        !to.contains("admin") && !to.contains("postmaster") && !to.contains("hostmaster")
    }
}

/// SMTP server, which handles user connections
/// and replicates received messages to the database.
pub struct Server {
    stream: tokio::net::TcpStream,
    state_machine: StateMachine,
}

impl Server {
    /// Creates a new server from a connected stream
    pub async fn new(domain: impl AsRef<str>, stream: tokio::net::TcpStream) -> Result<Self> {
        Ok(Self {
            stream,
            state_machine: StateMachine::new(domain),
        })
    }

    /// Runs the server loop, accepting and handling SMTP commands
    pub async fn serve(mut self) -> Result<()> {
        self.greet().await?;

        let mut buf = vec![0; 65536];
        loop {
            let n = self.stream.read(&mut buf).await?;

            if n == 0 {
                tracing::info!("Received EOF");
                self.state_machine.handle_smtp("quit").ok();
                break;
            }
            let msg = std::str::from_utf8(&buf[0..n])?;
            let response = self.state_machine.handle_smtp(msg)?;
            if response != StateMachine::HOLD_YOUR_HORSES {
                self.stream.write_all(response).await?;
            } else {
                tracing::debug!("Not responding, awaiting more data");
            }
            if response == StateMachine::KTHXBYE {
                break;
            }
        }
        tracing::trace!("State machine exited {:?}", self.state_machine.state);
        match self.state_machine.state {
            State::Received(mail) => 'rec: {
                tracing::info!("Sending mail");
                tracing::info!("{mail:?}");
                let data = MessageParser::default().parse(&mail.data);
                if let Some(data) = data {
                    let Some(from) = data.from() else {
                        break 'rec;
                    };
                    let from = from.clone().into_list();
                    if from.len() != 1 {
                        tracing::warn!("From length not supported");
                        break 'rec;
                    }
                    let from = from.first().unwrap();
                    let Some(email) = &from.address else {
                        tracing::warn!("From ??");
                        break 'rec;
                    };
                    let from = Contact {
                        email: Some(email.to_string()),
                        name: from.name().map(|e| e.to_string()),
                    };
                    let to = data
                        .to()
                        .map(|to| to.clone().into_list())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|address| Contact {
                            email: address.address.map(|e| e.to_string()),
                            name: address.name.map(|e| e.to_string()),
                        })
                        .collect::<Vec<_>>();
                    let cc = data
                        .cc()
                        .map(|to| to.clone().into_list())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|address| Contact {
                            email: address.address.map(|e| e.to_string()),
                            name: address.name.map(|e| e.to_string()),
                        })
                        .collect::<Vec<_>>();
                    let bcc = data
                        .bcc()
                        .map(|to| to.clone().into_list())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|address| Contact {
                            email: address.address.map(|e| e.to_string()),
                            name: address.name.map(|e| e.to_string()),
                        })
                        .collect::<Vec<_>>();
                    let reply_to = data
                        .reply_to()
                        .map(|to| to.clone().into_list())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|address| Contact {
                            email: address.address.map(|e| e.to_string()),
                            name: address.name.map(|e| e.to_string()),
                        })
                        .collect::<Vec<_>>();
                    let subject = data.subject().map(|e|e.to_string());

                    let content = data.parts.into_iter()
                        .map(
                            |part|Content{
                                value:part.text_contents().map(|e|e.to_string()),
                                mime:part.content_type().map(|e|
                                    if let Some(subtyp) = &e.c_subtype{
                                        format!("{}/{}",e.c_type,subtyp)
                                    }else{
                                        format!("{}",e.c_type)
                                    }
                                )
                            }
                        ).collect::<Vec<_>>()
                    ;

                    let message = Message{
                        from,
                        to,
                        reply_to,
                        cc,
                        bcc,
                        subject,
                        content
                    };
                    tracing::trace!("Sending {message:?}");
                    let json = serde_json::to_string(&message);
                    let Ok(json) = json else{
                        break 'rec;
                    };
                    let client = reqwest::Client::new();
                    let resp = client.post("https://worker-email-production.deepgauravraj.workers.dev/api/email")
                        .header("Content-Type", "application/json")
                        .header("Authorization", std::env::var("EMAIL_TOKEN").unwrap_or_default())
                        .body(json)
                        .send().await;
                    match resp {
                        Ok(resp) => {
                            let resp =  resp.text().await.unwrap_or_default();
                            tracing::debug!("RECEIVED SEND Response {resp}")
                        },
                        Err(err) => tracing::warn!("SEND ERROR {err:?}"),
                    }
                } else {
                    tracing::warn!("Cant parse message, discarding")
                }
                // self.db.lock().await.replicate(mail).await?;
            }
            State::ReceivingData(mail) => {
                tracing::info!("Received EOF before receiving QUIT");
                tracing::info!("Discarding mail EOF");
                tracing::info!("{mail:?}");
                // self.db.lock().await.replicate(mail).await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Sends the initial SMTP greeting
    async fn greet(&mut self) -> Result<()> {
        self.stream
            .write_all(StateMachine::OH_HAI)
            .await
            .map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regular_flow() {
        let mut sm = StateMachine::new("dummy");
        assert_eq!(sm.state, State::Fresh);
        sm.handle_smtp("HELO localhost").unwrap();
        assert_eq!(sm.state, State::Greeted);
        sm.handle_smtp("MAIL FROM: <local@example.com>").unwrap();
        assert!(matches!(sm.state, State::ReceivingRcpt(_)));
        sm.handle_smtp("RCPT TO: <a@localhost.com>").unwrap();
        assert!(matches!(sm.state, State::ReceivingRcpt(_)));
        sm.handle_smtp("RCPT TO: <b@localhost.com>").unwrap();
        assert!(matches!(sm.state, State::ReceivingRcpt(_)));
        sm.handle_smtp("DATA hello world\n").unwrap();
        assert!(matches!(sm.state, State::ReceivingData(_)));
        sm.handle_smtp("DATA hello world2\n").unwrap();
        assert!(matches!(sm.state, State::ReceivingData(_)));
        sm.handle_smtp("QUIT").unwrap();
        assert!(matches!(sm.state, State::Received(_)));
    }

    #[test]
    fn test_no_greeting() {
        let mut sm = StateMachine::new("dummy");
        assert_eq!(sm.state, State::Fresh);
        for command in [
            "MAIL FROM: <local@example.com>",
            "RCPT TO: <local@example.com>",
            "DATA hey",
            "GARBAGE",
        ] {
            assert!(sm.handle_smtp(command).is_err());
        }
    }
}
