#!/usr/bin/env python3
"""TCP tap between cmux peers and cmux-relay (W0 Task 4 spike).

Forwards LISTEN -> UPSTREAM and records what the relay operator could see:

  <capture>            every byte forwarded, both directions, as sent on the wire
  <capture>.decoded    WebSocket payloads after unmasking client frames (and
                       inflating permessage-deflate, if negotiated), so a grep
                       of 0 is not a false negative caused by WS masking
  <capture>.frames.jsonl  one line per WebSocket message: connection, direction,
                       opcode, size, JSON keys of text (RelayControl) messages,
                       and a hex/printable preview of binary messages

Only the standard library is used. Usage:
  tap_proxy.py --listen 127.0.0.1:8788 --upstream 127.0.0.1:8787 --capture FILE
"""

import argparse
import asyncio
import itertools
import json
import re
import time
import zlib

TICKET_RE = re.compile(r'"(ticket|join_ticket)":"[^"]*"')
AUTH_RE = re.compile(rb"(?im)^(authorization:\s*)(.*)$")


class Sink:
    def __init__(self, path):
        self.raw = open(path, "ab", buffering=0)
        self.decoded = open(path + ".decoded", "ab", buffering=0)
        self.frames = open(path + ".frames.jsonl", "a", buffering=1)

    def record(self, event):
        event.setdefault("t", round(time.time(), 3))
        self.frames.write(json.dumps(event, sort_keys=True) + "\n")


class WsParser:
    """Incremental HTTP-upgrade + RFC 6455 parser for one direction."""

    def __init__(self, sink, conn, direction):
        self.sink, self.conn, self.direction = sink, conn, direction
        self.buf = bytearray()
        self.in_headers = True
        self.deflate = False
        self.partner = None
        self.fragments = bytearray()
        self.frag_opcode = None
        self.frag_compressed = False
        self.inflater = None

    def feed(self, data):
        self.buf += data
        if self.in_headers:
            end = self.buf.find(b"\r\n\r\n")
            if end < 0:
                return
            head = bytes(self.buf[: end + 4])
            del self.buf[: end + 4]
            self.in_headers = False
            lower = head.lower()
            if b"permessage-deflate" in lower and self.direction == "relay->peer":
                self.deflate = True
                if self.partner:
                    self.partner.deflate = True
            self.sink.decoded.write(head)
            redacted = AUTH_RE.sub(rb"\1[REDACTED len=?]", head)
            self.sink.record({"conn": self.conn, "dir": self.direction, "kind": "http-head",
                              "text": redacted.decode("latin-1")})
        while self._frame():
            pass

    def _frame(self):
        b = self.buf
        if len(b) < 2:
            return False
        fin, rsv1, opcode = b[0] & 0x80, b[0] & 0x40, b[0] & 0x0F
        masked, length = b[1] & 0x80, b[1] & 0x7F
        pos = 2
        if length == 126:
            if len(b) < 4:
                return False
            length, pos = int.from_bytes(b[2:4], "big"), 4
        elif length == 127:
            if len(b) < 10:
                return False
            length, pos = int.from_bytes(b[2:10], "big"), 10
        mask = None
        if masked:
            if len(b) < pos + 4:
                return False
            mask, pos = bytes(b[pos:pos + 4]), pos + 4
        if len(b) < pos + length:
            return False
        payload = bytes(b[pos:pos + length])
        del b[: pos + length]
        if mask:
            payload = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
        if opcode >= 0x8:  # control frame
            self._message(opcode, payload, False)
            return True
        if opcode != 0:
            self.frag_opcode, self.frag_compressed = opcode, bool(rsv1)
            self.fragments = bytearray()
        self.fragments += payload
        if fin:
            self._message(self.frag_opcode, bytes(self.fragments), self.frag_compressed)
            self.fragments = bytearray()
        return True

    def _message(self, opcode, payload, compressed):
        if compressed:
            if self.inflater is None:
                self.inflater = zlib.decompressobj(-15)
            payload = self.inflater.decompress(payload + b"\x00\x00\xff\xff")
        self.sink.decoded.write(payload + b"\n")
        names = {1: "text", 2: "binary", 8: "close", 9: "ping", 10: "pong"}
        event = {"conn": self.conn, "dir": self.direction, "kind": names.get(opcode, str(opcode)),
                 "len": len(payload), "compressed": compressed}
        if opcode == 1:
            text = payload.decode("utf-8", "replace")
            try:
                obj = json.loads(text)
                event["json_type"] = obj.get("type")
                event["json_keys"] = sorted(obj.keys())
            except ValueError:
                pass
            event["text"] = TICKET_RE.sub(lambda m: f'"{m.group(1)}":"[REDACTED len={len(m.group(0))}]"', text)
        elif opcode == 2:
            event["hex32"] = payload[:32].hex()
            printable = sum(32 <= c < 127 for c in payload)
            event["printable_ratio"] = round(printable / max(1, len(payload)), 3)
            if payload[:1] == b"{":
                try:
                    obj = json.loads(payload)
                    event["plaintext_json"] = obj
                except ValueError:
                    pass
        self.sink.record(event)


async def pump(reader, writer, sink, parser):
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            sink.raw.write(data)
            try:
                parser.feed(data)
            except Exception as error:  # never break forwarding for a parse error
                sink.record({"conn": parser.conn, "dir": parser.direction, "kind": "parse-error",
                             "error": repr(error)})
            writer.write(data)
            await writer.drain()
    except (ConnectionError, asyncio.IncompleteReadError):
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", default="127.0.0.1:8788")
    ap.add_argument("--upstream", default="127.0.0.1:8787")
    ap.add_argument("--capture", required=True)
    args = ap.parse_args()
    sink = Sink(args.capture)
    lhost, lport = args.listen.rsplit(":", 1)
    uhost, uport = args.upstream.rsplit(":", 1)
    counter = itertools.count(1)

    async def handle(client_reader, client_writer):
        conn = next(counter)
        try:
            up_reader, up_writer = await asyncio.open_connection(uhost, int(uport))
        except OSError as error:
            sink.record({"conn": conn, "kind": "upstream-error", "error": repr(error)})
            client_writer.close()
            return
        sink.record({"conn": conn, "kind": "open", "peer": str(client_writer.get_extra_info("peername"))})
        to_relay = WsParser(sink, conn, "peer->relay")
        to_peer = WsParser(sink, conn, "relay->peer")
        to_peer.partner = to_relay
        await asyncio.gather(pump(client_reader, up_writer, sink, to_relay),
                             pump(up_reader, client_writer, sink, to_peer))
        sink.record({"conn": conn, "kind": "close"})

    server = await asyncio.start_server(handle, lhost, int(lport))
    print(f"tap listening on {args.listen} -> {args.upstream}", flush=True)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
