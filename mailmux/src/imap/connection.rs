use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use imap_next::client::{Client, Event, Options};
use imap_next::imap_types::command::{Command, CommandBody};
use imap_next::imap_types::core::{Tag, Vec1};
use imap_next::imap_types::fetch::{
    MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName,
};
use imap_next::imap_types::flag::{Flag, FlagFetch};
use imap_next::imap_types::response::{Code, Data, Status, StatusKind};
use imap_next::imap_types::search::SearchKey;
use imap_next::imap_types::sequence::SequenceSet;
use imap_next::stream::Stream;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use super::error::ImapFailure;
use crate::config::AccountConfig;

// Bound aggregate command results as well as each individual wire response.
const MAX_RESPONSE_ITEMS: usize = 1_000_000;
const MAX_FETCH_BYTES: usize = 128 * 1024 * 1024;
const MAX_WIRE_RESPONSE_BYTES: u32 = 100 * 1024 * 1024;

#[derive(Default)]
struct ResponseBudget {
    items: usize,
    bytes: usize,
}

impl ResponseBudget {
    fn reserve(&mut self, bytes: usize) -> Result<()> {
        if self.items >= MAX_RESPONSE_ITEMS || bytes > MAX_FETCH_BYTES - self.bytes {
            tracing::warn!(
                items = self.items,
                bytes = self.bytes,
                "IMAP response limit exceeded"
            );
            bail!("IMAP response limit exceeded; command results discarded");
        }
        self.items += 1;
        self.bytes += bytes;
        Ok(())
    }
}

/// Outcome of an IDLE wait.
#[derive(Debug, PartialEq, Eq)]
pub enum IdleOutcome {
    /// The server sent an EXISTS/EXPUNGE/RECENT notification.
    Update,
    /// The heartbeat timer fired before any server notification arrived.
    Heartbeat,
    /// A shutdown was requested via the cancellation token.
    Shutdown,
}

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
    idle_drain_timeout: Duration,
    idle_inactivity_timeout: Duration,
    fetch_drain_timeout: Duration,
}

impl ImapConnection {
    /// Connect to an IMAP server, wait for greeting, and login.
    pub async fn connect(config: &AccountConfig) -> Result<Self> {
        let credentials = config.credentials()?;
        let addr = format!("{}:{}", config.imap_host, config.imap_port);
        let connect_timeout = Duration::from_secs(config.imap_command_timeout_secs);
        debug!(host = %config.imap_host, port = config.imap_port, tls = config.tls, "connecting to IMAP server");

        let stream = tokio::time::timeout(connect_timeout, async {
            let tcp = TcpStream::connect(&addr)
                .await
                .map_err(ImapFailure::transport)
                .with_context(|| format!("connecting to {addr}"))?;

            if config.tls {
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
                        let ca_path = std::path::Path::new(ca_file)
                            .canonicalize()
                            .with_context(|| format!("resolving TLS CA file path '{ca_file}'"))?;
                        if !ca_path.is_file() {
                            bail!("TLS CA path '{}' is not a regular file", ca_path.display());
                        }
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let mode = std::fs::metadata(&ca_path)
                                .with_context(|| {
                                    format!(
                                        "reading metadata for TLS CA file '{}'",
                                        ca_path.display()
                                    )
                                })?
                                .permissions()
                                .mode();
                            if mode & 0o002 != 0 {
                                bail!(
                                    "TLS CA file '{}' is world-writable (mode {:o}); \
                                     fix permissions before use",
                                    ca_path.display(),
                                    mode & 0o777,
                                );
                            }
                        }
                        let pem = std::fs::read(&ca_path)
                            .with_context(|| {
                                format!("reading TLS CA file '{}'", ca_path.display())
                            })?;
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
                    .map_err(ImapFailure::transport).context("TLS handshake failed")?;
                Ok::<_, anyhow::Error>(Stream::tls(tls_stream.into()))
            } else {
                Ok(Stream::insecure(tcp))
            }
        })
        .await
        .map_err(|_| ImapFailure::Timeout("connection"))??;

        let mut options = Options::default();
        options.max_response_size = MAX_WIRE_RESPONSE_BYTES;
        let client = Client::new(options);
        let command_timeout = Duration::from_secs(config.imap_command_timeout_secs);
        let mut conn = Self {
            stream,
            client,
            tag_counter: 0,
            command_timeout,
            idle_drain_timeout: Duration::from_secs(config.idle_drain_timeout_secs),
            idle_inactivity_timeout: Duration::from_secs(config.idle_inactivity_timeout_secs),
            fetch_drain_timeout: Duration::from_millis(config.fetch_drain_timeout_millis),
        };

        // Wait for server greeting
        conn.wait_for_greeting().await?;

        // Login (clone strings to satisfy 'static lifetime requirements)
        conn.login(&credentials.username, &credentials.password)
            .await?;

        Ok(conn)
    }

    fn next_tag(&mut self) -> Result<Tag<'static>> {
        self.tag_counter += 1;
        Tag::try_from(format!("M{}", self.tag_counter))
            .map_err(|e| anyhow::anyhow!("invalid IMAP tag 'M{}': {e}", self.tag_counter))
    }

    async fn wait_for_greeting(&mut self) -> Result<()> {
        let timeout = self.command_timeout;
        tokio::time::timeout(timeout, async {
            loop {
                let event = self
                    .stream
                    .next(&mut self.client)
                    .await
                    .map_err(ImapFailure::from_stream)
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
        .map_err(|_| ImapFailure::Timeout("greeting"))?
    }

    async fn login(&mut self, username: &str, password: &str) -> Result<()> {
        let tag = self.next_tag()?;
        // Clone into owned strings so the Command is 'static
        let cmd = Command {
            tag: tag.clone(),
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
                    .map_err(ImapFailure::from_stream)
                    .context("during LOGIN")?;
                match event {
                    Event::StatusReceived { status } => {
                        check_status(&status, "LOGIN")?;
                        if is_command_completion(&status, &tag) {
                            return Ok(());
                        }
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
        .map_err(|_| ImapFailure::Timeout("LOGIN"))?
    }

    /// SELECT a mailbox. Returns (uid_validity, exists_count).
    pub async fn select(&mut self, mailbox: &str) -> Result<(u32, u32)> {
        let tag = self.next_tag()?;
        let cmd = Command {
            tag: tag.clone(),
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
                    .map_err(ImapFailure::from_stream)
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
                        if is_command_completion(&status, &tag) {
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
        .map_err(|_| ImapFailure::Timeout("SELECT"))??;

        let uid_validity =
            uid_validity.context("SELECT omitted required UIDVALIDITY; refusing to sync")?;
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

        let tag = self.next_tag()?;
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
            tag: tag.clone(),
            body: CommandBody::Fetch {
                sequence_set,
                macro_or_item_names: fetch_attrs,
                uid: true,
            },
        };
        let _handle = self.client.enqueue_command(cmd);

        let mut messages = Vec::new();
        let mut budget = ResponseBudget::default();
        let timeout = self.command_timeout;

        loop {
            let event =
                match tokio::time::timeout(timeout, self.stream.next(&mut self.client)).await {
                    Ok(result) => result
                        .map_err(ImapFailure::from_stream)
                        .context("during UID FETCH")?,
                    Err(_) => return Err(ImapFailure::Timeout("UID FETCH").into()),
                };
            match event {
                Event::DataReceived { data } => {
                    if let Data::Fetch { seq: _, items } = data {
                        let msg = parse_fetch_response(items.as_ref(), &mut budget)?;
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
                    if is_command_completion(&status, &tag) {
                        // Some servers (e.g. Proton Mail Bridge) send the
                        // tagged OK before the FETCH data. If nothing has
                        // arrived yet, drain briefly for trailing responses.
                        if messages.is_empty() {
                            while let Ok(Ok(Event::DataReceived {
                                data: Data::Fetch { items, .. },
                            })) = tokio::time::timeout(
                                self.fetch_drain_timeout,
                                self.stream.next(&mut self.client),
                            )
                            .await
                            {
                                if let Some(msg) =
                                    parse_fetch_response(items.as_ref(), &mut budget)?
                                {
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

        messages.retain(|message| {
            message.uid >= uid_start && uid_end.is_none_or(|end| message.uid <= end)
        });
        messages.sort_unstable_by_key(|message| message.uid);
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
        let tag = self.next_tag()?;
        let sequence_set: SequenceSet = range
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid UID range: {range}"))?;

        let cmd = Command {
            tag: tag.clone(),
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
        let mut budget = ResponseBudget::default();
        let timeout = self.command_timeout;

        loop {
            let event =
                match tokio::time::timeout(timeout, self.stream.next(&mut self.client)).await {
                    Ok(result) => result
                        .map_err(ImapFailure::from_stream)
                        .context("during UID FETCH (uid list)")?,
                    Err(_) => return Err(ImapFailure::Timeout("UID FETCH uid list").into()),
                };
            match event {
                Event::DataReceived { data } => {
                    if let Data::Fetch { items, .. } = data {
                        for item in items.as_ref() {
                            if let MessageDataItem::Uid(u) = item {
                                budget.reserve(0)?;
                                uids.push(u.get());
                            }
                        }
                    }
                }
                Event::StatusReceived { status } => {
                    check_status(&status, "UID FETCH (uid list)")?;
                    if is_command_completion(&status, &tag) {
                        if uids.is_empty() {
                            while let Ok(Ok(Event::DataReceived {
                                data: Data::Fetch { items, .. },
                            })) = tokio::time::timeout(
                                self.fetch_drain_timeout,
                                self.stream.next(&mut self.client),
                            )
                            .await
                            {
                                for item in items.as_ref() {
                                    if let MessageDataItem::Uid(u) = item {
                                        budget.reserve(0)?;
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
        uids.sort_unstable();
        uids.dedup();

        Ok(uids)
    }

    /// UID SEARCH SINCE <date> — returns UIDs of messages on or after the given date.
    pub async fn uid_search_since(&mut self, since: chrono::NaiveDate) -> Result<Vec<u32>> {
        let tag = self.next_tag()?;
        let imap_date = imap_next::imap_types::datetime::NaiveDate::try_from(since)
            .context("converting date for UID SEARCH SINCE")?;
        let criteria = Vec1::from(SearchKey::Since(imap_date));
        let cmd = Command {
            tag: tag.clone(),
            body: CommandBody::search(None, criteria, true),
        };
        let _handle = self.client.enqueue_command(cmd);

        let mut uids = Vec::new();
        let mut budget = ResponseBudget::default();
        let timeout = self.command_timeout;
        tokio::time::timeout(timeout, async {
            loop {
                let event = self
                    .stream
                    .next(&mut self.client)
                    .await
                    .map_err(ImapFailure::from_stream)
                    .context("during UID SEARCH SINCE")?;
                match event {
                    Event::DataReceived { data } => {
                        if let Data::Search(results, ..) = data {
                            for uid in results {
                                budget.reserve(0)?;
                                uids.push(uid.get());
                            }
                        }
                    }
                    Event::StatusReceived { status } => {
                        check_status(&status, "UID SEARCH SINCE")?;
                        if is_command_completion(&status, &tag) {
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
        .map_err(|_| ImapFailure::Timeout("UID SEARCH SINCE"))??;

        uids.sort_unstable();
        uids.dedup();
        Ok(uids)
    }

    /// Enter IMAP IDLE mode. Returns when the server sends an update
    /// (EXISTS, EXPUNGE, etc.), the heartbeat timer fires, or the token
    /// is cancelled.  If `heartbeat_interval` is `Some`, the heartbeat
    /// branch fires after that duration with no server notification.
    pub async fn idle(
        &mut self,
        token: &CancellationToken,
        heartbeat_interval: Option<Duration>,
    ) -> Result<IdleOutcome> {
        let tag = self.next_tag()?;
        let cmd = Command {
            tag: tag.clone(),
            body: CommandBody::Idle,
        };
        let _handle = self.client.enqueue_command(cmd);

        // Wait for IDLE to be accepted, collecting any mailbox updates that
        // arrive before the server sends the continuation response.  This
        // closes the race where a new message is delivered during the ~500 ms
        // window between the previous sync completing and IDLE being accepted:
        // without this, the server's EXISTS notification would be silently
        // discarded and the message would go undetected until the next
        // unrelated IDLE wake-up.
        let timeout = self.command_timeout;
        let got_update_during_setup: bool = tokio::time::timeout(timeout, async {
            let mut saw_update = false;
            loop {
                let event = self
                    .stream
                    .next(&mut self.client)
                    .await
                    .map_err(ImapFailure::from_stream)
                    .context("during IDLE setup")?;
                match event {
                    Event::IdleCommandSent { .. } => {
                        debug!("IDLE command sent");
                    }
                    Event::IdleAccepted { .. } => {
                        debug!("IDLE accepted, waiting for updates");
                        return Ok::<bool, anyhow::Error>(saw_update);
                    }
                    Event::IdleRejected { .. } => {
                        return Err(ImapFailure::IdleUnsupported.into());
                    }
                    Event::DataReceived { data } => match data {
                        Data::Exists(_) | Data::Expunge(_) | Data::Recent(_) => {
                            debug!(data = ?data, "IDLE received mailbox update during setup");
                            saw_update = true;
                        }
                        _ => {
                            trace!(data = ?data, "ignoring non-update data during IDLE setup");
                        }
                    },
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
        .map_err(|_| ImapFailure::Timeout("IDLE setup"))??;

        let effective_heartbeat = heartbeat_interval
            .unwrap_or(self.idle_inactivity_timeout)
            .min(self.idle_inactivity_timeout);

        let outcome = if got_update_during_setup {
            IdleOutcome::Update
        } else {
            let heartbeat = tokio::time::sleep(effective_heartbeat);
            tokio::pin!(heartbeat);
            loop {
                tokio::select! {
                    event = self.stream.next(&mut self.client) => {
                        match event.map_err(ImapFailure::from_stream).context("during IDLE")? {
                            Event::DataReceived { data: Data::Exists(_) | Data::Expunge(_) | Data::Recent(_) } => {
                                break IdleOutcome::Update;
                            }
                            Event::StatusReceived { status } => {
                                check_status(&status, "IDLE")?;
                                if is_command_completion(&status, &tag) {
                                    // The server ended IDLE itself; no DONE remains to drain.
                                    return Ok(IdleOutcome::Update);
                                }
                            }
                            _ => {}
                        }
                    }
                    _ = &mut heartbeat => break IdleOutcome::Heartbeat,
                    _ = token.cancelled() => break IdleOutcome::Shutdown,
                }
            }
        };

        // All exits use the same bounded drain. A timeout/error must force a
        // reconnect: reusing this stream would let a stale DONE response end
        // the next command before its actual results arrive.
        self.client.set_idle_done();
        self.drain_idle_done(&tag).await?;
        Ok(outcome)
    }

    async fn drain_idle_done(&mut self, tag: &Tag<'_>) -> Result<()> {
        tokio::time::timeout(self.idle_drain_timeout, async {
            loop {
                if let Event::StatusReceived { status } = self
                    .stream
                    .next(&mut self.client)
                    .await
                    .map_err(ImapFailure::from_stream)
                    .context("draining IDLE DONE")?
                {
                    check_status(&status, "IDLE DONE")?;
                    if is_command_completion(&status, tag) {
                        return Ok(());
                    }
                }
            }
        })
        .await
        .map_err(|_| ImapFailure::Timeout("IDLE DONE"))?
    }

    /// Send LOGOUT.
    pub async fn logout(&mut self) -> Result<()> {
        let tag = self.next_tag()?;
        let cmd = Command {
            tag: tag.clone(),
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

fn parse_fetch_response(
    items: &[MessageDataItem],
    budget: &mut ResponseBudget,
) -> Result<Option<FetchedMessage>> {
    // Count every response, including malformed/unsolicited responses. Inspect
    // borrowed bodies before cloning so rejected data never grows our result.
    let bytes = items.iter().try_fold(0usize, |total, item| {
        let size = match item {
            MessageDataItem::BodyExt { data, .. } | MessageDataItem::Rfc822(data) => {
                data.0.as_ref().map_or(0, |value| value.as_ref().len())
            }
            MessageDataItem::Flags(flags) => flags
                .iter()
                .map(|flag| flag_fetch_to_string(flag).len())
                .sum(),
            _ => 0,
        };
        total
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("IMAP response size overflow"))
    })?;
    budget.reserve(bytes)?;
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

    let (Some(uid), Some(raw_bytes)) = (uid, raw_bytes) else {
        return Ok(None);
    };

    Ok(Some(FetchedMessage {
        uid,
        flags,
        raw_bytes,
        size,
    }))
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

fn is_command_completion(status: &Status<'_>, tag: &Tag<'_>) -> bool {
    matches!(status, Status::Tagged(response) if response.tag == *tag)
}

fn check_status(status: &Status, operation: &'static str) -> Result<()> {
    match status {
        Status::Tagged(response) => match response.body.kind {
            StatusKind::Ok => Ok(()),
            _ if operation == "LOGIN" => Err(ImapFailure::Authentication.into()),
            _ => Err(ImapFailure::Protocol(anyhow::anyhow!("{operation} rejected")).into()),
        },
        Status::Untagged(body) => match body.kind {
            StatusKind::Ok => Ok(()),
            _ if operation == "LOGIN" => Err(ImapFailure::Authentication.into()),
            _ => Err(ImapFailure::Protocol(anyhow::anyhow!("{operation} rejected")).into()),
        },
        Status::Bye(_) => {
            Err(ImapFailure::Transport(anyhow::anyhow!("server closed {operation}")).into())
        }
    }
}

#[cfg(test)]
mod uid_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn pair() -> (ImapConnection, BufReader<TcpStream>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        let mut options = Options::default();
        options.discard_greeting = true;
        (
            ImapConnection {
                stream: Stream::insecure(client),
                client: Client::new(options),
                tag_counter: 0,
                command_timeout: Duration::from_secs(1),
                idle_drain_timeout: Duration::from_secs(1),
                idle_inactivity_timeout: Duration::from_secs(1800),
                fetch_drain_timeout: Duration::from_millis(200),
            },
            BufReader::new(peer),
        )
    }
    async fn command(peer: &mut BufReader<TcpStream>, expected: &str, data: &str) {
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        assert!(line.contains(expected));
        let tag = line.split_whitespace().next().unwrap();
        peer.write_all(format!("{data}{tag} OK done\r\n").as_bytes())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn select_without_uidvalidity_is_rejected() {
        let (mut connection, mut peer) = pair().await;
        let server = tokio::spawn(async move {
            command(&mut peer, " SELECT ", "* 5 EXISTS\r\n").await;
        });
        assert!(
            connection
                .select("INBOX")
                .await
                .unwrap_err()
                .to_string()
                .contains("UIDVALIDITY")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fetched_uids_are_sorted_and_deduplicated_and_unsolicited_bodies_are_excluded() {
        let (mut connection, mut peer) = pair().await;
        let server = tokio::spawn(async move {
            command(&mut peer, " UID FETCH ", "* 1 FETCH (UID 3)\r\n* 2 FETCH (UID 1)\r\n* 3 FETCH (UID 2)\r\n* 4 FETCH (UID 3)\r\n").await;
            let mut bodies = String::new();
            for uid in [3, 1, 99, 2] {
                let body = format!("Subject: {uid}\r\n\r\nBody");
                bodies.push_str(&format!(
                    "* {uid} FETCH (UID {uid} BODY[] {{{}}}\r\n{body})\r\n",
                    body.len()
                ));
            }
            command(&mut peer, " UID FETCH ", &bodies).await;
            command(&mut peer, " UID SEARCH ", "* SEARCH 5 3 5\r\n").await;
        });
        assert_eq!(
            connection.uid_fetch_uid_list(1).await.unwrap(),
            vec![1, 2, 3]
        );
        let messages = connection.uid_fetch_range(1, Some(3)).await.unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.uid)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            connection
                .uid_search_since(chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap())
                .await
                .unwrap(),
            vec![3, 5]
        );
        server.await.unwrap();
    }
}

#[cfg(test)]
mod response_limit_tests {
    use super::*;
    use imap_next::imap_types::core::NString;
    use std::num::NonZeroU32;

    #[test]
    fn repeated_small_responses_hit_element_limit() {
        let mut budget = ResponseBudget::default();
        for _ in 0..MAX_RESPONSE_ITEMS {
            budget.reserve(0).unwrap();
        }
        assert!(budget.reserve(0).is_err());
        assert_eq!(budget.items, MAX_RESPONSE_ITEMS);
    }

    #[test]
    fn aggregate_bytes_are_checked_before_retaining_message() {
        let items = [
            MessageDataItem::Uid(NonZeroU32::new(1).unwrap()),
            MessageDataItem::Rfc822(NString::try_from("body").unwrap()),
        ];
        let mut budget = ResponseBudget {
            items: 0,
            bytes: MAX_FETCH_BYTES - 4,
        };
        let message = parse_fetch_response(&items, &mut budget).unwrap().unwrap();
        assert_eq!(message.raw_bytes, b"body");
        assert!(parse_fetch_response(&items, &mut budget).is_err());
        assert_eq!(budget.bytes, MAX_FETCH_BYTES);
        assert_eq!(budget.items, 1);
    }

    #[test]
    fn malformed_fetch_responses_also_consume_budget() {
        let mut budget = ResponseBudget {
            items: MAX_RESPONSE_ITEMS - 1,
            bytes: 0,
        };
        assert!(parse_fetch_response(&[], &mut budget).unwrap().is_none());
        assert!(parse_fetch_response(&[], &mut budget).is_err());
    }
}

#[cfg(test)]
mod command_completion_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    async fn pair() -> (ImapConnection, BufReader<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        let mut options = Options::default();
        options.discard_greeting = true;
        (
            ImapConnection {
                stream: Stream::insecure(tcp),
                client: Client::new(options),
                tag_counter: 0,
                command_timeout: Duration::from_secs(1),
                idle_drain_timeout: Duration::from_secs(1),
                idle_inactivity_timeout: Duration::from_secs(1800),
                fetch_drain_timeout: Duration::from_millis(200),
            },
            BufReader::new(peer),
        )
    }

    async fn read_line(peer: &mut BufReader<TcpStream>) -> String {
        let mut line = String::new();
        peer.read_line(&mut line).await.unwrap();
        line
    }

    #[tokio::test]
    async fn login_drains_keepalive_before_select() {
        let (mut conn, mut peer) = pair().await;
        let server = tokio::spawn(async move {
            assert!(read_line(&mut peer).await.starts_with("M1 LOGIN"));
            peer.write_all(b"* OK keepalive\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            peer.write_all(b"M1 OK logged in\r\n").await.unwrap();
            assert!(read_line(&mut peer).await.starts_with("M2 SELECT"));
            peer.write_all(b"* OK [UIDVALIDITY 123] valid\r\n* 2 EXISTS\r\nM2 OK selected\r\n")
                .await
                .unwrap();
        });
        conn.login("user", "secret").await.unwrap();
        assert_eq!(conn.select("INBOX").await.unwrap(), (123, 2));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn untagged_ok_cannot_hide_login_rejection() {
        let (mut conn, mut peer) = pair().await;
        let server = tokio::spawn(async move {
            read_line(&mut peer).await;
            peer.write_all(b"* OK keepalive\r\nM1 NO authentication failed\r\n")
                .await
                .unwrap();
        });
        assert!(conn.login("user", "secret").await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn every_idle_exit_drains_keepalives_before_next_command() {
        for mode in ["setup_update", "update", "heartbeat", "shutdown"] {
            let (mut conn, mut peer) = pair().await;
            let server = tokio::spawn(async move {
                assert_eq!(read_line(&mut peer).await, "M1 IDLE\r\n");
                if mode == "setup_update" {
                    peer.write_all(b"* 1 EXISTS\r\n").await.unwrap();
                }
                peer.write_all(b"+ idling\r\n").await.unwrap();
                if mode == "update" {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    peer.write_all(b"* 1 EXISTS\r\n").await.unwrap();
                }
                assert_eq!(read_line(&mut peer).await, "DONE\r\n");
                peer.write_all(b"* OK keepalive\r\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
                peer.write_all(b"M1 OK idle done\r\n").await.unwrap();
                assert!(read_line(&mut peer).await.starts_with("M2 SELECT"));
                peer.write_all(b"* OK [UIDVALIDITY 123] valid\r\n* 2 EXISTS\r\nM2 OK selected\r\n")
                    .await
                    .unwrap();
            });
            let token = CancellationToken::new();
            if mode == "shutdown" {
                token.cancel();
            }
            let outcome = conn
                .idle(&token, Some(Duration::from_millis(50)))
                .await
                .unwrap();
            let expected = match mode {
                "heartbeat" => IdleOutcome::Heartbeat,
                "shutdown" => IdleOutcome::Shutdown,
                _ => IdleOutcome::Update,
            };
            assert_eq!(outcome, expected, "{mode}");
            assert_eq!(conn.select("INBOX").await.unwrap(), (123, 2), "{mode}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn failed_idle_drain_is_an_error() {
        let (mut conn, mut peer) = pair().await;
        let server = tokio::spawn(async move {
            read_line(&mut peer).await;
            peer.write_all(b"+ idling\r\n").await.unwrap();
            assert_eq!(read_line(&mut peer).await, "DONE\r\n");
            peer.write_all(b"* OK keepalive\r\n").await.unwrap();
            // Closing without the tagged completion must not produce success.
        });
        let token = CancellationToken::new();
        token.cancel();
        assert!(conn.idle(&token, None).await.is_err());
        server.await.unwrap();
    }
}

#[cfg(test)]
mod typed_error_tests {
    use super::*;
    use crate::imap::error::{FailureKind, kind};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn pair() -> (ImapConnection, BufReader<TcpStream>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut options = Options::default();
        options.discard_greeting = true;
        (
            ImapConnection {
                stream: Stream::insecure(client),
                client: Client::new(options),
                tag_counter: 0,
                command_timeout: Duration::from_millis(50),
                idle_drain_timeout: Duration::from_secs(1),
                idle_inactivity_timeout: Duration::from_secs(1800),
                fetch_drain_timeout: Duration::from_millis(200),
            },
            BufReader::new(server),
        )
    }

    #[tokio::test]
    async fn non_english_login_rejection_is_typed_authentication() {
        let (mut connection, mut peer) = pair().await;
        let server = tokio::spawn(async move {
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            peer.write_all(b"M1 NO keine Berechtigung private-user\r\n")
                .await
                .unwrap();
        });
        let error = connection
            .login("test", "test")
            .await
            .unwrap_err()
            .context("unrelated wrapper");
        assert_eq!(kind(&error), FailureKind::Authentication);
        assert_eq!(crate::metrics::classify_error(&error), "auth");
        assert!(!format!("{error:#}").contains("private-user"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn real_command_deadline_and_closed_connection_have_typed_categories() {
        let (mut connection, mut peer) = pair().await;
        let server = tokio::spawn(async move {
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            std::future::pending::<()>().await;
        });
        let error = connection.login("test", "test").await.unwrap_err();
        assert_eq!(kind(&error), FailureKind::Timeout);
        assert_eq!(crate::metrics::classify_error(&error), "connection_timeout");
        server.abort();
        let (mut connection, peer) = pair().await;
        drop(peer);
        let error = connection.login("test", "test").await.unwrap_err();
        assert_eq!(kind(&error), FailureKind::Network);
        assert_eq!(crate::metrics::classify_error(&error), "network");
    }

    #[tokio::test]
    async fn explicit_idle_rejection_is_distinct_from_a_transport_failure() {
        let (mut connection, mut peer) = pair().await;
        let server = tokio::spawn(async move {
            let mut line = String::new();
            peer.read_line(&mut line).await.unwrap();
            peer.write_all(b"M1 BAD unsupported\r\n").await.unwrap();
        });
        let error = connection
            .idle(&CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert_eq!(kind(&error), FailureKind::IdleUnsupported);
        server.await.unwrap();
    }
}
