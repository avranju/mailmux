use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use imap_next::client::{Client, Event, Options};
use imap_next::imap_types::command::{Command, CommandBody};
use imap_next::imap_types::core::{Tag, Vec1};
use imap_next::imap_types::search::SearchKey;
use imap_next::imap_types::fetch::{
    MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName,
};
use imap_next::imap_types::flag::{Flag, FlagFetch};
use imap_next::imap_types::response::{Code, Data, Status, StatusKind};
use imap_next::imap_types::sequence::SequenceSet;
use imap_next::stream::Stream;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::config::AccountConfig;

const IDLE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for trailing FETCH responses after a tagged OK.
/// Proton Mail Bridge sends the tagged completion before the message data;
/// this grace period lets those responses arrive before we return.
const FETCH_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// A TLS certificate verifier that accepts any certificate without validation.
/// Only use for local bridges (e.g. Proton Mail Bridge) on loopback interfaces.
#[derive(Debug)]
struct NoCertVerifier;

impl ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

/// Wraps an imap-next client+stream pair.
pub struct ImapConnection {
    stream: Stream,
    client: Client,
    tag_counter: u32,
    command_timeout: Duration,
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
            let server_name: ServerName<'_> =
                config.imap_host.clone().try_into().map_err(|e| {
                    anyhow::anyhow!("invalid server name '{}': {e}", config.imap_host)
                })?;

            let tls_config = if config.tls_accept_invalid_certs {
                tracing::warn!(
                    account = %config.id,
                    host = %config.imap_host,
                    "TLS certificate verification is disabled — only use for local bridges on loopback"
                );
                Arc::new(
                    rustls::ClientConfig::builder()
                        .dangerous()
                        .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
                        .with_no_client_auth(),
                )
            } else {
                let mut root_store = rustls::RootCertStore::empty();
                root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

                if let Some(ca_file) = &config.tls_ca_file {
                    let pem = std::fs::read(ca_file)
                        .with_context(|| format!("reading TLS CA file '{ca_file}'"))?;
                    let certs: Vec<CertificateDer<'static>> =
                        rustls_pemfile::certs(&mut pem.as_slice())
                            .collect::<Result<_, _>>()
                            .with_context(|| format!("parsing TLS CA file '{ca_file}'"))?;
                    if certs.is_empty() {
                        bail!("TLS CA file '{ca_file}' contains no certificates");
                    }
                    for cert in certs {
                        root_store.add(cert).with_context(|| {
                            format!("adding certificate from '{ca_file}' to trust store")
                        })?;
                    }
                    debug!(account = %config.id, ca_file, "loaded custom TLS CA certificate(s)");
                }

                Arc::new(
                    rustls::ClientConfig::builder()
                        .with_root_certificates(root_store)
                        .with_no_client_auth(),
                )
            };

            let connector = tokio_rustls::TlsConnector::from(tls_config);
            let tls_stream = connector
                .connect(server_name.to_owned(), tcp)
                .await
                .map_err(|e| anyhow::anyhow!(format!("TLS handshake failed: {e}")))?;
            Stream::tls(tls_stream.into())
        } else {
            Stream::insecure(tcp)
        };

        let client = Client::new(Options::default());
        let command_timeout = Duration::from_secs(config.imap_command_timeout_secs);
        let mut conn = Self {
            stream,
            client,
            tag_counter: 0,
            command_timeout,
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
        let timeout = self.command_timeout;
        tokio::time::timeout(timeout, async {
            loop {
                let event = self
                    .stream
                    .next(&mut self.client)
                    .await
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
        })
        .await
        .unwrap_or_else(|_| bail!("IMAP greeting timed out after {}s", timeout.as_secs()))
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

        let timeout = self.command_timeout;
        tokio::time::timeout(timeout, async {
            loop {
                let event = self
                    .stream
                    .next(&mut self.client)
                    .await
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
        })
        .await
        .unwrap_or_else(|_| bail!("IMAP LOGIN timed out after {}s", timeout.as_secs()))
    }

    /// SELECT a mailbox. Returns (uid_validity, exists_count).
    pub async fn select(&mut self, mailbox: &str) -> Result<(u32, u32)> {
        let tag = self.next_tag();
        let cmd = Command {
            tag,
            body: CommandBody::select(mailbox.to_owned()).context("building SELECT command")?,
        };
        let _handle = self.client.enqueue_command(cmd);

        let mut uid_validity: Option<u32> = None;
        let mut exists: u32 = 0;

        let timeout = self.command_timeout;
        tokio::time::timeout(timeout, async {
            loop {
                let event = self
                    .stream
                    .next(&mut self.client)
                    .await
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
                        if let Some(Code::UidValidity(uv)) = status.code() {
                            uid_validity = Some(uv.get());
                            debug!(mailbox, uid_validity = uv.get(), "UIDVALIDITY");
                        }
                        check_status(&status, "SELECT")?;
                        // Only the tagged response terminates SELECT. Untagged
                        // status lines (e.g. * OK [UNSEEN n]) may arrive
                        // before the tagged completion — keep looping.
                        if matches!(status, Status::Tagged(_)) {
                            break;
                        }
                    }
                    Event::CommandSent { .. } => {}
                    other => {
                        trace!(event = ?other, "ignoring event during SELECT");
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .unwrap_or_else(|_| bail!("IMAP SELECT timed out after {}s", timeout.as_secs()))?;

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
        let sequence_set: SequenceSet = range
            .parse()
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
        let timeout = self.command_timeout;

        loop {
            let event =
                match tokio::time::timeout(timeout, self.stream.next(&mut self.client)).await {
                    Ok(result) => result.context("during UID FETCH")?,
                    Err(_) => bail!(
                        "IMAP UID FETCH timed out after {}s (no data received for {}s)",
                        timeout.as_secs(),
                        timeout.as_secs()
                    ),
                };
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
                    // Only the tagged response terminates the FETCH. Untagged
                    // status lines (e.g. * OK [UIDNEXT n]) may arrive before
                    // the server has finished streaming data — keep looping.
                    if matches!(status, Status::Tagged(_)) {
                        // Some servers (e.g. Proton Mail Bridge) send the
                        // tagged OK before the FETCH data. If nothing has
                        // arrived yet, drain briefly for trailing responses.
                        if messages.is_empty() {
                            while let Ok(Ok(Event::DataReceived {
                                data: Data::Fetch { items, .. },
                            })) = tokio::time::timeout(
                                FETCH_DRAIN_TIMEOUT,
                                self.stream.next(&mut self.client),
                            )
                            .await
                            {
                                if let Some(msg) = parse_fetch_response(items.as_ref()) {
                                    messages.push(msg);
                                }
                            }
                        }
                        break;
                    }
                }
                Event::CommandSent { .. } => {}
                other => {
                    trace!(event = ?other, "ignoring event during UID FETCH");
                }
            }
        }

        Ok(messages)
    }

    /// UID FETCH a range returning only UIDs — no message bodies.
    /// Used for the initial sync scan so we can find the right starting UID
    /// without downloading the full body of every message in the mailbox.
    /// UIDs below `uid_start` are filtered out defensively: some servers
    /// normalise an inverted range (e.g. `101:*` when `*`=100) and return
    /// the last message even though it is below the requested start.
    pub async fn uid_fetch_uid_list(&mut self, uid_start: u32) -> Result<Vec<u32>> {
        let range = format!("{uid_start}:*");
        let tag = self.next_tag();
        let sequence_set: SequenceSet = range
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid UID range: {range}"))?;

        let cmd = Command {
            tag,
            body: CommandBody::Fetch {
                sequence_set,
                macro_or_item_names: MacroOrMessageDataItemNames::MessageDataItemNames(vec![
                    MessageDataItemName::Uid,
                ]),
                uid: true,
            },
        };
        let _handle = self.client.enqueue_command(cmd);

        let mut uids = Vec::new();
        let timeout = self.command_timeout;

        loop {
            let event =
                match tokio::time::timeout(timeout, self.stream.next(&mut self.client)).await {
                    Ok(result) => result.context("during UID FETCH (uid list)")?,
                    Err(_) => bail!(
                        "IMAP UID FETCH (uid list) timed out after {}s",
                        timeout.as_secs()
                    ),
                };
            match event {
                Event::DataReceived { data } => {
                    if let Data::Fetch { items, .. } = data {
                        for item in items.as_ref() {
                            if let MessageDataItem::Uid(u) = item {
                                uids.push(u.get());
                            }
                        }
                    }
                }
                Event::StatusReceived { status } => {
                    check_status(&status, "UID FETCH (uid list)")?;
                    if matches!(status, Status::Tagged(_)) {
                        if uids.is_empty() {
                            while let Ok(Ok(Event::DataReceived {
                                data: Data::Fetch { items, .. },
                            })) = tokio::time::timeout(
                                FETCH_DRAIN_TIMEOUT,
                                self.stream.next(&mut self.client),
                            )
                            .await
                            {
                                for item in items.as_ref() {
                                    if let MessageDataItem::Uid(u) = item {
                                        uids.push(u.get());
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
                Event::CommandSent { .. } => {}
                other => {
                    trace!(event = ?other, "ignoring event during UID FETCH (uid list)");
                }
            }
        }

        // Guard against servers that normalise an inverted range (e.g. `101:*`
        // when `*`=100) and return UIDs below the requested start.
        uids.retain(|&uid| uid >= uid_start);

        Ok(uids)
    }

    /// UID SEARCH SINCE <date> — returns UIDs of messages on or after the given date.
    pub async fn uid_search_since(&mut self, since: chrono::NaiveDate) -> Result<Vec<u32>> {
        let tag = self.next_tag();
        let imap_date = imap_next::imap_types::datetime::NaiveDate::try_from(since)
            .context("converting date for UID SEARCH SINCE")?;
        let criteria = Vec1::from(SearchKey::Since(imap_date));
        let cmd = Command {
            tag,
            body: CommandBody::search(None, criteria, true),
        };
        let _handle = self.client.enqueue_command(cmd);

        let mut uids = Vec::new();
        let timeout = self.command_timeout;
        tokio::time::timeout(timeout, async {
            loop {
                let event = self
                    .stream
                    .next(&mut self.client)
                    .await
                    .context("during UID SEARCH SINCE")?;
                match event {
                    Event::DataReceived { data } => {
                        if let Data::Search(results, ..) = data {
                            for uid in results {
                                uids.push(uid.get());
                            }
                        }
                    }
                    Event::StatusReceived { status } => {
                        check_status(&status, "UID SEARCH SINCE")?;
                        if matches!(status, Status::Tagged(_)) {
                            break;
                        }
                    }
                    Event::CommandSent { .. } => {}
                    other => {
                        trace!(event = ?other, "ignoring event during UID SEARCH SINCE");
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .unwrap_or_else(|_| bail!("IMAP UID SEARCH SINCE timed out after {}s", timeout.as_secs()))?;

        Ok(uids)
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
        let timeout = self.command_timeout;
        tokio::time::timeout(timeout, async {
            loop {
                let event = self
                    .stream
                    .next(&mut self.client)
                    .await
                    .context("during IDLE setup")?;
                match event {
                    Event::IdleCommandSent { .. } => {
                        debug!("IDLE command sent");
                    }
                    Event::IdleAccepted { .. } => {
                        debug!("IDLE accepted, waiting for updates");
                        return Ok(());
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
        })
        .await
        .unwrap_or_else(|_| bail!("IMAP IDLE setup timed out after {}s", timeout.as_secs()))?;

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
                    // Drain until we get the tagged response, with a short timeout
                    // to avoid blocking shutdown indefinitely.
                    let _ = tokio::time::timeout(IDLE_DRAIN_TIMEOUT, async {
                        loop {
                            match self.stream.next(&mut self.client).await {
                                Ok(Event::StatusReceived { .. }) => break,
                                Ok(Event::IdleDoneSent { .. }) => continue,
                                Ok(_) => continue,
                                Err(_) => break,
                            }
                        }
                    }).await;
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

        let timeout = self.command_timeout;
        let _ = tokio::time::timeout(timeout, async {
            loop {
                match self.stream.next(&mut self.client).await {
                    Ok(Event::StatusReceived { .. }) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        })
        .await;

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
            MessageDataItem::BodyExt {
                section: None,
                data,
                ..
            } => {
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
