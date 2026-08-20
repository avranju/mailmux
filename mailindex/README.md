# mailindex

`mailindex` is a local-first RFC 5322 archive index. It accepts multipart uploads,
parses and bounds normalized content into a local Turso database, and maintains a
rebuildable Tantivy index. HTTP and read-only MCP clients share the same search and
retrieval service.

## Run

```sh
MAILINDEX_API_TOKEN=secret cargo run -p mailindex -- --config mailindex/config.example.toml
```

The example uses loopback and a token. `PUT /v1/documents/{source}/{source_id}`
accepts exactly one `metadata` JSON field and one `message` field. New and changed
messages return 202 after the Turso transaction commits; identical raw bytes return
200. Search is `POST /v1/search`, retrieval is `GET /v1/documents/...`, and the
escaped citation page is `/view/...`. `/mcp` provides `mail_search` and `mail_get`
over Streamable HTTP. `/health` and `/ready` are unauthenticated.

`api_token_env` names an environment variable, never a literal secret. Auth protects
uploads, JSON APIs, and MCP; views are protected by default and can be made public with
`protect_view = false`. The configured request, body, attachment, and
search limits are enforced; attachment extraction is best effort and never rejects a
message. Raw messages and attachment bytes are not stored.

Upload metadata may contain `account_id`, `mailbox_name`, and `uid`; the complete
object is retained as producer metadata JSON. Identity is the `(source, source_id)`
path pair, not RFC `Message-ID`. Search requires nonblank text and supports inclusive
`after`, exclusive `before`, account/mailbox/sender OR groups, and limits from 1 to 50.
`max_chars` applies across the body and all attachment text. Retrieval responses
separate canonical `body_truncated`/attachment `text_truncated` flags from
per-response `body_response_truncated`/attachment `response_truncated` flags;
the aggregate response flag is set only when characters are omitted. A changed upload is
durable before 202; a same-hash upload is a 200 no-op and wakes pending work.

Turso 0.8.0-pre.6 is used without its FTS/default features. Tantivy 0.26.1 is a
separate disposable index. The service uses Tantivy's single writer and a batching
worker; canonical Turso rows remain the source of truth. The `rebuild-index` command
must be run with the serving process stopped and builds a sibling index before a
backup-and-swap installation. `index-status` reports canonical state counts.

```sh
mailindex --config /etc/mailindex/config.toml index-status
# Stop the serving process first; rebuild uses Tantivy's writer lock.
mailindex --config /etc/mailindex/config.toml rebuild-index
```
Rebuild streams ordered Turso rows into a temporary sibling, verifies the document
count, and installs it with an active-index backup/restoration path. Temporary and
backup siblings use collision-resistant UUID names, and pre-existing backup or
temporary siblings from earlier failed runs are never deleted or overwritten; they
are retained for operator recovery. `index-status` opens only Turso and reports
canonical pending/indexed/error counts.

## mailmux adapter

`contrib/mailmux-submit.sh` reads the mailmux command processor JSON from stdin,
uploads raw bytes from `email.raw_message_path`, and emits ProcessorOutput JSON. It
uses `MAILINDEX_URL` and `MAILINDEX_API_TOKEN` from the environment and exits nonzero
for validation, transport, or non-2xx failures. For example:

```sh
MAILINDEX_URL=http://127.0.0.1:8090 MAILINDEX_API_TOKEN=secret \
  mailindex/contrib/mailmux-submit.sh <processor-input.json
```

The implementation uses local Turso 0.8.0-pre.6 (default features disabled), Tantivy
0.26.1, rmcp 3.1.3 Streamable HTTP, mail-parser 0.11, and best-effort html2text and
pdf-extract attachment extraction. MCP exposes only read-only `mail_search` and
`mail_get` tools and shares HTTP search/retrieval behavior.
