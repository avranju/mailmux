# Manual Testing Guide for mailtx

`mailtx` reads JSON from stdin, loads a `.eml` file from disk, calls an LLM to extract transaction details, runs the deterministic account matcher, and POSTs to a Firefly III HTTP endpoint. To test it manually you need to provide: a config file, a sample email, and a mock HTTP server.

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
error_if_duplicate_hash = false

[[firefly.asset_accounts]]
id = "hdfc_savings"
firefly_account_id = "12"
account_suffixes = ["9772"]
debit_card_last4 = ["7406"]
aliases = ["hdfc savings"]
```

## 3. Run a mock HTTP server

Start a Python server that accepts the Firefly POST and prints the request body:

```bash
python3 -c "
import http.server, json

class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers['Content-Length'])
        body = self.rfile.read(length)
        print('--- REQUEST ---')
        print(json.dumps(json.loads(body), indent=2))
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(b'{\"data\":{\"id\":\"42\"}}')
    def log_message(self, *a): pass

http.server.HTTPServer(('localhost', 8080), H).serve_forever()
" &
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
