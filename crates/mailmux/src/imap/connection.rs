use std::sync::Arc;

use anyhow::{Context, Result, bail};
use imap_next::client::{Client, Event, Options};
use imap_next::imap_types::command::{Command, CommandBody};
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::fetch::{MessageDataItem, MessageDataItemName, MacroOrMessageDataItemNames};
use imap_next::imap_types::flag::{Flag, FlagFetch};
use imap_next::imap_types::response::{Code, Data, Status, StatusKind};
use imap_next::imap_types::sequence::SequenceSet;
use imap_next::stream::Stream;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::config::AccountConfig;

/// Wraps an imap-next client+stream pair.
pub struct ImapConnection {
    stream: Stream,
    client: Client,
    tag_counter: u32,
}

impl ImapConnection {
    /// Connect to an IMAP server, wait for greeting, and login.
    pub async fn connect(config: &AccountConfig) -> Result<Self> {
        let addr = format!("{}:{}", config.imap_host, config.imap_port);
        debug!(host = %config.imap_host, port = config.imap_port, tls = config.tls, "connecting to IMAP server");

        let tcp = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("connecting to {addr}"))?;

        let stream = if config.tls {
            let server_name: rustls::pki_types::ServerName<'_> = config.imap_host.clone().try_into()
                .map_err(|e| anyhow::anyhow!("invalid server name '{}': {e}", config.imap_host))?;
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let tls_config = Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(root_store)
                    .with_no_client_auth(),
            );
            let connector = tokio_rustls::TlsConnector::from(tls_config);
            let tls_stream = connector.connect(server_name.to_owned(), tcp).await
                .context("TLS handshake failed")?;
            Stream::tls(tls_stream.into())
        } else {
            Stream::insecure(tcp)
        };

        let client = Client::new(Options::default());
        let mut conn = Self {
            stream,
            client,
            tag_counter: 0,
        };

        // Wait for server greeting
        conn.wait_for_greeting().await?;

        // Login (clone strings to satisfy 'static lifetime requirements)
        let username = config.username.clone();
        let password = config.password.clone();
        conn.login(&username, &password).await?;

        Ok(conn)
    }

    fn next_tag(&mut self) -> Tag<'static> {
        self.tag_counter += 1;
        Tag::try_from(format!("M{}", self.tag_counter)).expect("valid tag")
    }

    async fn wait_for_greeting(&mut self) -> Result<()> {
        loop {
            let event = self.stream.next(&mut self.client).await
                .context("waiting for server greeting")?;
            match event {
                Event::GreetingReceived { greeting } => {
                    debug!(greeting = ?greeting.kind, "received server greeting");
                    return Ok(());
                }
                other => {
                    trace!(event = ?other, "ignoring event while waiting for greeting");
                }
            }
        }
    }

    async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        let tag = self.next_tag();
        // Clone into owned strings so the Command is 'static
        let cmd = Command {
            tag,
            body: CommandBody::login(username.to_owned(), password.to_owned())
                .context("building LOGIN command")?,
        };
        let _handle = self.client.enqueue_command(cmd);

        loop {
            let event = self.stream.next(&mut self.client).await
                .context("during LOGIN")?;
            match event {
                Event::StatusReceived { status } => {
                    return check_status(&status, "LOGIN");
                }
                Event::CommandSent { .. } => {
                    debug!("LOGIN command sent");
                }
                Event::DataReceived { .. } => {}
                other => {
                    trace!(event = ?other, "ignoring event during LOGIN");
                }
            }
        }
    }

    /// SELECT a mailbox. Returns (uid_validity, exists_count).
    pub async fn select(&mut self, mailbox: &str) -> Result<(u32, u32)> {
        let tag = self.next_tag();
        let cmd = Command {
            tag,
            body: CommandBody::select(mailbox.to_owned())
                .context("building SELECT command")?,
        };
        let _handle = self.client.enqueue_command(cmd);

        let mut uid_validity: Option<u32> = None;
        let mut exists: u32 = 0;

        loop {
            let event = self.stream.next(&mut self.client).await
                .context("during SELECT")?;
            match event {
                Event::DataReceived { data } => {
                    if let Data::Exists(n) = data {
                        exists = n;
                        debug!(mailbox, exists = n, "mailbox EXISTS");
                    }
                }
                Event::StatusReceived { status } => {
                    // Check for UIDVALIDITY in the status code
                    if let Some(code) = status.code() {
                        if let Code::UidValidity(uv) = code {
                            uid_validity = Some(uv.get());
                            debug!(mailbox, uid_validity = uv.get(), "UIDVALIDITY");
                        }
                    }
                    check_status(&status, "SELECT")?;
                    break;
                }
                Event::CommandSent { .. } => {}
                other => {
                    trace!(event = ?other, "ignoring event during SELECT");
                }
            }
        }

        let uid_validity = uid_validity.unwrap_or(0);
        Ok((uid_validity, exists))
    }

    /// UID FETCH messages by UID range. Returns Vec<FetchedMessage>.
    pub async fn uid_fetch_range(
        &mut self,
        uid_start: u32,
        uid_end: Option<u32>,
    ) -> Result<Vec<FetchedMessage>> {
        let range = match uid_end {
            Some(end) => format!("{uid_start}:{end}"),
            None => format!("{uid_start}:*"),
        };

        let tag = self.next_tag();
        let sequence_set: SequenceSet = range.parse()
            .map_err(|_| anyhow::anyhow!("invalid UID range: {range}"))?;

        let fetch_attrs = MacroOrMessageDataItemNames::MessageDataItemNames(vec![
            MessageDataItemName::Uid,
            MessageDataItemName::Flags,
            MessageDataItemName::BodyExt {
                section: None,
                partial: None,
                peek: true,
            },
            MessageDataItemName::Rfc822Size,
        ]);

        let cmd = Command {
            tag,
            body: CommandBody::Fetch {
                sequence_set,
                macro_or_item_names: fetch_attrs,
                uid: true,
            },
        };
        let _handle = self.client.enqueue_command(cmd);

        let mut messages = Vec::new();

        loop {
            let event = self.stream.next(&mut self.client).await
                .context("during UID FETCH")?;
            match event {
                Event::DataReceived { data } => {
                    if let Data::Fetch { seq: _, items } = data {
                        let msg = parse_fetch_response(items.as_ref());
                        if let Some(msg) = msg {
                            messages.push(msg);
                        }
                    }
                }
                Event::StatusReceived { status } => {
                    check_status(&status, "UID FETCH")?;
                    break;
                }
                Event::CommandSent { .. } => {}
                other => {
                    trace!(event = ?other, "ignoring event during UID FETCH");
                }
            }
        }

        Ok(messages)
    }

    /// Enter IMAP IDLE mode. Returns when the server sends an update
    /// (EXISTS, EXPUNGE, etc.) or the token is cancelled.
    /// Returns `true` if an update was received, `false` if cancelled.
    pub async fn idle(&mut self, token: &CancellationToken) -> Result<bool> {
        let tag = self.next_tag();
        let cmd = Command {
            tag,
            body: CommandBody::Idle,
        };
        let _handle = self.client.enqueue_command(cmd);

        // Wait for IDLE to be accepted
        loop {
            let event = self.stream.next(&mut self.client).await
                .context("during IDLE setup")?;
            match event {
                Event::IdleCommandSent { .. } => {
                    debug!("IDLE command sent");
                }
                Event::IdleAccepted { .. } => {
                    debug!("IDLE accepted, waiting for updates");
                    break;
                }
                Event::IdleRejected { .. } => {
                    bail!("IDLE rejected by server");
                }
                Event::StatusReceived { status } => {
                    check_status(&status, "IDLE")?;
                }
                other => {
                    trace!(event = ?other, "ignoring event during IDLE setup");
                }
            }
        }

        // Now in IDLE mode — wait for updates or cancellation
        let got_update = loop {
            tokio::select! {
                event = self.stream.next(&mut self.client) => {
                    match event.context("during IDLE")? {
                        Event::DataReceived { data } => {
                            match data {
                                Data::Exists(_) | Data::Expunge(_) | Data::Recent(_) => {
                                    debug!(data = ?data, "IDLE received mailbox update");
                                    // Send DONE to exit IDLE
                                    self.client.set_idle_done();
                                    break true;
                                }
                                _ => {
                                    trace!(data = ?data, "IDLE received non-update data");
                                }
                            }
                        }
                        Event::IdleDoneSent { .. } => {
                            debug!("IDLE DONE sent");
                        }
                        Event::StatusReceived { .. } => {
                            // Tagged OK after DONE
                            break true;
                        }
                        other => {
                            trace!(event = ?other, "ignoring event during IDLE");
                        }
                    }
                }
                _ = token.cancelled() => {
                    debug!("IDLE cancelled by shutdown");
                    self.client.set_idle_done();
                    // Drain until we get the tagged response
                    loop {
                        match self.stream.next(&mut self.client).await {
                            Ok(Event::StatusReceived { .. }) => break,
                            Ok(Event::IdleDoneSent { .. }) => continue,
                            Ok(_) => continue,
                            Err(_) => break,
                        }
                    }
                    break false;
                }
            }
        };

        Ok(got_update)
    }

    /// Send LOGOUT.
    pub async fn logout(&mut self) -> Result<()> {
        let tag = self.next_tag();
        let cmd = Command {
            tag,
            body: CommandBody::Logout,
        };
        let _handle = self.client.enqueue_command(cmd);

        loop {
            match self.stream.next(&mut self.client).await {
                Ok(Event::StatusReceived { .. }) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }

        Ok(())
    }
}

/// A fetched message with UID, flags, and raw bytes.
#[derive(Debug)]
pub struct FetchedMessage {
    pub uid: u32,
    pub flags: Vec<String>,
    pub raw_bytes: Vec<u8>,
    pub size: Option<u32>,
}

fn parse_fetch_response(items: &[MessageDataItem]) -> Option<FetchedMessage> {
    let mut uid: Option<u32> = None;
    let mut flags: Vec<String> = Vec::new();
    let mut raw_bytes: Option<Vec<u8>> = None;
    let mut size: Option<u32> = None;

    for item in items {
        match item {
            MessageDataItem::Uid(u) => {
                uid = Some(u.get());
            }
            MessageDataItem::Flags(f) => {
                for flag in f {
                    flags.push(flag_fetch_to_string(flag));
                }
            }
            MessageDataItem::BodyExt { section: None, data, .. } => {
                if let Some(bytes) = data.clone().into_option() {
                    raw_bytes = Some(bytes.into_owned());
                }
            }
            MessageDataItem::Rfc822(nstring) => {
                if let Some(bytes) = nstring.clone().into_option() {
                    raw_bytes = Some(bytes.into_owned());
                }
            }
            MessageDataItem::Rfc822Size(s) => {
                size = Some(*s);
            }
            _ => {}
        }
    }

    let uid = uid?;
    let raw_bytes = raw_bytes?;

    Some(FetchedMessage {
        uid,
        flags,
        raw_bytes,
        size,
    })
}

fn flag_fetch_to_string(ff: &FlagFetch<'_>) -> String {
    match ff {
        FlagFetch::Flag(flag) => flag_to_string(flag),
        FlagFetch::Recent => "\\Recent".to_string(),
    }
}

fn flag_to_string(flag: &Flag<'_>) -> String {
    match flag {
        Flag::Answered => "\\Answered".to_string(),
        Flag::Deleted => "\\Deleted".to_string(),
        Flag::Draft => "\\Draft".to_string(),
        Flag::Flagged => "\\Flagged".to_string(),
        Flag::Seen => "\\Seen".to_string(),
        other => format!("{other:?}"),
    }
}

fn check_status(status: &Status, operation: &str) -> Result<()> {
    match status {
        Status::Tagged(tagged) => match tagged.body.kind {
            StatusKind::Ok => Ok(()),
            StatusKind::No => bail!("{operation} rejected: {}", tagged.body.text),
            StatusKind::Bad => bail!("{operation} error: {}", tagged.body.text),
        },
        Status::Untagged(body) => match body.kind {
            StatusKind::Ok => Ok(()),
            StatusKind::No => bail!("{operation} rejected (untagged): {}", body.text),
            StatusKind::Bad => bail!("{operation} error (untagged): {}", body.text),
        },
        Status::Bye(bye) => {
            bail!("{operation}: server sent BYE: {}", bye.text)
        }
    }
}
