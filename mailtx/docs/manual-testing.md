# Manual Testing Guide for mailtx

`mailtx` reads JSON from stdin, loads a `.eml` file from disk, calls an LLM to extract transaction details, runs the deterministic account matcher, and POSTs to a Firefly III HTTP endpoint. To test it manually you need to provide: a config file, a sample email, and a mock HTTP server.

## Cloud/auto and local OpenAI-compatible acceptance

The existing cloud example below exercises legacy `llm_model` routing. For local
acceptance, run Firefly mock on 8080, llama.cpp on 8081, or llama-swap on 9292.
**The deployed llama.cpp/llama-swap acceptance has not been performed in this
repository environment**; these procedures are operator-run and model/server
specific.
The local LLM modes require neither cloud credentials nor inherited cloud keys.

For llama.cpp, start `llama-server -m /models/bank.gguf --host 127.0.0.1 --port 8081`.
For llama-swap, configure a model entry in its `models` configuration pointing to
your GGUF/model path, start the service on port 9292, then use its Chat
Completions-compatible API. Exact model-launch options vary by deployed version;
verify `/v1/chat/completions` is enabled before acceptance.

```toml
[llm]
backend = "openai_compatible"
model = "bank-extractor"
base_url = "http://127.0.0.1:8081" # llama.cpp; use 9292 for llama-swap
allow_insecure_http = true
endpoint = "/v1/chat/completions"
response_format = "json_schema" # repeat with json_object and prompt
# api_key_env = "MAILTX_LLM_API_KEY" # omit for unauthenticated server
```

Exercise the debit fixture below, then create and run these additional fixtures:

```bash
cat > /tmp/mailtx-test/credit.eml <<'EOF'
From: alerts@mybank.com
Subject: Salary credited INR 50000
Content-Type: text/plain

Your account XX9772 was credited with INR 50,000.00 on 09-Mar-2026. Narration: ACME Salary.
EOF
cat > /tmp/mailtx-test/html-only.eml <<'EOF'
From: alerts@mybank.com
Subject: Card purchase
MIME-Version: 1.0
Content-Type: text/html; charset=utf-8

<html><body><p>INR 250.00 debited from account XX9772 on 10-Mar-2026. Narration: Cafe.</p></body></html>
EOF
cat > /tmp/mailtx-test/non-transaction.eml <<'EOF'
From: alerts@mybank.com
Subject: Monthly statement available
Content-Type: text/plain

Your monthly statement is now available online. No transaction was made.
EOF
```

Replace `llm_model` in the sample TOML with the `[llm]` table above (do not
append it: explicit `llm_model` plus `[llm]` is a configuration error). Run each
fixture using its path in stdin JSON. Run each supported `json_schema`,
`json_object`, and `prompt` mode; repeat cold and warm, with and without
configured API-key authentication. For the deadline check set both
`timeout_secs = 1` and `connect_timeout_secs = 1` (the connection deadline may
not exceed the total deadline). Expected: found exits 0 and has
`metadata.outcome=posted`; valid not_found exits 0 and has `no_transaction` with
no transaction POST; malformed output or timeout exits nonzero with
`metadata.outcome=error` and LLM error metrics. Check mock requests and manually
verify amount, date, direction, and category. Never use a real financial account
for initial validation.

## 1. Create a sample `.eml` file

```bash
mkdir -p /tmp/mailtx-test
```

Create `/tmp/mailtx-test/sample.eml`:

```
From: alerts@mybank.com
To: me@example.com
Subject: HDFC Bank: Debit of INR 1,234.56 from A/c XX9772
Date: Mon, 09 Mar 2026 10:30:00 +0530
Content-Type: text/plain

Dear Customer,

INR 1,234.56 has been debited from your HDFC Bank Account XX9772 on 09-Mar-2026.
Narration: Amazon Pay
Available balance: INR 45,678.90

This is an auto-generated message.
```

## 2. Create a config TOML

Create `/tmp/mailtx-test/config.toml`:

```toml
allowed_senders = ["alerts@mybank.com"]
llm_model = "claude-haiku-4-5-20251001"

[firefly]
base_url = "http://localhost:8080"
access_token = "test-token"
default_asset_account_id = "1"
apply_rules = false
fire_webhooks = false
error_if_duplicate_hash = true
allow_insecure_http = true  # Only safe for the local mock server below

[[firefly.asset_accounts]]
id = "hdfc_savings"
firefly_account_id = "12"
account_suffixes = ["9772"]
debit_card_last4 = ["7406"]
aliases = ["hdfc savings"]
```

## 3. Run a mock HTTP server

Start this stateful mock. It implements category GET, external-ID count GET,
and transaction POST; posted IDs are retained so replaying the same input skips
an additional POST:

```bash
python3 -c '
import http.server, json, urllib.parse
ids = set()
class H(http.server.BaseHTTPRequestHandler):
    def send_json(self, obj):
        b=json.dumps(obj).encode(); self.send_response(200); self.send_header("Content-Type","application/json"); self.send_header("Content-Length",str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        u=urllib.parse.urlparse(self.path)
        if u.path.endswith("/v1/categories"):
            self.send_json({"data":[],"meta":{"pagination":{"current_page":1,"total_pages":1}}})
        elif u.path.endswith("/v1/search/transactions/count"):
            external=urllib.parse.parse_qs(u.query).get("external_identifier",[""])[0]
            self.send_json({"count":int(external in ids)})
        else: self.send_error(404)
    def do_POST(self):
        body=json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        print(json.dumps(body,indent=2))
        for tx in body.get("transactions",[]):
            if tx.get("external_id"): ids.add(tx["external_id"])
        self.send_json({"data":{"id":"42"}})
    def log_message(self,*a): pass
http.server.HTTPServer(("127.0.0.1",8080),H).serve_forever()
' &
```

## 4. Run mailtx

```bash
echo '{
  "event": {"id": 1},
  "email": {
    "subject": "HDFC Bank: Debit of INR 1,234.56 from A/c XX9772",
    "sender": "HDFC Bank Alerts <alerts@mybank.com>",
    "raw_message_path": "/tmp/mailtx-test/sample.eml"
  }
}' | MAILTX_CONFIG=/tmp/mailtx-test/config.toml \
     ANTHROPIC_API_KEY=your-key-here \
     cargo run -p mailtx
```

Or if already built:

```bash
echo '{"event":{"id":1},"email":{"subject":"HDFC Bank: Debit of INR 1,234.56 from A/c XX9772","sender":"alerts@mybank.com","raw_message_path":"/tmp/mailtx-test/sample.eml"}}' \
  | MAILTX_CONFIG=/tmp/mailtx-test/config.toml \
    ANTHROPIC_API_KEY=your-key-here \
    ./target/debug/mailtx
```

## What to watch

| Stream | What you'll see |
|--------|----------------|
| **stderr** | Tracing logs: sender check, LLM result, account match method, POST result |
| **Mock server stdout** | The exact JSON body sent to Firefly |
| **Exit code** | 0 = success or deliberate no-op, 1 = error |

## Testing edge cases

- **Sender not in allowlist** — change `sender` in the stdin JSON to something not in `allowed_senders`; should exit 0 silently with no POST.
- **No transaction found** — use a non-transaction email body; the LLM should return `status: "not_found"` and mailtx exits 0 without posting.
- **Account matcher fallback** — remove account-specific signals (card/account numbers, aliases) from the email body; resolution should fall back to `default_asset_account_id`.
- **Different LLM provider** — change `llm_model` to e.g. `"gemini-2.0-flash"` and set `GEMINI_API_KEY` instead of `ANTHROPIC_API_KEY`.
