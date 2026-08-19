# Mail Index/Search Service using Turso, Tantivy, and MCP

## 1. Purpose

Build a standalone local-first email indexing and search service that:

1. accepts archived RFC 5322 email documents over HTTP;
2. parses and normalizes email bodies and attachments;
3. stores the normalized canonical representation in **Turso Database**;
4. maintains a disposable **Tantivy** full-text search index;
5. exposes search/document retrieval over both HTTP and **Model Context Protocol (MCP)**;
6. provides stable human-viewable source URLs suitable for citation by an AI agent.

The immediate producer is a `mailmux` command processor. The immediate MCP client is `nerdbot`. The service must not depend on either project internally.

## 2. Architectural boundary

```text
                         ingestion
mailmux processor  ---------------------->
                         HTTP
                    +----------------+
                    |   mailindex    |
                    |                |
                    | MIME parser    |
                    | normalizer     |
                    | extractors     |
                    |                |
                    | Turso          |  canonical normalized documents
                    | Tantivy        |  disposable search index
                    |                |
                    | HTTP API       |
                    | MCP server     |
                    +-------+--------+
                            |
                            | MCP / HTTP search
                            v
                        nerdbot
```

Ownership rules:

- the upstream mail system owns the original raw `.eml` archive;
- `mailindex` owns normalized searchable documents and attachment metadata/text in Turso;
- Tantivy is derived state and MUST be rebuildable entirely from Turso;
- consumers never need direct access to Turso or Tantivy files.

## 3. Technology choices

### 3.1 Turso

Use the `turso` Rust crate directly as the in-process canonical SQL store. At specification time, Turso provides a native async Rust API and a local database builder (`Builder::new_local`).

Do not use `rusqlite` or `sqlx` for the mailindex database in v1.

Turso is still evolving rapidly and is not yet a 1.0-stable dependency. Keep all Turso-specific calls behind a small repository/storage module so replacing or adapting the database layer is straightforward.

Turso currently has experimental full-text search backed by Tantivy. Do **not** use that feature in v1. This project deliberately keeps the canonical SQL store and search index as separate layers. A future benchmark may compare Turso FTS with the standalone Tantivy index.

### 3.2 Tantivy

Use Tantivy as an embedded library, not as an external search service.

Tantivy index files live in a configured directory and may be deleted/rebuilt from Turso at any time.

Do not store full email bodies or full attachment text as Tantivy stored fields. Store only the stable document key required to map a search hit back to Turso; index the large text fields without storing duplicate copies.

### 3.3 MCP

Use the official Rust MCP SDK (`rmcp`) unless implementation constraints discovered at coding time make that impossible. At specification time the official Rust SDK supports both MCP servers and clients and Streamable HTTP transport.

The primary deployed MCP transport for `mailindex` is **Streamable HTTP**, hosted by the same process as the ingestion/search HTTP API.

A `--mcp-stdio` development/test mode is optional, not required for v1.

### 3.4 HTTP

Use Axum on Tokio.

## 4. Goals

The implementation MUST:

- be a single long-running Rust binary;
- ingest one email idempotently using a stable `(source, source_id)` key;
- parse MIME safely and normalize a canonical plain-text body;
- retain useful message metadata;
- retain attachment metadata;
- extract searchable text from supported attachment types;
- persist canonical data before it is considered accepted;
- asynchronously/batch-update Tantivy so large historical backfills do not commit one Tantivy segment per HTTP request;
- recover after restart when Turso and Tantivy are temporarily out of sync;
- search by free-text query plus structured filters;
- retrieve a complete normalized email by stable key;
- expose `mail_search` and `mail_get` MCP tools;
- expose equivalent HTTP debugging/automation APIs;
- expose a stable, human-readable `/view/...` URL for each document;
- have bounded body/attachment/request sizes;
- never execute email HTML or attachments.

## 5. Non-goals

V1 does NOT include:

- semantic/vector embeddings;
- OCR of scanned images/PDF pages;
- LLM-based parsing or classification;
- mailbox synchronization or IMAP;
- sending/deleting/mutating upstream mail;
- email flag synchronization;
- a rich web mail UI;
- distributed Tantivy indexes;
- multi-node replication;
- Turso Cloud synchronization;
- Turso experimental FTS;
- thread reconstruction as an MCP tool;
- indexing arbitrary user files unrelated to email.

## 6. Process modes and configuration

Default mode runs the HTTP + MCP server plus background index worker.

Example:

```bash
mailindex --config /etc/mailindex/config.toml
```

Suggested configuration:

```toml
[server]
bind = "127.0.0.1:8090"
public_base_url = "https://mailindex.example.internal"
max_request_bytes = 52428800 # 50 MiB
api_token_env = "MAILINDEX_API_TOKEN"

[storage]
database_path = "/var/lib/mailindex/mailindex.db"

[index]
path = "/var/lib/mailindex/tantivy"
writer_memory_bytes = 134217728
batch_size = 100
commit_interval_ms = 1000

[content]
max_body_chars = 500000
max_attachment_bytes = 26214400
max_attachment_text_chars = 500000
pdf_enabled = true

[search]
default_limit = 10
max_limit = 50
max_get_chars = 100000
```

Secrets MUST be obtained from environment variables, never embedded in config.

If `api_token_env` is set, require `Authorization: Bearer <token>` for ingestion, JSON search/document APIs, and MCP. The human `/view/...` endpoint may either share the same auth requirement or be separately configurable; default to protected.

If the service binds to a non-loopback address and no authentication is configured, log a prominent warning. It is acceptable to make this a startup error if implementation remains simple.

## 7. Stable identity model

Every ingested document has:

```text
source      arbitrary producer namespace, e.g. "mailmux"
source_id   producer-stable identifier, e.g. mailmux emails.id
```

The unique key is `(source, source_id)`.

Construct a stable Tantivy key such as:

```text
mailmux:82913
```

Do not use the email RFC `Message-ID` as the primary key: duplicate copies can exist in different accounts/mailboxes, and `Message-ID` may be absent or malformed.

## 8. Ingestion HTTP API

### 8.1 Endpoint

```http
PUT /v1/documents/{source}/{source_id}
Content-Type: multipart/form-data
```

Multipart fields:

1. `metadata` — required UTF-8 JSON object.
2. `message` — required raw RFC 5322 message bytes, MIME type `message/rfc822` where possible.

Example metadata produced from mailmux:

```json
{
  "account_id": "personal",
  "mailbox_name": "Archive",
  "uid": 48321,
  "mailmux_email_id": 82913
}
```

Do not require producer-supplied subject/sender/body; parse authoritative message metadata from the RFC 5322 message itself. Producer metadata is provenance, not a substitute for MIME parsing.

### 8.2 Response

New/changed document:

```http
202 Accepted
```

```json
{
  "source": "mailmux",
  "source_id": "82913",
  "document_id": 123,
  "changed": true,
  "index_state": "pending",
  "view_url": "https://.../view/mailmux/82913"
}
```

Identical already-indexed document:

```http
200 OK
```

```json
{
  "source": "mailmux",
  "source_id": "82913",
  "document_id": 123,
  "changed": false,
  "index_state": "indexed",
  "view_url": "..."
}
```

### 8.3 Idempotency

Calculate SHA-256 of the raw uploaded message.

If `(source, source_id)` exists with the same raw hash and canonical row is already indexed, treat the request as a no-op.

If bytes differ, re-parse and replace normalized document + attachment rows, then mark the document pending for reindex.

## 9. Mailmux submission adapter

Include a small reference adapter under something like:

```text
contrib/mailmux-submit.sh
```

It consumes the existing mailmux command-processor stdin JSON:

```json
{
  "event": { ... },
  "email": {
    "id": 82913,
    "account_id": "personal",
    "mailbox_name": "Archive",
    "uid": 48321,
    "raw_message_path": "/var/lib/mailmux/.../48321.eml"
  }
}
```

The script:

1. validates `.email` exists;
2. reads `email.raw_message_path`;
3. creates metadata JSON from the mailmux record;
4. sends a multipart `PUT` to `/v1/documents/mailmux/{email.id}`;
5. uses an API bearer token from an environment variable;
6. emits a valid mailmux `ProcessorOutput` JSON object on stdout;
7. exits non-zero on transport or non-2xx failures.

Using `jq` + `curl` is acceptable for this reference adapter. The index service itself must not depend on shell tools.

The raw `.eml` bytes are uploaded, rather than merely sending a filesystem path. This avoids coupling `mailindex` deployment to mailmux filesystem mounts.

## 10. MIME parsing and normalization

Use a mature Rust mail parser such as `mail-parser`.

For every message extract where available:

- RFC Message-ID;
- In-Reply-To;
- References;
- Date;
- From;
- To;
- Cc;
- Bcc if present in stored source;
- Reply-To;
- Subject;
- text body;
- HTML body;
- MIME part/attachment metadata.

### 10.1 Canonical body text

Preferred body selection:

1. use a meaningful `text/plain` alternative when available;
2. otherwise convert the primary `text/html` body to plain text;
3. if both exist, do not blindly concatenate both and duplicate the message;
4. normalize line endings and obvious excessive whitespace;
5. preserve ordinary quoted replies in v1; quote-stripping is deliberately deferred because over-aggressive stripping can destroy useful historical evidence.

Do not store or render active email HTML as trusted content.

### 10.2 Limits

Reject/skip according to configured limits rather than allowing unbounded allocation.

- request maximum;
- per-attachment maximum;
- canonical body character maximum;
- extracted attachment text maximum.

Record truncation flags so consumers know when content is incomplete.

## 11. Attachment extraction

Create an extractor abstraction, for example:

```rust
trait AttachmentTextExtractor {
    fn supports(&self, media_type: &str, filename: Option<&str>) -> bool;
    fn extract(&self, bytes: &[u8]) -> Result<ExtractedText>;
}
```

Required v1 support:

- `text/plain` and other safe `text/*` formats: decode to UTF-8 best effort;
- `text/html`: convert to plain text;
- `text/calendar`: retain as text;
- `application/pdf`: extract text using a pure Rust library such as `pdf-extract` where practical.

Unsupported binary attachments are still recorded with metadata and `extraction_status = "unsupported"`.

Extraction failure MUST NOT reject the entire email. Persist the email and attachment metadata, record `extraction_status = "error"` and a concise error string, and continue.

Do not OCR images in v1.

## 12. Turso schema

Use explicit migration/version management owned by the application. Do not assume an external SQLite migration tool works perfectly with Turso.

Suggested schema (exact SQL may be adapted to Turso compatibility):

```sql
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);

CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,

    account_id TEXT,
    mailbox_name TEXT,
    imap_uid INTEGER,

    message_id TEXT,
    in_reply_to TEXT,
    references_json TEXT,

    sent_at TEXT,
    subject TEXT,
    sender TEXT,
    to_json TEXT,
    cc_json TEXT,
    bcc_json TEXT,
    reply_to_json TEXT,

    body_text TEXT NOT NULL DEFAULT '',
    body_truncated INTEGER NOT NULL DEFAULT 0,

    raw_sha256 TEXT NOT NULL,

    index_state TEXT NOT NULL DEFAULT 'pending',
    index_error TEXT,
    index_attempts INTEGER NOT NULL DEFAULT 0,
    indexed_at TEXT,

    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,

    UNIQUE(source, source_id)
);

CREATE INDEX idx_documents_sent_at ON documents(sent_at);
CREATE INDEX idx_documents_account ON documents(account_id);
CREATE INDEX idx_documents_mailbox ON documents(mailbox_name);
CREATE INDEX idx_documents_message_id ON documents(message_id);

CREATE TABLE attachments (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL,
    part_index INTEGER NOT NULL,
    filename TEXT,
    media_type TEXT,
    content_disposition TEXT,
    content_id TEXT,
    size_bytes INTEGER,
    sha256 TEXT,
    extraction_status TEXT NOT NULL,
    extraction_error TEXT,
    extracted_text TEXT,
    text_truncated INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE,
    UNIQUE(document_id, part_index)
);

CREATE INDEX idx_attachments_document ON attachments(document_id);
```

Enable/enforce foreign keys if required by the Turso API/runtime.

JSON fields are stored as JSON text in v1; do not require Turso-specific JSON extensions unless needed.

## 13. Canonical-write / Tantivy consistency model

Turso is the source of truth. Tantivy is eventually consistent derived state.

### 13.1 Ingestion sequence

For a changed/new email:

1. parse/normalize/extract in memory;
2. begin Turso transaction;
3. upsert document;
4. replace its attachment rows;
5. set `index_state = 'pending'`, clear prior index error;
6. commit Turso transaction;
7. notify the background index worker;
8. return HTTP 202.

Never report the document as durable before the Turso transaction commits.

### 13.2 Background index worker

A single logical writer task owns the Tantivy `IndexWriter`.

It repeatedly selects pending documents from Turso in bounded batches (for example 100), converts them to Tantivy documents, performs delete+add upserts, and commits the batch once.

Wake conditions:

- ingestion notification;
- periodic timer (for example 1 second);
- startup reconciliation.

This avoids a Tantivy commit for every incoming HTTP request during a historical backfill.

### 13.3 Crash recovery

After a successful Tantivy commit, update the corresponding Turso rows to:

```text
index_state = indexed
indexed_at = now
index_error = NULL
```

If the process crashes:

- after Turso commit but before Tantivy commit: row remains pending and will be indexed after restart;
- after Tantivy commit but before Turso status update: row remains pending and may be indexed again after restart; delete+add by stable key makes that harmless.

### 13.4 Indexing error

On an unrecoverable batch/document indexing error, record:

```text
index_state = error
index_error = concise message
index_attempts += 1
```

Provide a repair/requeue operation:

```http
POST /v1/documents/{source}/{source_id}/reindex
```

and a whole-index rebuild command described below.

## 14. Tantivy schema

The Tantivy index should optimize search while minimizing duplicate storage.

Recommended logical fields:

```text
document_key       exact string, INDEXED + STORED
source             exact string, filterable
account_id         exact string, filterable
mailbox_name       exact string, filterable
sent_timestamp     i64/date, indexed/fast for ranges
sender_exact       exact normalized address, filterable
sender_text        tokenized text
recipients_text    tokenized text
subject            tokenized text with positions
body               tokenized text with positions
attachment_text    tokenized text with positions
```

Only `document_key` needs to be a large-scale stored field. Search result metadata/content is loaded from Turso after Tantivy returns document keys.

Do not mark `body` or `attachment_text` as `STORED`.

Use the standard English tokenizer/stemmer initially if appropriate, while preserving an easy path to a less language-specific tokenizer. Email archives may be multilingual; avoid deeply baking English-only behavior into schema APIs.

Use BM25/default Tantivy scoring.

Weight subject higher than body, and body higher than attachment text where the Tantivy/query API permits straightforward boosts. Suggested starting weights:

```text
subject          3.0
sender/recipient 2.0
body             1.0
attachment       0.8
```

These are defaults, not sacred constants; keep them isolated/configurable enough to tune after real evaluation.

## 15. Tantivy rebuild command

Provide:

```bash
mailindex rebuild-index
```

Behavior:

1. open Turso;
2. create a fresh Tantivy index in a temporary sibling directory;
3. stream all canonical Turso documents in bounded batches;
4. index and commit;
5. atomically replace/swap the active index directory where the platform allows;
6. update index state appropriately;
7. preserve the old index until the new one is successfully built, or use a clearly documented safe replacement strategy.

At minimum, a failed rebuild MUST NOT destroy the only working index.

Also support:

```bash
mailindex index-status
```

showing total documents and counts by `index_state` if inexpensive to implement.

## 16. Search service API

Create one internal `SearchService` used by both HTTP handlers and MCP tools. Do not maintain separate search implementations.

### 16.1 Search request model

```json
{
  "query": "flight itinerary Bangalore",
  "after": "2021-01-01",
  "before": "2022-01-01",
  "account_ids": ["personal"],
  "mailboxes": ["Archive"],
  "senders": [],
  "limit": 10
}
```

Rules:

- `query` is required for v1 and must not be blank;
- `after` is inclusive;
- `before` is exclusive;
- date-only and RFC3339 forms use the same semantics as the mailmux backfill spec;
- `limit` defaults to configured default and is clamped/rejected above configured maximum;
- multiple accounts/mailboxes/senders use OR within the same filter category and AND across categories.

Build date/account/mailbox filters programmatically in Tantivy; do not interpolate them into user query syntax.

Use `QueryParser` or an equivalent safe query builder for the free-text portion. Natural keyword queries must work without callers understanding Tantivy field syntax.

### 16.2 HTTP endpoint

```http
POST /v1/search
Content-Type: application/json
```

Response:

```json
{
  "results": [
    {
      "source": "mailmux",
      "source_id": "82913",
      "score": 7.42,
      "sent_at": "2021-10-03T09:42:00Z",
      "sender": "eticket@emirates.com",
      "subject": "Your Emirates e-ticket receipt",
      "snippet": "...JFK ... DXB ... BLR...",
      "attachments": [
        {"filename": "eticket.pdf", "media_type": "application/pdf"}
      ],
      "view_url": "https://.../view/mailmux/82913"
    }
  ]
}
```

Snippets are best effort and can be generated from the Turso body/attachment text after the top document keys are known. Do not duplicate huge text fields in Tantivy solely to generate snippets.

## 17. Document retrieval HTTP API

```http
GET /v1/documents/{source}/{source_id}
```

Return normalized JSON including:

- metadata;
- body text;
- body truncation flag;
- attachments and extraction status/text;
- view URL;
- index state.

Support optional bounded query parameters if useful, e.g. `max_chars`, but enforce server-side maxima.

## 18. Human source view

Provide:

```http
GET /view/{source}/{source_id}
```

This is intentionally simple and read-only. Render server-generated HTML containing:

- source/account/mailbox;
- From / To / Cc;
- Date;
- Subject;
- normalized plain-text body in escaped `<pre>` or equivalent;
- attachment names/types/sizes;
- extracted attachment text in escaped collapsible sections when reasonably sized.

Never inject raw email HTML into the page. Escape all untrusted content.

This stable URL is what an AI answer can cite to the user.

## 19. MCP server

Host the MCP Streamable HTTP endpoint at:

```text
/mcp
```

Expose at least two tools.

### 19.1 `mail_search`

Description should tell the agent this searches a private historical email archive and that iterative searches are encouraged.

Input schema:

```json
{
  "type": "object",
  "properties": {
    "query": {"type": "string"},
    "after": {"type": "string"},
    "before": {"type": "string"},
    "account_ids": {"type": "array", "items": {"type": "string"}},
    "mailboxes": {"type": "array", "items": {"type": "string"}},
    "senders": {"type": "array", "items": {"type": "string"}},
    "limit": {"type": "integer", "minimum": 1, "maximum": 50}
  },
  "required": ["query"]
}
```

Return structured content equivalent to the HTTP search result model.

### 19.2 `mail_get`

Input:

```json
{
  "source": "mailmux",
  "source_id": "82913",
  "max_chars": 50000
}
```

Return:

- full metadata;
- normalized body up to requested/server maximum;
- attachment metadata;
- extracted attachment text up to limits;
- explicit truncation flags;
- `view_url`.

The tool description should instruct agents to call `mail_get` before relying on a short search snippet for an important factual claim.

### 19.3 Deferred MCP tools

Do not implement in v1 unless trivial after core completion:

- `mail_get_thread`;
- `mail_get_attachment`;
- semantic search mode;
- mutation tools.

The v1 MCP server is read-only.

## 20. Internal module layout

Suggested structure:

```text
src/
  main.rs
  cli.rs
  config.rs

  storage/
    mod.rs
    turso.rs
    migrations.rs
    models.rs

  ingest/
    mod.rs
    parser.rs
    normalize.rs
    attachments.rs
    extractors/
      mod.rs
      text.rs
      html.rs
      pdf.rs

  index/
    mod.rs
    schema.rs
    writer.rs
    rebuild.rs

  search/
    mod.rs
    query.rs
    service.rs

  http/
    mod.rs
    ingest.rs
    search.rs
    documents.rs
    view.rs
    auth.rs

  mcp/
    mod.rs
    server.rs
    tools.rs
```

Keep Turso, Tantivy and MCP library-specific types close to their adapter modules rather than leaking them through the entire application.

## 21. Health/readiness

Expose:

```text
GET /health
GET /ready
```

`/health` means the process is alive and can open/query Turso.

`/ready` means:

- Turso migrations succeeded;
- Tantivy index opened/created;
- background index worker started;
- HTTP/MCP service is ready to accept work.

A backlog of pending documents does not make the service unready.

## 22. Observability

Use structured `tracing` logs.

Log fields where relevant:

- source/source_id/document_id;
- account/mailbox;
- attachment count;
- raw bytes;
- normalized body chars;
- extraction failures;
- index batch size and duration;
- search query duration and result count.

Never log entire email bodies, attachment contents, bearer tokens, or raw RFC 5322 messages.

Prometheus metrics are optional for the first implementation, but design the worker so counters/gauges can be added cleanly later.

## 23. Tests

Create fixture `.eml` messages under `tests/fixtures` covering:

1. plain-text email;
2. multipart alternative text+HTML;
3. HTML-only email;
4. UTF-8/non-ASCII subject and body;
5. text attachment;
6. PDF attachment with extractable text;
7. unsupported binary attachment;
8. malformed attachment that triggers extraction error;
9. duplicate RFC Message-ID across distinct `(source,source_id)` values;
10. missing Message-ID/date.

Required tests:

### Storage

- migrations from empty DB;
- upsert new document;
- idempotent same-hash ingestion;
- changed-hash replacement;
- attachment replacement transaction;
- foreign-key cleanup;
- pending/indexed/error state changes.

### Parsing

- canonical body chooses plain text rather than duplicating HTML alternative;
- HTML fallback conversion;
- metadata extraction;
- attachment extraction/truncation behavior.

### Indexing/search

- pending document is indexed by worker;
- reindex replaces prior Tantivy document rather than duplicating it;
- keyword body search;
- subject search ranking;
- attachment-text search;
- date/account/mailbox filters;
- deleted/replaced content no longer matches after commit;
- rebuild from Turso produces equivalent searchable corpus;
- simulated crash-state reconciliation: pending Turso row becomes searchable after restart/worker run.

### HTTP

- multipart ingestion;
- auth rejection;
- body size rejection;
- idempotent response codes;
- search API;
- document API;
- view endpoint HTML escaping.

### MCP

Using an MCP test client, verify:

- tool discovery includes `mail_search` and `mail_get`;
- schemas are correct;
- `mail_search` returns structured results;
- `mail_get` returns bounded content and source URL;
- invalid arguments produce MCP tool errors rather than process crashes.

## 24. Dependency guidance

At implementation time inspect current stable releases rather than blindly hard-coding versions from this document.

Expected crates include:

```text
tokio
axum
serde / serde_json
toml
clap
tracing / tracing-subscriber
thiserror / anyhow
turso
tantivy
rmcp
mail-parser
sha2
html-to-text helper (e.g. html2text)
pdf-extract
mime / multipart support as needed
```

As of the specification date:

- Tantivy 0.26.x is current and supports incremental indexing, deletes, commits, fast fields and BM25 search;
- Turso's Rust crate provides async local in-process database APIs and is still pre-1.0;
- the official Rust MCP SDK is actively evolving; use a released version compatible with the current stable MCP protocol and isolate SDK-specific glue.

## 25. Acceptance criteria

The project is complete when:

- [ ] A single `mailindex` process hosts HTTP APIs, MCP, Turso storage and Tantivy search.
- [ ] A raw RFC 5322 email can be uploaded with `(source,source_id)` provenance.
- [ ] Re-uploading identical bytes is idempotent.
- [ ] Changed bytes replace normalized content and trigger reindex.
- [ ] Turso stores canonical message and attachment metadata/text.
- [ ] Tantivy stores a disposable full-text index without duplicating full body/attachment text as stored fields.
- [ ] Historical bulk ingestion does not perform one Tantivy commit per email.
- [ ] Restart reconciles Turso pending state into Tantivy.
- [ ] `POST /v1/search` supports text + date/account/mailbox filters.
- [ ] `GET /v1/documents/...` returns normalized content.
- [ ] `GET /view/...` yields a stable escaped human-readable source page.
- [ ] MCP discovers `mail_search` and `mail_get` over Streamable HTTP.
- [ ] Search hits include stable source/view URLs.
- [ ] Text/HTML/PDF attachment extraction behaves as specified.
- [ ] Unsupported or broken attachments do not prevent email ingestion.
- [ ] `mailindex rebuild-index` can rebuild Tantivy from Turso.
- [ ] A reference mailmux submission script works with normal and historical mailmux processor invocations.
- [ ] Tests cover parsing, Turso persistence, Tantivy indexing, HTTP and MCP.

## 26. Implementation guidance for an autonomous coding agent

1. Establish the Turso storage model/migrations and fixture parsing first.
2. Implement idempotent HTTP ingestion and verify canonical rows before adding search.
3. Add Tantivy schema + background batch worker + restart reconciliation.
4. Add HTTP search/document/view endpoints using one internal `SearchService`.
5. Add MCP last, as a thin adapter over the already-tested search/document services.
6. Keep large content in Turso only; Tantivy should index it but not store duplicate copies unless a demonstrated requirement demands it.
7. Avoid experimental Turso FTS in v1 even though it exists; standalone Tantivy is intentional.
8. Run `cargo fmt`, `cargo clippy --all-targets --all-features`, and `cargo test` before completion.
9. Record any dependency/API changes discovered during implementation in README/decision notes instead of quietly changing architecture.
