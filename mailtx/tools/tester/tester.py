#!/usr/bin/env python3
"""
Manual testing harness for mailtx.

Usage:
    python tester.py <config.toml> <xai-api-key> <email.eml> [<email2.eml> ...]

Starts a mock Firefly HTTP server on port 8080, runs `cargo run -p mailtx`
with the given config and email, and displays the results in a split TUI:
  top pane  — HTTP request bodies received by the mock server
  bottom pane — stdout/stderr from the mailtx process

Key bindings:
  Tab       switch active pane
  ↑ / ↓    scroll one line
  PgUp/PgDn scroll one page
  q         quit
"""

import argparse
import curses
import email as email_module
import json
import os
import queue
import random
import re
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

# tester.py lives inside mailtx/tools/tester/; workspace root is three levels up
_WORKSPACE_ROOT = Path(__file__).resolve().parents[3]
_SEP = "─" * 60

# ── ANSI colour rendering ────────────────────────────────────────────────────

_ANSI_RE = re.compile(r"\x1b\[([0-9;]*)m")

# Standard ANSI foreground colour index → curses colour constant.
_ANSI_COLORS = [
    curses.COLOR_BLACK,
    curses.COLOR_RED,
    curses.COLOR_GREEN,
    curses.COLOR_YELLOW,
    curses.COLOR_BLUE,
    curses.COLOR_MAGENTA,
    curses.COLOR_CYAN,
    curses.COLOR_WHITE,
]
_ANSI_PAIR_BASE = 4  # curses colour pairs 4-11 are reserved for ANSI colours


def _parse_ansi(line: str) -> list[tuple[str, int]]:
    """Split *line* into (text, curses_attr) segments by interpreting ANSI SGR codes."""
    segments: list[tuple[str, int]] = []
    pos = 0
    fg = -1  # -1 = terminal default colour
    bold = False

    def attr() -> int:
        a = curses.A_BOLD if bold else curses.A_NORMAL
        if fg != -1:
            a |= curses.color_pair(_ANSI_PAIR_BASE + fg)
        return a

    for m in _ANSI_RE.finditer(line):
        if m.start() > pos:
            segments.append((line[pos : m.start()], attr()))
        for part in m.group(1).split(";"):
            n = int(part) if part else 0
            if n == 0:
                fg, bold = -1, False
            elif n == 1:
                bold = True
            elif 30 <= n <= 37:
                fg = n - 30
            elif n == 39:
                fg = -1
            elif 90 <= n <= 97:  # bright = same colour + bold
                fg = n - 90
                bold = True
        pos = m.end()

    if pos < len(line):
        segments.append((line[pos:], attr()))

    return segments or [(line, curses.A_NORMAL)]


# ── shared queues ────────────────────────────────────────────────────────────

http_queue: queue.Queue[str] = queue.Queue()
proc_queue: queue.Queue[str] = queue.Queue()


# ── mock HTTP server ─────────────────────────────────────────────────────────


class MockFireflyHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        try:
            pretty = json.dumps(json.loads(body), indent=2)
        except Exception:
            pretty = body.decode("utf-8", errors="replace")

        http_queue.put("── Incoming POST ──────────────────────────────────────")
        for line in pretty.splitlines():
            http_queue.put(line)
        http_queue.put("")

        tx_id = str(random.randint(1, 999_999))
        resp = json.dumps({"data": {"id": tx_id}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(resp)))
        self.end_headers()
        self.wfile.write(resp)

    def log_message(self, fmt: str, *_):
        pass  # suppress default access log


def start_server() -> HTTPServer:
    server = HTTPServer(("localhost", 8080), MockFireflyHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


# ── email parsing ────────────────────────────────────────────────────────────


def extract_email_info(path: str) -> tuple[str, str]:
    with open(path, "rb") as f:
        msg = email_module.message_from_binary_file(f)
    return msg.get("Subject", ""), msg.get("From", "")


# ── mailtx runner ────────────────────────────────────────────────────────────


def run_mailtx(config_path: str, api_key: str, eml_path: str) -> None:
    abs_eml = os.path.abspath(eml_path)
    subject, sender = extract_email_info(eml_path)
    event_id = random.randint(1, 1_000_000)
    stdin_payload = json.dumps(
        {
            "event": {"id": event_id},
            "email": {
                "subject": subject,
                "sender": sender,
                "raw_message_path": abs_eml,
            },
        }
    ).encode("utf-8")

    proc_queue.put(f"Subject  : {subject}")
    proc_queue.put(f"Sender   : {sender}")
    proc_queue.put(f"Event ID : {event_id}")
    proc_queue.put(f"EML path : {abs_eml}")
    proc_queue.put("")

    env = os.environ.copy()
    env["MAILTX_CONFIG"] = os.path.abspath(config_path)
    env["XAI_API_KEY"] = api_key

    try:
        proc = subprocess.Popen(
            ["cargo", "run", "-p", "mailtx"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=_WORKSPACE_ROOT,
        )
    except Exception as e:
        proc_queue.put(f"Failed to start mailtx: {e}")
        proc_queue.put("")
        return

    # Safely write to stdin if available
    try:
        if proc.stdin is not None:
            proc.stdin.write(stdin_payload)
            try:
                proc.stdin.flush()
            except Exception:
                pass
            try:
                proc.stdin.close()
            except Exception:
                pass
    except Exception as e:
        proc_queue.put(f"Error writing to mailtx stdin: {e}")

    def drain(stream) -> None:
        if stream is None:
            return
        try:
            for raw in iter(stream.readline, b""):
                try:
                    proc_queue.put(raw.decode("utf-8", errors="replace").rstrip("\n"))
                except Exception:
                    # Fallback: push repr if decoding fails unexpectedly
                    proc_queue.put(repr(raw))
        except Exception as e:
            proc_queue.put(f"Error reading process stream: {e}")
        finally:
            try:
                stream.close()
            except Exception:
                pass

    threading.Thread(target=drain, args=(proc.stdout,), daemon=True).start()
    threading.Thread(target=drain, args=(proc.stderr,), daemon=True).start()

    try:
        proc.wait()
    except Exception as e:
        proc_queue.put(f"Error waiting for mailtx to exit: {e}")

    proc_queue.put("")
    proc_queue.put(
        f"── mailtx exited (code {getattr(proc, 'returncode', 'unknown')}) ──────────────────────────"
    )


def run_all_mailtx(config_path: str, api_key: str, eml_paths: list[str]) -> None:
    for i, eml_path in enumerate(eml_paths):
        if i > 0:
            proc_queue.put("")
            proc_queue.put(_SEP)
            proc_queue.put("")
        proc_queue.put(f"[{i + 1}/{len(eml_paths)}] {os.path.basename(eml_path)}")
        proc_queue.put("")
        run_mailtx(config_path, api_key, eml_path)


# ── TUI helpers ──────────────────────────────────────────────────────────────


def _drain_queue(q: queue.Queue, lines: list) -> bool:
    """Drain all available items from *q* into *lines*. Returns True if any were added."""
    got = False
    while True:
        try:
            lines.append(q.get_nowait())
            got = True
        except queue.Empty:
            return got


# ── TUI ──────────────────────────────────────────────────────────────────────

PAD_LINES = 8192  # max lines buffered per pane
HINT = " Tab: switch pane  ↑/↓/PgUp/PgDn: scroll  q: quit "


def run_tui(stdscr, config_path: str, api_key: str, eml_paths: list[str]) -> None:
    curses.curs_set(0)
    curses.start_color()
    curses.use_default_colors()
    curses.init_pair(1, curses.COLOR_WHITE, -1)  # inactive pane title
    curses.init_pair(2, curses.COLOR_CYAN, -1)  # active pane title
    curses.init_pair(3, curses.COLOR_YELLOW, -1)  # hint bar
    for _i, _c in enumerate(_ANSI_COLORS):  # ANSI foreground colours
        curses.init_pair(_ANSI_PAIR_BASE + _i, _c, -1)

    stdscr.nodelay(True)
    stdscr.keypad(True)

    H, W = stdscr.getmaxyx()
    avail = H - 1  # bottom row reserved for hint bar
    top_h = avail // 2  # height (including borders) of top frame
    bot_h = avail - top_h  # height (including borders) of bottom frame
    inner_w = max(W - 2, 1)
    inner_h_top = max(top_h - 2, 1)  # usable rows inside top border
    inner_h_bot = max(bot_h - 2, 1)  # usable rows inside bottom border

    # Bordered frame windows
    top_frame = curses.newwin(top_h, W, 0, 0)
    bot_frame = curses.newwin(bot_h, W, top_h, 0)

    # Pads hold all buffered content; we scroll a viewport over them
    http_pad = curses.newpad(PAD_LINES, inner_w)
    proc_pad = curses.newpad(PAD_LINES, inner_w)

    # Absolute screen coordinates of each content viewport
    # (min_row, min_col, max_row, max_col) — all inclusive
    http_vp = (1, 1, top_h - 2, W - 2)
    proc_vp = (top_h + 1, 1, top_h + bot_h - 2, W - 2)

    http_lines: list[str] = []
    proc_lines: list[str] = []
    scrolls = [0, 0]  # [http_scroll, proc_scroll] indexed by active pane
    page_sizes = [inner_h_top, inner_h_bot]
    active = 1  # 0 = top (HTTP), 1 = bottom (process) — start on process

    def draw_frame(win, title: str, is_active: bool) -> None:
        win.erase()
        win.box()
        attr = (
            (curses.color_pair(2) | curses.A_BOLD)
            if is_active
            else curses.color_pair(1)
        )
        try:
            win.addstr(0, 2, f" {title} ", attr)
        except curses.error:
            pass

    def render_pad(
        pad, lines: list[str], scroll: int, vp: tuple[int, int, int, int]
    ) -> int:
        min_row, min_col, max_row, max_col = vp
        visible = max_row - min_row + 1
        scroll = max(0, min(scroll, max(0, len(lines) - visible)))
        pad.erase()
        col_limit = max_col - min_col + 1
        for i, line in enumerate(lines):
            x = 0
            for text, text_attr in _parse_ansi(line):
                if x >= col_limit:
                    break
                try:
                    pad.addstr(i, x, text[: col_limit - x], text_attr)
                except curses.error:
                    pass
                x += len(text)
        try:
            pad.noutrefresh(scroll, 0, min_row, min_col, max_row, max_col)
        except curses.error:
            pass
        return scroll

    def draw_hint() -> None:
        try:
            stdscr.addstr(H - 1, 0, HINT[: W - 1], curses.color_pair(3) | curses.A_BOLD)
        except curses.error:
            pass

    # Start mailtx in the background immediately
    threading.Thread(
        target=run_all_mailtx, args=(config_path, api_key, eml_paths), daemon=True
    ).start()

    while True:
        # Drain queues; auto-scroll to bottom when new lines arrive
        if _drain_queue(http_queue, http_lines):
            scrolls[0] = max(0, len(http_lines) - inner_h_top)
        if _drain_queue(proc_queue, proc_lines):
            scrolls[1] = max(0, len(proc_lines) - inner_h_bot)

        # Handle keyboard input
        key = stdscr.getch()
        if key in (ord("q"), ord("Q")):
            break
        elif key == ord("\t"):
            active = 1 - active
        elif key == curses.KEY_UP:
            scrolls[active] = max(0, scrolls[active] - 1)
        elif key == curses.KEY_DOWN:
            scrolls[active] += 1
        elif key == curses.KEY_PPAGE:
            scrolls[active] = max(0, scrolls[active] - page_sizes[active])
        elif key == curses.KEY_NPAGE:
            scrolls[active] += page_sizes[active]

        # Redraw
        draw_frame(top_frame, "HTTP Requests", active == 0)
        draw_frame(bot_frame, "Process Output", active == 1)
        top_frame.noutrefresh()
        bot_frame.noutrefresh()

        scrolls[0] = render_pad(http_pad, http_lines, scrolls[0], http_vp)
        scrolls[1] = render_pad(proc_pad, proc_lines, scrolls[1], proc_vp)

        draw_hint()
        stdscr.noutrefresh()
        curses.doupdate()

        curses.napms(50)  # ~20 fps


# ── entry point ───────────────────────────────────────────────────────────────


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Manual test harness for mailtx",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("config", help="Path to config.toml")
    parser.add_argument("api_key", help="X AI API key (XAI_API_KEY)")
    parser.add_argument("eml", nargs="+", metavar="eml", help="Path(s) to .eml file(s)")
    args = parser.parse_args()

    start_server()
    curses.wrapper(run_tui, args.config, args.api_key, args.eml)


if __name__ == "__main__":
    main()
