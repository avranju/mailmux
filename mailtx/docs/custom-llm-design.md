# Design: configurable LLM servers for mailtx

**Status:** Implemented. See [mailtx configuration](../README.md#configuration) and the [manual testing guide](manual-testing.md) for current usage.

## Recommendation

Add an explicit `openai_compatible` LLM backend accepting a model name, base
URL, and endpoint path. This supports llama.cpp and llama-swap without
server-specific adapters. Preserve the existing `genai` backend for automatic
cloud-provider routing and existing configurations.

Use the existing `reqwest` dependency for the custom backend's small,
non-streaming Chat Completions implementation. Share the extraction prompt,
transaction schema, output parsing, and validation between both backends. No
new production dependencies, database changes, or mailmux protocol changes
are needed.

“Arbitrary” means arbitrary **model names and server locations implementing
this API**, not arbitrary HTTP request/response protocols. A URL cannot tell
mailtx how to serialize an unknown API. Native llama.cpp `/completion`, native
Ollama `/api/chat`, Anthropic Messages, and OpenAI Responses are different
protocols; they are not interchangeable endpoint paths. Existing provider
support remains available through `genai`. Other custom protocols can be added
as explicit backends if needed.

## Pre-implementation assessment

The following assessment describes the source before this design was implemented.

## Current implementation and gaps

The assessment is based on the source and the workspace's `genai` **0.6.5**
lockfile entry. `mailtx/Cargo.lock` still contains 0.5.3; workspace builds use
`Cargo.lock` at the repository root.

| Location | Current behavior | Required change |
| --- | --- | --- |
| `src/config.rs` | Only `llm_model` is configurable; provider is inferred from its name | Explicit backend, URL, endpoint, authentication, and request settings |
| `src/main.rs` | Constructs `genai::Client::default()` and passes a model string | Construct an application-owned LLM client from resolved settings |
| `src/llm.rs` | Builds prompt/schema, requests `JsonSpec`, parses text | Separate shared extraction logic from transport; make JSON mode selectable |
| `src/main.rs` | Any status other than `found` is treated as a successful skip | Reject invalid statuses; only `not_found` is a deliberate skip |

Unknown model names normally fall back to genai's **native Ollama adapter**.
Consequently, changing `llm_model` to a llama-swap model ID does not select
llama-swap or OpenAI Chat Completions. Recognized names can instead route to a
cloud provider. Neither is appropriate for an explicitly configured server.

The current prompt also relies on the API schema to describe several fields;
merely removing `response_format` would not provide a sufficiently explicit
JSON contract. The existing schema only requires `status`, which needs
adjustment for strict OpenAI-style schema output.

Some existing README/AGENTS descriptions still describe the old Anthropic-only
implementation. Update the LLM-related descriptions when implementing this
feature.

## Why not just configure genai?

That is a viable alternative, not a library limitation:

- `ServiceTarget` / `ModelSpec::Target` selects an explicit adapter, endpoint,
  and auth, bypassing model-name provider inference and default auth resolution.
- `AdapterKind::OpenAI` selects Chat Completions, not the Responses API.
- `Endpoint` provides a base URL. The OpenAI adapter appends
  `chat/completions` using URL joining, so a trailing slash matters.
- `AuthData::RequestOverride` can replace the final URL **and headers**. This
  supports a custom endpoint and genuinely unauthenticated requests.
- `Client::builder().with_reqwest(...)` supports an HTTP client with explicit
  timeouts. `ChatOptions::extra_body` supports server-specific parameters.

A standard `/v1/chat/completions` server can therefore be supported entirely
with genai. However, a base-URL override alone does not implement configurable
endpoint paths or no-auth behavior. Passing `AuthData::None` to its OpenAI
adapter is not sufficient: that adapter expects a single API key unless a
request override is used.

For the requested literal configuration contract, a narrow reqwest backend is
clearer. Genai also interprets `::` namespaces and reasoning suffixes in model
names, and chooses some request fields based on model-name prefixes. These
are useful for provider routing but undesirable for opaque llama-swap IDs.
Avoid dummy cloud credentials, synthetic model names, and payload overrides
solely to defeat these heuristics.

**Trade-off:** mailtx maintains a small subset of the OpenAI wire protocol.
Keep it deliberately limited to text-only, non-streaming extraction and cover
it with mock HTTP tests. Do not replace genai's existing provider adapters or
build a general-purpose LLM SDK.

## Configuration

### llama.cpp

Keep the existing sender and Firefly settings; replace `llm_model` with:

```toml
[llm]
backend = "openai_compatible"
model = "bank-extractor"
base_url = "http://127.0.0.1:8080"
endpoint = "/v1/chat/completions"
allow_insecure_http = true
response_format = "json_schema"
timeout_secs = 180
connect_timeout_secs = 10
max_tokens = 1024
temperature = 0.0
# api_key_env = "MAILTX_LLM_API_KEY"
```

Configure llama-server with a compatible instruction/chat model and an alias
such as `--alias bank-extractor`. Alternatively use its model ID from
`GET /v1/models`. API keys are optional unless authentication is enabled on
the server.

### llama-swap

The same backend is used; only connection settings and the model ID change:

```toml
[llm]
backend = "openai_compatible"
model = "bank-extractor" # llama-swap model ID or alias, not necessarily a GGUF filename
base_url = "http://127.0.0.1:9292"
endpoint = "/v1/chat/completions"
allow_insecure_http = true
response_format = "json_schema"
timeout_secs = 180
max_tokens = 1024
```

Send the configured model string unchanged. Llama-swap is responsible for
loading/swapping the model and any upstream name rewriting (`useModelName`).
Mailtx should not start processes, poll administrative endpoints, or manage
model lifecycle. Do not require a `/v1/models` lookup before every request;
explicit IDs should also work for unlisted models.

For reasoning models, optional server-specific parameters can be expressed as
TOML data and converted to JSON:

```toml
[llm.extra_body]
chat_template_kwargs = { enable_thinking = false }
```

This setting is template/server-dependent, not a universal way to disable
reasoning. Llama-swap filters may also override or remove request settings.
JSON guarantees ultimately depend on the upstream inference server.

### Settings and defaults

| Setting | Proposed behavior |
| --- | --- |
| `backend` | `auto` by default; alternatively `openai_compatible` |
| `model` | Required and nonblank for custom servers; existing Claude default for `auto` |
| `base_url` | Required for `openai_compatible`; no default server or cloud fallback |
| `endpoint` | Default `/v1/chat/completions`; a path under the base URL |
| `api_key_env` | Optional explicit env-var name for a Bearer token; absent means no Authorization header |
| `response_format` | `json_schema` by default; also `json_object` or `prompt` |
| `timeout_secs` | Total LLM HTTP request deadline, default 60 seconds |
| `connect_timeout_secs` | Default 10 seconds; must not exceed the total deadline |
| `max_tokens` | Custom backend default 1024; preserve genai's existing behavior when unset in `auto` |
| `temperature` | Custom backend default 0.0; preserve existing behavior when unset in `auto` |
| `allow_insecure_http` | Default false; explicit permission for plaintext transport to the configured LLM host |
| `extra_body` | Custom-backend-only JSON-compatible object, default empty |

Use typed enums for backend/response format, reject unknown settings in the
new `[llm]` table, and reject custom-only settings in `auto` mode. Validate
positive deadlines/token limits and finite, supported temperature values.
Zero temperature reduces variation; it does not guarantee determinism.

`extra_body` adds top-level server-specific fields. Reject collisions with
managed keys (`model`, `messages`, `stream`, `response_format`, `max_tokens`,
`max_completion_tokens`, `temperature`, and `n`) rather than silently
changing routing, output mode, or request limits. Core request construction
remains authoritative.

### Backward compatibility

Continue accepting:

```toml
llm_model = "claude-haiku-4-5-20251001"
```

Resolution rules:

1. No `[llm]`: use the legacy model or the current default, with `auto` routing.
2. `[llm]` present: use its settings.
3. Both `[llm]` and an explicitly supplied `llm_model`: configuration error.

Represent the legacy model as optional during deserialization so its default
does not look like an explicitly supplied value. Apply defaults during
resolution into an internal `ResolvedLlmConfig`.

In `auto`, keep genai's normal provider-key environment variables. In custom
mode, **never implicitly read `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or other
inherited provider keys**. If `api_key_env` is set but missing or empty, fail
before issuing the LLM request; do not proceed anonymously or fall back.

The new finite default deadline is an intentional operational improvement over
the currently unbounded LLM call; document it for existing deployments.

## URL and transport contract

Treat the base URL as a **path prefix**, not just an origin:

| Base URL | Endpoint | Final URL |
| --- | --- | --- |
| `http://localhost:8080` | `/v1/chat/completions` | `http://localhost:8080/v1/chat/completions` |
| `https://example.com/inference/` | `/v1/chat/completions` | `https://example.com/inference/v1/chat/completions` |
| `https://example.com/v1` | `/chat/completions` | `https://example.com/v1/chat/completions` |

Normalize only the boundary slash. Do not infer, insert, or deduplicate `/v1`.
Do not use ordinary `Url::join` with a leading-slash endpoint, because that
would discard the base path prefix.

Validate the base as an absolute HTTP(S) URL with a host and no embedded
credentials, query, or fragment. Require an endpoint starting with exactly one
slash; reject absolute URLs, query/fragment components, and traversal segments
(including encoded traversal). Construct the path on the parsed URL and
verify that the final origin is unchanged. Signed/query-based URLs are outside
this first design; add them explicitly if a real deployment requires them.

Use a dedicated LLM HTTP client, not one carrying Firefly credentials:

- Explicit connection and total-request deadlines.
- Redirects disabled, preventing destination changes and HTTPS downgrades.
- Normal TLS certificate validation; no `danger_accept_invalid_certs` knob.
- HTTPS by default. HTTP requires `llm.allow_insecure_http = true`.
- Bearer auth only when configured; mark header values sensitive.
- Bound response bodies, including chunked responses, to a small limit such as
  1 MiB before parsing.

The LLM HTTP override deliberately permits the configured host, including
LAN/container hosts, for self-hosted deployments. It exposes email contents
and any LLM credentials on that transport and must be clearly documented.
Use HTTPS or an encrypted tunnel for untrusted networks. **Firefly's existing
loopback-only HTTP exception is unchanged**; these are separate settings.

## Internal architecture

Keep the application-specific interface small; an enum is sufficient for two
backends, without introducing a plugin framework:

```text
Config::load / resolve LLM settings
  -> LlmClient::from_config
      -> Auto(genai client + model)
      -> OpenAiCompatible(reqwest client + resolved server settings)

LlmClient::extract_transaction(subject, body, categories)
  -> build shared prompt + schema
  -> execute selected transport
  -> obtain final text
  -> parse and validate TransactionData
  -> existing account matching / transfers / Firefly posting
```

Keep `TransactionData` in `llm.rs`, so its consumers retain their existing
module path. Factor helpers for prompt construction, schema generation, and
parsing/validation; put the custom wire implementation in
`src/llm/openai_compatible.rs`. Keep the short genai implementation in `llm.rs`
unless it grows enough to justify a separate module.

`main.rs` should know neither provider-specific auth nor endpoint assembly.
Replace its default client/model wiring with the configured application client;
preserve sender skips, categories, account matching, transfer state,
idempotency, metrics, and ProcessorOutput behavior.

### Custom request and response

Issue one POST with approximately this payload:

```json
{
  "model": "bank-extractor",
  "messages": [
    {"role": "system", "content": "Extraction instructions and JSON contract"},
    {"role": "user", "content": "Subject, email body, and category choices"}
  ],
  "stream": false,
  "temperature": 0.0,
  "max_tokens": 1024,
  "response_format": {
    "type": "json_schema",
    "json_schema": {
      "name": "transaction",
      "strict": true,
      "schema": {
        "type": "object",
        "properties": {
          "status": {"type": "string", "enum": ["found", "not_found"]},
          "amount": {"type": ["number", "null"]},
          "transaction_type": {
            "type": ["string", "null"],
            "enum": ["deposit", "withdrawal", null]
          },
          "narration": {"type": ["string", "null"]},
          "transaction_date": {"type": ["string", "null"]},
          "category": {"type": ["string", "null"]}
        },
        "required": [
          "status", "amount", "transaction_type", "narration",
          "transaction_date", "category"
        ],
        "additionalProperties": false
      }
    }
  }
}
```

Deserialize a small typed response envelope and read
`choices[0].message.content`. Ignore unrelated fields such as timings, usage,
and separate `reasoning_content`. Empty choices, missing/empty/non-text
content, refusals, or tool-call responses are errors. An explicit
`finish_reason = "length"` is a truncation error even if a JSON prefix parses;
content filtering is also an error. Permit a missing finish reason for
compatible servers if the final content otherwise passes validation.

Do not search arbitrary prose for a JSON-looking substring. Retain conservative
support for a single enclosing Markdown code fence. Embedded thinking/prose
must either be prevented with server/template settings or fail parsing; do
not guess which parts of a financial extraction are safe to discard.

## Structured output and validation

Support three explicit modes, without automatic retries/downgrades:

| Mode | Wire behavior | Guarantee |
| --- | --- | --- |
| `json_schema` | OpenAI-style `response_format.json_schema` | Schema-constrained output when supported by server |
| `json_object` | `response_format = { "type": "json_object" }` | JSON syntax, not transaction field correctness |
| `prompt` | Omit `response_format` | No server-enforced JSON guarantee |

Current llama.cpp supports JSON and schema-constrained Chat Completions.
Its request parser accepts the standard nested `json_schema.schema` envelope.
Older builds and other servers may differ; test the deployed version and
select a weaker mode explicitly if necessary. Llama-swap forwards to its
upstream; it does not independently enforce the schema.

Include an explicit JSON field/type contract in the shared prompt **in every
mode**, plus examples for `found` and `not_found`. Treat the email as untrusted
input, not instructions. Preserve the current category preference and date
extraction instructions.

Use a flat strict-output-compatible schema:

- Object with `additionalProperties: false`.
- All six fields required: `status`, `amount`, `transaction_type`,
  `narration`, `transaction_date`, and `category`.
- `status`: `found` or `not_found`.
- `amount`: number or null.
- `transaction_type`: `deposit`, `withdrawal`, or null.
- Narration/date/category: string or null.

Use null for unavailable values; for `not_found`, instruct the model to return
null transaction fields. Avoid complex conditional schema branches, which
are less portable across grammar-constrained local servers.

Client-side validation is mandatory regardless of mode:

- Unknown status is an error, **never** a successful `no_transaction` skip.
- `found` requires a finite, nonzero amount and a supported transaction type.
  Preserve the current signed-amount handling in Firefly rather than changing
  banking semantics as part of this feature.
- Preserve existing narration/date fallbacks and optional category behavior.
- Missing optional fields may remain accepted in weaker modes for compatibility;
  required business fields must still pass validation.

Only an explicitly valid `not_found` is a deliberate skip. This prevents a
less capable model's malformed output from silently discarding bank emails.

## Deadlines, failures, privacy, and observability

Cold model loading and swapping can take much longer than a cloud round-trip.
The LLM deadline includes connection, queue/swap/load time, inference, and
response-body reading. For the 180-second examples, start with mailmux
`timeout_secs = 240` and `concurrency = 1`, then tune for actual cold-start
latency and Firefly work. Account for category fetching, posting, and any
expired-transfer flushes as well. Coordinate with llama-swap's health-check
and proxy timeouts.

Use no in-process retries in the first implementation. Connection failures,
timeouts, non-2xx responses, malformed envelopes, and invalid extraction all
exit nonzero and use mailmux's existing retry schedule. Permanent 4xx errors
still fail under the current protocol; do not turn them into successful skips.
Do not silently downgrade JSON mode or retry against a cloud provider.

Preserve existing `llm_calls_total{result="success"|"error"}` and stdout
ProcessorOutput metadata. Log backend, configured model, elapsed time, and
safe failure categories to stderr; do not add URLs/models as metric labels.
Never include credentials, request bodies, raw model responses, or raw HTTP
error bodies in errors. Current `llm.rs` includes extracted JSON in parse
errors; remove that. Also sanitize genai error variants that embed request
payloads rather than printing their full error chains.

A related pre-existing issue is that mailmux's command processor timeout does
not explicitly enable `kill_on_drop` on the child. Expiring the wait future
can leave a child running. Track explicit child termination as a separate
mailmux hardening change so retries cannot overlap orphaned processors; an
LLM request deadline alone is not a whole-process deadline.

## Implementation and verification plan

1. **Configuration:** add raw/resolved LLM types, legacy resolution, typed
   modes, URL validation, auth policy, and deadline validation.
2. **Shared extraction:** factor prompt/schema/parser; make the schema nullable
   and strict-compatible; reject invalid status and required business fields.
3. **Transport:** implement the custom reqwest backend; adapt existing genai
   execution to the application client and configurable modes/deadlines.
4. **Integration:** switch `main.rs` wiring; retain all downstream behavior.
5. **Documentation:** update `README.md`, `AGENTS.md`, and
   `docs/manual-testing.md` with cloud, llama.cpp, llama-swap, HTTP security,
   authentication, and cold-start timeout examples.

### Automated tests

Use local mock HTTP servers (the existing Firefly tests already use TCP
fixtures; a dev-only mock library is also reasonable). No real LLM or cloud
credentials should be required for the default test suite.

- Legacy/default configuration, new auto/custom configuration, conflicting
  configuration, unknown keys, missing custom model/URL, invalid deadlines.
- URL boundary slashes, base-path prefixes, default/custom endpoint, `/v1`
  behavior, absolute-endpoint/traversal rejection, transport security policy.
- Exact request path and unchanged model IDs, including cloud-looking names,
  slashes, colons, `::`, and reasoning-looking suffixes.
- No auth without `api_key_env` **even when cloud provider keys are inherited**;
  explicit Bearer auth, missing/empty configured key, no Firefly-token leakage.
  Avoid mutating process-global env concurrently in Rust tests; inject an env
  lookup or run isolated subprocesses.
- Every JSON mode, schema/nullability, prompt field contract/category lists,
  extra-body parameters, and protected-key rejection.
- Valid found/not-found responses; fences; invalid status; missing required
  business data; empty/wrong response shapes; refusal/tool calls; truncation;
  malformed JSON; excessive response size; separate reasoning fields.
- 401/404/429/5xx, connection failure, timeout, redirect rejection, and error
  redaction. Assert that request/response secrets cannot reach log messages.
- Pipeline regression: valid extraction posts the same Firefly transaction;
  valid `not_found` does not post; malformed extraction fails rather than
  becoming `no_transaction`; event-based idempotency remains intact.

### Manual acceptance

Run representative debit, credit, HTML-only, and non-transaction fixtures
against both llama.cpp and llama-swap. Exercise cold and warm models,
authenticated and unauthenticated servers, and the deployed JSON modes.
Validate extraction accuracy, not just successful HTTP/JSON: constrained JSON
can still contain an incorrect amount, date, or category. Use a mock Firefly
endpoint or a separate test instance before processing real bank emails.

## Scope boundaries

Defer native custom protocols, streaming, tools, multimodal extraction,
provider discovery, model lifecycle management, automatic cloud fallback,
request/response mapping templates, and financial numeric-type changes.
The two-backend boundary leaves room for explicit new protocols without
changing transaction processing.

## References

- [llama.cpp server API documentation](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)
- [llama.cpp Chat Completions request parser](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/server-common.cpp)
- [llama-swap overview and supported endpoints](https://github.com/mostlygeek/llama-swap/blob/main/README.md)
- [llama-swap configuration example](https://github.com/mostlygeek/llama-swap/blob/main/docs/config.example.yaml)
- [genai 0.6.5 ServiceTarget](https://docs.rs/genai/0.6.5/genai/struct.ServiceTarget.html)
- [genai 0.6.5 AuthData](https://docs.rs/genai/0.6.5/genai/resolver/enum.AuthData.html)

Upstream server documentation tracks moving branches. The relevant API
contracts should be tested against the actual server versions deployed.
