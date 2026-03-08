# Rust LLM Crate Research

**Date:** 2026-02-23
**Context:** Evaluating provider-agnostic Rust crates for calling LLM APIs, to
replace the hand-rolled Anthropic HTTP client in `src/llm.rs`.

---

## Candidates Evaluated

| Crate            | Version | Total Downloads | Last Published | GitHub Stars | Verdict                             |
| ---------------- | ------- | --------------- | -------------- | ------------ | ----------------------------------- |
| `rig-core`       | 0.31.0  | 284,261         | 2026-02-17     | 6,105        | Strong but fast-moving and breaking |
| `genai`          | 0.5.3   | 123,610         | 2026-01-31     | 660          | **Recommended**                     |
| `langchain-rust` | 4.6.0   | 129,786         | 2024-10-06     | 1,232        | Stale — avoid                       |
| `llm` (graniet)  | 1.3.7   | 58,719          | 2026-01-09     | 312          | Niche adoption, awkward design      |
| `llm-chain`      | 0.13.0  | 82,452          | 2023-11-15     | 1,591        | Abandoned — avoid                   |

---

## Detailed Assessments

### `rig-core` — Feature-rich agentic framework

**Providers:** OpenAI, Anthropic, Azure, Cohere, DeepSeek, Gemini, Groq,
HuggingFace, Mistral, Moonshot, Ollama, OpenRouter, Perplexity, Together, xAI,
and more. AWS Bedrock and Google Vertex AI available as separate integration
crates.

**Basic usage:**

```rust
use rig::providers::anthropic;
use rig::completion::Prompt;

let client = anthropic::Client::from_env();
let agent = client
    .agent("claude-3-5-haiku-20241022")
    .preamble("Be concise.")
    .build();
let response = agent.prompt("What is Rust?").await?;
```

**Structured output** via `Extractor`, using `#[derive(JsonSchema)]`:

```rust
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
struct Transaction {
    pub amount: f64,
    pub transaction_type: String,
    pub narration: Option<String>,
}

let extractor = openai_client
    .extractor::<Transaction>(openai::GPT_4O)
    .build();
let tx = extractor.extract("Your debit of ₹1,234 at Amazon Pay").await?;
```

**Strengths:**

- Largest community by a wide margin (6k stars, active Discord)
- 20+ providers with a consistent abstraction layer
- Batteries-included: RAG, vector store integrations (LanceDB, Qdrant,
  pgvector, MongoDB, Neo4j), embeddings, streaming, tool/function calling,
  multi-agent orchestration, MCP (Model Context Protocol) support
- The `Extractor` API for structured output is the most ergonomic of all
  candidates — derive `JsonSchema` on a struct and the schema is generated
  automatically

**Weaknesses:**

- Provider-specific client types leak into application code:
  `anthropic::Client`, `openai::Client`, etc. Swapping providers requires
  code changes, not just a config change.
- The README carries an explicit warning: _"Here be dragons! future updates
  **will** contain breaking changes."_ The version history confirms this — 31
  major version bumps (v0.1 → v0.31) in approximately 18 months, releasing
  roughly every two weeks.
- `schemars` is a mandatory dependency for structured output, adding compile
  time and a derive macro stack.
- 121 open GitHub issues relative to its star count.

---

### `genai` — Focused, provider-agnostic chat completion library

**Providers:** OpenAI, Anthropic, Gemini, xAI, Ollama, Groq, DeepSeek, Cohere,
Together, Fireworks, and more. Auto-routes by model name prefix: names starting
with `gpt` → OpenAI, `claude` → Anthropic, `gemini` → Gemini, unrecognised →
Ollama.

**Basic usage:**

```rust
use genai::Client;
use genai::chat::{ChatMessage, ChatRequest};

let client = Client::default(); // reads API keys from env vars automatically

let res = client.exec_chat(
    "claude-haiku-4-5-20251001",   // change this string to switch providers
    ChatRequest::new(vec![
        ChatMessage::system("Be concise."),
        ChatMessage::user("What is Rust?"),
    ]),
    None,
).await?;
println!("{}", res.first_text().unwrap_or_default());
```

**Structured output** via `JsonSpec`, using a plain `serde_json::Value` schema:

```rust
use genai::chat::{ChatResponseFormat, JsonSpec};

let chat_req = ChatRequest::new(vec![
    ChatMessage::system("Extract transaction data from the email."),
    ChatMessage::user(&prompt),
])
.with_response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
    "transaction",
    serde_json::json!({
        "type": "object",
        "properties": {
            "status":           { "type": "string", "enum": ["found", "not_found"] },
            "amount":           { "type": "number" },
            "transaction_type": { "type": "string", "enum": ["deposit", "withdrawal"] },
            "narration":        { "type": "string" }
        },
        "required": ["status"]
    }),
)));
```

**Tool/function calling:**

```rust
use genai::chat::{Tool, ToolResponse};

let tool = Tool::new("get_exchange_rate")
    .with_description("Get the current exchange rate between two currencies")
    .with_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "from": { "type": "string" },
            "to":   { "type": "string" }
        },
        "required": ["from", "to"]
    }));

let chat_req = ChatRequest::new(vec![...]).with_tools(vec![tool]);
let res = client.exec_chat("gpt-4o-mini", chat_req, None).await?;
let tool_calls = res.into_tool_calls();
// ... execute tool locally, then continue the conversation
```

**Per-request options** (temperature, max tokens, etc.):

```rust
use genai::chat::ChatOptions;

let options = ChatOptions::default()
    .with_temperature(0.0)
    .with_max_tokens(512);

client.exec_chat("claude-haiku-4-5-20251001", chat_req, Some(&options)).await?;
```

**Strengths:**

- Single `Client` dispatches to all providers transparently. Switching
  providers is a model name string change — no code restructuring, no import
  changes.
- No breaking-change warning. The 0.5.x stable series has received regular
  patch releases with a disciplined changelog.
- Structured output uses plain `serde_json::Value` — no extra derive
  dependencies (no `schemars`).
- Lean dependency footprint: tokio, reqwest 0.13, serde, serde_json, futures.
- Includes reasoning/thinking model support (DeepSeek R1, Claude extended
  thinking, Gemini thinking) — useful for future workloads.
- Active development: a 0.6 beta series was in progress at time of research
  (v0.6.0-beta.2 published 2026-02-22).

**Weaknesses:**

- Smaller community than rig-core (660 vs 6,105 stars).
- No built-in RAG or vector store abstractions (by design — the author
  recommends provider-specific SDKs for those use cases).
- `JsonSpec` structured output is silently ignored on providers that don't
  support it; error handling for this case is planned but not yet implemented
  in the 0.5.x line.
- The 0.6 series introduces breaking changes and was in WIP beta at time of
  research.

---

### `langchain-rust` — Not recommended

A Rust port of LangChain. Despite reasonable total download counts accumulated
over time, the last substantive development was in late 2024 and the project
has not tracked the upstream Python LangChain project. Effectively stale.

### `llm` (graniet) — Not recommended

Providers are compile-time feature flags, making runtime provider selection
awkward. Low adoption (312 stars). The crate name previously belonged to an
archived local-inference project (`rustformers/llm`), causing confusion in
searches and documentation.

### `llm-chain` — Abandoned

Last published to crates.io in November 2023. Do not use for new projects.

---

## Recommendation: `genai`

For the mailtx use case — a single structured extraction call per
email, with the ability to switch LLM providers via config — `genai` is the
correct choice.

The decisive factor is the single-client architecture. With `genai`, changing
from Anthropic to OpenAI (or any other supported provider) requires no code
changes: update the `ANTHROPIC_MODEL` environment variable to a model name from
a different provider and it works. This is the definition of provider-agnostic.

`rig-core` would be preferable if the project needed agent pipelines, vector
store integrations, or the compile-time structured output ergonomics of
`JsonSchema` derivation. None of those requirements apply here.

**Dependency to add:**

```toml
genai = "0.5"
```

**Environment variable to rename:** `ANTHROPIC_MODEL` → `LLM_MODEL` (since the
model is no longer Anthropic-specific). The value `claude-haiku-4-5-20251001`
continues to work unchanged; switching to `gpt-4o-mini` or `gemini-2.0-flash`
requires only updating the env var.
