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
        let addr = format!("{}:{}", config.imap_host, config.imap_port);
        let connect_timeout = Duration::from_secs(config.imap_command_timeout_secs);
        debug!(host = %config.imap_host, port = config.imap_port, tls = config.tls, "connecting to IMAP server");

        let stream = tokio::time::timeout(connect_timeout, async {
            let tcp = TcpStream::connect(&addr)
                .await
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
                    .map_err(|e| anyhow::anyhow!(format!("TLS handshake failed: {e}")))?;
                Ok::<_, anyhow::Error>(Stream::tls(tls_stream.into()))
            } else {
                Ok(Stream::insecure(tcp))
            }
        })
        .await
        .unwrap_or_else(|_| bail!("IMAP connection timed out after {}s", connect_timeout.as_secs()))?;

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
        let username = config.username.clone();
        let password = config.password.clone();
        conn.login(&username, &password).await?;

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
        let tag = self.next_tag()?;
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
        let tag = self.next_tag()?;
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
            tag,
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
                    if matches!(status, Status::Tagged(_)) {
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
        let mut budget = ResponseBudget::default();
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
                                budget.reserve(0)?;
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
            tag,
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
        .unwrap_or_else(|_| {
            bail!(
                "IMAP UID SEARCH SINCE timed out after {}s",
                timeout.as_secs()
            )
        })?;

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
            tag,
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
                        bail!("IDLE rejected by server");
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
        .unwrap_or_else(|_| bail!("IMAP IDLE setup timed out after {}s", timeout.as_secs()))?;

        // If a mailbox update arrived before IDLE was accepted, exit IDLE
        // immediately so the caller can run an incremental sync.
        if got_update_during_setup {
            self.client.set_idle_done();
            let _ = tokio::time::timeout(self.idle_drain_timeout, async {
                loop {
                    match self.stream.next(&mut self.client).await {
                        Ok(Event::StatusReceived { .. }) => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            })
            .await;
            return Ok(IdleOutcome::Update);
        }

        // Now in IDLE mode — wait for updates, heartbeat, or cancellation.
        // IMPORTANT: when an update arrives we queue DONE but must keep
        // looping until the server's tagged OK is received.  Breaking
        // immediately after set_idle_done() leaves the DONE acknowledgment
        // unread; the next command's response loop then consumes it and
        // returns early with an empty result, causing every post-IDLE sync
        // to report "no new messages" even when new mail is present.
        let effective_heartbeat = match heartbeat_interval {
            Some(d) if d > self.idle_inactivity_timeout => {
                tracing::warn!(
                    requested_secs = d.as_secs(),
                    max_secs = self.idle_inactivity_timeout.as_secs(),
                    "heartbeat interval exceeds maximum IDLE inactivity timeout, clamping"
                );
                self.idle_inactivity_timeout
            }
            Some(d) => d,
            None => self.idle_inactivity_timeout,
        };
        let mut heartbeat: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(tokio::time::sleep(effective_heartbeat));

        let mut saw_update = false;
        let outcome = loop {
            tokio::select! {
                event = self.stream.next(&mut self.client) => {
                    match event.context("during IDLE")? {
                        Event::DataReceived { data } => {
                            match data {
                                Data::Exists(_) | Data::Expunge(_) | Data::Recent(_) => {
                                    debug!(data = ?data, "IDLE received mailbox update");
                                    if !saw_update {
                                        self.client.set_idle_done();
                                        saw_update = true;
                                    }
                                    // Keep looping to drain the DONE acknowledgment.
                                }
                                _ => {
                                    trace!(data = ?data, "IDLE received non-update data");
                                }
                            }
                        }
                        Event::IdleDoneSent { .. } => {
                            debug!("IDLE DONE sent");
                        }
                        Event::StatusReceived { status } => {
                            // Only the tagged OK terminates IDLE; untagged * OK
                            // keepalives can arrive at any time and must not
                            // cause an early exit.
                            if matches!(status, Status::Tagged(_)) {
                                break IdleOutcome::Update;
                            }
                        }
                        other => {
                            trace!(event = ?other, "ignoring event during IDLE");
                        }
                    }
                }
                _ = &mut heartbeat => {
                    debug!("IDLE heartbeat fired, exiting IDLE for periodic sync");
                    if !saw_update {
                        self.client.set_idle_done();
                    }
                    let _ = tokio::time::timeout(self.idle_drain_timeout, async {
                        loop {
                            match self.stream.next(&mut self.client).await {
                                Ok(Event::StatusReceived { .. }) => break,
                                Ok(Event::IdleDoneSent { .. }) => continue,
                                Ok(_) => continue,
                                Err(_) => break,
                            }
                        }
                    })
                    .await;
                    break IdleOutcome::Heartbeat;
                }
                _ = token.cancelled() => {
                    debug!("IDLE cancelled by shutdown");
                    if !saw_update {
                        self.client.set_idle_done();
                    }
                    // Drain until we get the tagged response, with a short timeout
                    // to avoid blocking shutdown indefinitely.
                    let _ = tokio::time::timeout(self.idle_drain_timeout, async {
                        loop {
                            match self.stream.next(&mut self.client).await {
                                Ok(Event::StatusReceived { .. }) => break,
                                Ok(Event::IdleDoneSent { .. }) => continue,
                                Ok(_) => continue,
                                Err(_) => break,
                            }
                        }
                    }).await;
                    break IdleOutcome::Shutdown;
                }
            }
        };

        Ok(outcome)
    }

    /// Send LOGOUT.
    pub async fn logout(&mut self) -> Result<()> {
        let tag = self.next_tag()?;
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
