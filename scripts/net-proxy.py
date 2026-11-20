#!/usr/bin/env python3
"""Rocky OS net proxy: serial bridge for the scratch-built browser.

Rocky OS has NO network stack (no Ethernet driver, no TCP/IP, no
DNS, no TLS in-kernel). Fetching works like early dial-up, over a
second serial port (COM2):

  QEMU -serial unix:/tmp/rocky-net,server,nowait   (COM2)
  this proxy connects to that socket as a client

Protocol (line-based, UTF-8):
  OS -> proxy:  GET <url>\n
  proxy -> OS:  OK <len>\n  + exactly <len> bytes of article text
                + \nEND\n
  or:           ERR <message>\n

The proxy fetches the URL over real HTTPS with urllib, extracts
the main article text with stdlib html.parser (title, headings,
paragraphs, list items; script/style/nav/header/footer dropped),
collapses whitespace and caps the text at 16_000 characters.

Every request is logged to /tmp/rocky-net.log. The proxy retries
connecting to the QEMU socket until QEMU is up, survives a
disconnect by reconnecting, and handles a second GET on the same
connection.

Usage: python3 scripts/net-proxy.py
"""
import html.parser
import logging
import os
import socket
import sys
import time
import urllib.error
import urllib.request

SOCK_PATH = "/tmp/rocky-net"
LOG_PATH = "/tmp/rocky-net.log"
MAX_CHARS = 16_000

logging.basicConfig(
    filename=LOG_PATH,
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
)
log = logging.getLogger("net-proxy")

SKIP_TAGS = {
    "script", "style", "nav", "header", "footer", "aside",
    "noscript", "svg", "form", "button", "input", "select",
    "textarea", "iframe", "table",
}
BLOCK_TAGS = {"h1", "h2", "h3", "p", "li", "title"}


class ArticleParser(html.parser.HTMLParser):
    """Extract title / headings / paragraphs / list items as text blocks."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.blocks = []          # list of (kind, text)
        self._kind = None         # current block kind, or None
        self._buf = []
        self._skip_depth = 0
        self._title = ""

    def _flush(self):
        if self._kind is None:
            return
        text = " ".join("".join(self._buf).split())
        if text:
            if self._kind == "title":
                self._title = text
            else:
                self.blocks.append((self._kind, text))
        self._kind = None
        self._buf = []

    def handle_starttag(self, tag, attrs):
        if tag in SKIP_TAGS:
            # <input> is a void element: no end tag ever arrives,
            # so counting it would leave the parser skipping
            # everything after the first search box.
            if tag != "input":
                self._skip_depth += 1
            return
        if self._skip_depth:
            return
        if tag in BLOCK_TAGS:
            self._flush()
            self._kind = tag
            self._buf = []
        elif tag == "br":
            self._buf.append(" ")

    def handle_endtag(self, tag):
        if tag in SKIP_TAGS:
            if self._skip_depth:
                self._skip_depth -= 1
            return
        if self._skip_depth:
            return
        if tag in BLOCK_TAGS:
            self._flush()

    def handle_data(self, data):
        if self._skip_depth or self._kind is None:
            return
        self._buf.append(data)


def fetch_article(url):
    """Fetch url over HTTPS and return extracted article text."""
    if not (url.startswith("http://") or url.startswith("https://")):
        raise ValueError("only http(s) URLs are supported")
    req = urllib.request.Request(
        url, headers={"User-Agent": "RockyOS/0.1 (serial proxy)"}
    )
    with urllib.request.urlopen(req, timeout=25) as resp:
        raw = resp.read(2_000_000)
    charset = resp.headers.get_content_charset() or "utf-8"
    html = raw.decode(charset, errors="replace")
    parser = ArticleParser()
    parser.feed(html)
    parser.close()
    parser._flush()

    title = parser._title
    if title.endswith(" - Wikipedia"):
        title = title[: -len(" - Wikipedia")]
    lines = []
    if title:
        lines.append("TITLE:" + title)
    for kind, text in parser.blocks:
        if kind == "title":
            continue
        lines.append(kind.upper() + ":" + text)
    text = "\n".join(lines)
    return text[:MAX_CHARS]


def handle_conn(conn):
    """Serve GET requests on one connected socket until it closes."""
    f = conn.makefile("rwb", buffering=0)
    while True:
        line = f.readline()
        if not line:
            return
        try:
            request = line.decode("utf-8", errors="replace").strip()
        except Exception:
            return
        if not request:
            continue
        log.info("request: %r", request)
        if not request.startswith("GET "):
            f.write(b"ERR expected GET <url>\n")
            continue
        url = request[4:].strip()
        try:
            text = fetch_article(url)
            payload = text.encode("utf-8")
            f.write(b"OK %d\n" % len(payload))
            f.write(payload)
            f.write(b"\nEND\n")
            log.info("fetched %s -> %d bytes", url, len(payload))
        except Exception as e:  # noqa: BLE001 - report any fetch failure
            msg = str(e).replace("\n", " ")[:200]
            try:
                f.write(("ERR %s\n" % msg).encode("utf-8", errors="replace"))
            except OSError:
                return
            log.info("fetch failed %s: %s", url, msg)


def connect():
    """Connect to the QEMU COM2 unix socket, retrying until it exists."""
    while True:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(SOCK_PATH)
            return s
        except OSError as e:
            log.info("waiting for %s: %s", SOCK_PATH, e)
            time.sleep(0.5)


def main():
    log.info("net-proxy starting, socket %s", SOCK_PATH)
    print("net-proxy: waiting for QEMU COM2 socket /tmp/rocky-net ...",
          flush=True)
    while True:
        try:
            conn = connect()
            print("net-proxy: connected to QEMU", flush=True)
            log.info("connected")
            handle_conn(conn)
            log.info("connection closed, reconnecting")
            print("net-proxy: connection closed, reconnecting ...",
                  flush=True)
        except KeyboardInterrupt:
            return
        except OSError as e:
            log.info("connection error: %s", e)
            time.sleep(0.5)
        finally:
            try:
                conn.close()
            except Exception:
                pass


if __name__ == "__main__":
    main()
