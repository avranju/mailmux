# mailtx Tester Tool

`tools/tester/tester.py` is an interactive manual testing harness for `mailtx`.
It wires together a mock Firefly HTTP server, the mailtx binary, and a
terminal UI so you can feed a real `.eml` file through the full pipeline and
observe the results without a live Firefly instance.

## Location

```
mailtx/tools/tester/
├── .python-version   ← pins Python 3.14
├── pyproject.toml    ← uv project manifest
└── tester.py
```

## Usage

```bash
cd mailtx/tools/tester
uv run tester.py <config.toml> <xai-api-key> <email.eml> [<email2.eml> ...]
```

| Argument | Description |
|---|---|
| `config.toml` | Path to a mailtx config file |
| `xai-api-key` | X AI API key, passed as `XAI_API_KEY` to the child process |
| `email.eml ...` | One or more `.eml` files to process in order |

## What It Does

1. **Starts a mock HTTP server** on `localhost:8080` that stands in for Firefly
   III. It accepts `POST /v1/transactions`, pretty-prints the request body, and
   responds with HTTP 200 and a randomly generated transaction ID:
   ```json
   { "data": { "id": "481923" } }
   ```

2. **Parses each `.eml` file** to extract the `Subject` and `From` headers.

3. **Builds the stdin payload** that mailtx expects from mailmux — a JSON
   document with a randomly generated event ID, the extracted subject and
   sender, and the absolute path to the `.eml` file:
   ```json
   {
     "event": { "id": 748291 },
     "email": {
       "subject": "HDFC Bank: Debit of INR 1,234.56 from A/c XX9772",
       "sender": "HDFC Bank Alerts <alerts@mybank.com>",
       "raw_message_path": "/absolute/path/to/email.eml"
     }
   }
   ```

4. **Runs `cargo run -p mailtx` once per file**, sequentially, from the
   workspace root with the following environment variables set for the child
   process:

   | Variable | Source |
   |---|---|
   | `MAILTX_CONFIG` | `config.toml` argument (resolved to absolute path) |
   | `XAI_API_KEY` | `xai-api-key` argument |

5. **Displays output in a split terminal UI** with two independently scrollable
   panes:

   - **Top — HTTP Requests:** each POST body received by the mock server,
     pretty-printed as JSON.
   - **Bottom — Process Output:** for each file, a numbered header
     (`[1/3] filename.eml`), the resolved email metadata, all stdout and stderr
     lines from mailtx, and its exit code. A separator line is printed between
     files. Each file is processed only after the previous one exits.

   Both panes auto-scroll to the bottom as new lines arrive.

## Key Bindings

| Key | Action |
|---|---|
| `Tab` | Switch active pane |
| `↑` / `↓` | Scroll one line |
| `PgUp` / `PgDn` | Scroll one page |
| `q` | Quit |

## Adding Dependencies

The project uses `uv`. To add a dependency:

```bash
cd mailtx/tools/tester
uv add <package>
```

This creates `uv.lock` and a local `.venv` automatically.
