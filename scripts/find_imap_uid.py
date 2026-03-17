#!/usr/bin/env python3
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""
Find IMAP UIDs for messages received since a given date.
Useful for diagnosing missed mailmux sync events.

Usage:
    IMAP_PASSWORD=xxx uv run scripts/find_imap_uid.py --user you@gmail.com
    IMAP_PASSWORD=xxx uv run scripts/find_imap_uid.py --user you@example.com --host mail.example.com
    IMAP_PASSWORD=xxx uv run scripts/find_imap_uid.py --user you@gmail.com --since 2026-03-17 --uid-from 243960
"""

import argparse
import imaplib
import os
import sys
from datetime import date


def main() -> None:
    parser = argparse.ArgumentParser(description="List IMAP INBOX UIDs by date range")
    parser.add_argument("--user", required=True, help="IMAP username / email address")
    parser.add_argument(
        "--host",
        default="imap.gmail.com",
        help="IMAP server hostname (default: imap.gmail.com)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=993,
        help="IMAP server port (default: 993)",
    )
    parser.add_argument(
        "--no-tls",
        action="store_true",
        help="Use plain IMAP instead of IMAP over TLS",
    )
    parser.add_argument(
        "--mailbox",
        default="INBOX",
        help="Mailbox to search (default: INBOX)",
    )
    parser.add_argument(
        "--since",
        default=date.today().strftime("%Y-%m-%d"),
        help="Only show messages on or after this date, YYYY-MM-DD (default: today)",
    )
    parser.add_argument(
        "--uid-from",
        type=int,
        default=None,
        help="Also filter to UIDs >= this value",
    )
    args = parser.parse_args()

    password = os.environ.get("IMAP_PASSWORD")
    if not password:
        print("Error: IMAP_PASSWORD environment variable not set", file=sys.stderr)
        sys.exit(1)

    # Convert YYYY-MM-DD to DD-Mon-YYYY for IMAP SINCE
    since_date = date.fromisoformat(args.since)
    imap_date = since_date.strftime("%d-%b-%Y")

    print(f"Connecting to {args.host}:{args.port} as {args.user} ...")
    if args.no_tls:
        M = imaplib.IMAP4(args.host, args.port)
    else:
        M = imaplib.IMAP4_SSL(args.host, args.port)
    M.login(args.user, password)
    M.select(args.mailbox, readonly=True)

    typ, data = M.uid("SEARCH", None, "SINCE", imap_date)
    if typ != "OK":
        print(f"SEARCH failed: {data}", file=sys.stderr)
        M.logout()
        sys.exit(1)

    uids = [int(u) for u in data[0].split() if u]
    if args.uid_from is not None:
        uids = [u for u in uids if u >= args.uid_from]

    if not uids:
        print("No messages found.")
        M.logout()
        return

    print(f"Found {len(uids)} message(s). Fetching headers...\n")

    uid_list = ",".join(str(u) for u in uids)
    typ, items = M.uid(
        "FETCH", uid_list, "(UID INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)])"
    )
    if typ != "OK":
        print(f"FETCH failed: {items}", file=sys.stderr)
        M.logout()
        sys.exit(1)

    for item in items:
        if isinstance(item, tuple):
            meta = item[0].decode(errors="replace")
            headers = item[1].decode(errors="replace").strip() if len(item) > 1 else ""
            print(meta)
            if headers:
                print(headers)
            print()

    M.logout()


if __name__ == "__main__":
    main()
