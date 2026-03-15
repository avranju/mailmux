# Transfer Detection Design

Bank transfers between two asset accounts that are both configured in mailtx
produce two separate emails — one debit alert from the source bank and one
credit alert from the destination bank, typically arriving minutes to an hour
apart. Without special handling, each email becomes an independent
withdrawal/deposit transaction in Firefly III. This document describes the
design for coalescing them into a single Firefly `transfer` transaction.

## Problem

The pipeline is stateless: each invocation processes exactly one email and
immediately POSTs to Firefly. A Firefly `transfer` requires both `source_id`
and `destination_id`, which are only available once both emails have been
processed. The two emails arrive at different times and are processed in
separate invocations.

## Approach

1. **Deterministic transfer detection** — config-driven keyword matching on the
   LLM-extracted description identifies whether an email is a leg of a known
   transfer route, and which account it is going to/coming from.
2. **Pending transfer store** — a local SQLite database holds the first leg
   until the second arrives.
3. **Leg matching** — when the second leg is processed and matches a pending
   entry, a Firefly `transfer` is created and the pending entry is deleted.
4. **Expiry fallback** — if no match arrives within the configured window, the
   pending leg is flushed as a regular withdrawal/deposit with a warning tag so
   nothing is silently lost.

## Config

### Transfer rules

Each `[[transfer_rules]]` entry is **directional**: it describes one specific
route from a source account to a destination account and the keywords that
identify each leg.

```toml
# Path to the SQLite database used to hold pending transfer legs.
# Required when any transfer_rules are defined.
state_db = "/var/lib/mailtx/state.db"

# How long (in hours) to wait for the counterpart leg before giving up.
# Default: 48
transfer_match_window_hours = 48

[[transfer_rules]]
# `source_account` and `destination_account` must match the `id` field of a
# configured [[firefly.asset_accounts]] entry.
source_account      = "hdfc_9772"
destination_account = "sbm"

# All strings in withdrawal_keywords must appear (case-insensitive substring)
# in the LLM-extracted description of the withdrawal email.
withdrawal_keywords = ["MY SBM ACCOUNT"]

# All strings in deposit_keywords must appear (case-insensitive substring)
# in the LLM-extracted description of the deposit email.
deposit_keywords    = ["NEFT", "HDFC"]

[[transfer_rules]]
source_account      = "hdfc_9772"
destination_account = "hdfc_9558"
withdrawal_keywords = ["00123456789558"]
deposit_keywords    = ["50123456789772"]

[[transfer_rules]]
source_account      = "hdfc_9558"
destination_account = "hdfc_9772"
withdrawal_keywords = ["50123456789772"]
deposit_keywords    = ["00123456789558"]
```

Multiple keywords in a list are AND-matched. Use more keywords where a single
keyword might match unrelated transactions (e.g. the deposit side of the
HDFC→SBM rule uses both `"NEFT"` and `"HDFC"` to reduce false positives).

## Detection Algorithm

After the LLM extracts the transaction (type, description, amount, date) and
the account matcher resolves the account ID, check for a matching transfer rule:

**For a withdrawal from account X:**
- Find all rules where `source_account == X`.
- For each candidate rule, check that every string in `withdrawal_keywords`
  appears in the description (case-insensitive substring).
- The first matching rule identifies this email as a transfer leg going to
  `destination_account`.

**For a deposit to account Y:**
- Find all rules where `destination_account == Y`.
- For each candidate rule, check that every string in `deposit_keywords`
  appears in the description.
- The first matching rule identifies this email as a transfer leg coming from
  `source_account`.

If no rule matches, the transaction is processed as a regular
withdrawal/deposit (existing behaviour, unchanged).

## Pending Store and Leg Matching

Once a leg is identified as belonging to a transfer rule:

1. Look up the pending store for a counterpart entry with:
   - Same `rule_id`
   - Same `amount` (exact)
   - Opposite `leg` type (withdrawal ↔ deposit)
   - `inserted_at` within `transfer_match_window_hours`

2. **Match found** — create a Firefly `transfer` transaction using the
   `source_id` and `destination_id` from the rule. Delete the pending entry.

3. **No match** — insert this leg into the pending store with its rule, leg
   type, amount, date, narration, category, and current timestamp. Exit 0
   without creating any Firefly transaction.

If two transfers of the same amount between the same accounts arrive within the
window, match FIFO (oldest pending leg first).

## Expiry and Fallback

On startup, mailtx scans the pending store for entries older than
`transfer_match_window_hours`. Each expired entry is:

1. Submitted to Firefly as a regular withdrawal or deposit (based on its leg
   type), with the tag `unmatched-transfer` appended alongside the normal tag.
2. Deleted from the pending store.

This ensures no transaction is silently dropped if the counterpart email never
arrives (e.g. the second email was filtered, already processed, or the sender
configuration changed).

## Firefly Transfer Payload

A matched transfer is posted as:

```json
{
  "transactions": [{
    "type": "transfer",
    "date": "<date from either leg>",
    "amount": "<amount>",
    "description": "<narration>",
    "source_id": "<firefly_account_id of source_account>",
    "destination_id": "<firefly_account_id of destination_account>",
    "tags": ["mailmux-mailtx"],
    "category_name": "<category if present>"
  }]
}
```

The narration and category are taken from whichever leg arrived second (the
leg that completes the match), on the assumption that the credit email tends to
carry more descriptive narration. This is a simple heuristic and could be made
configurable.

## What Does Not Change

- The LLM schema and prompt are unchanged — no new fields are extracted.
- The account matcher is unchanged — it resolves the account for the current
  email only.
- Regular (non-transfer) transactions follow the existing pipeline without
  touching the pending store.
- The `error_if_duplicate_hash` Firefly setting continues to act as a backstop
  against duplicate posts.
