#!/usr/bin/env python3
"""Probe the live event stream of a `tole serve` through a reverse proxy (#1044).

It checks the two things the proxy must not break:

  1. Streaming: `text_delta` frames reach the client while the turn runs, not in
     one batch at its end. The script prints when the prompt was sent, when the
     first `text_delta` frame arrived and how the frames are spread over the turn.
  2. Resume: after the stream is cut mid-turn, a new stream opened with
     `?after=<last seq seen>` delivers every durable `entry` frame that was
     missed, once, in order (compared with `GET /sessions/{id}/entries`).

It needs a serve whose provider answers (a real one, or a mock that streams),
and it sends ONE prompt per phase (two in all), which spends tokens on a real
provider. It uses only the Python 3 standard library and never prints the token.

    stream_probe.py https://tole.example.com --token-file secrets/tole_serve_token
    stream_probe.py http://127.0.0.1:7801 --token-file t --prompt "Count from 1 to 40, one per line."

To measure the proxy's own delay, compare `first text_delta` with the moment the
provider emitted its first token (a mock provider can log it), and compare with
a run straight against serve (the same script, the serve's own URL).

A stream that stays silent longer than --stream-timeout (default 120 s) is read as
broken: raise it when TOLE_SERVE_HEARTBEAT_SECS is set above that.

Exit status: 0 both checks passed, 1 a check failed, 2 the probe could not run or
could not reach a verdict (for example the turn ended before the stream was cut).
"""
import argparse
import http.client
import json
import ssl
import statistics
import sys
import threading
import time
import urllib.parse

DEFAULT_PROMPT = "Count from 1 to 40, one number per line, no other text."


def connect(base, cafile, insecure, timeout):
    u = urllib.parse.urlparse(base)
    if u.scheme == "https":
        ctx = ssl.create_default_context(cafile=cafile)
        if insecure:
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
        return http.client.HTTPSConnection(u.hostname, u.port or 443, timeout=timeout, context=ctx)
    return http.client.HTTPConnection(u.hostname, u.port or 80, timeout=timeout)


class Api:
    def __init__(self, base, token, cafile, insecure, stream_timeout=120):
        self.base, self.token, self.cafile, self.insecure = base, token, cafile, insecure
        self.stream_timeout = stream_timeout

    def request(self, method, path, body=None, timeout=1800, auth=True):
        conn = connect(self.base, self.cafile, self.insecure, timeout)
        headers = {"Accept": "application/json"}
        if auth:
            headers["Authorization"] = "Bearer " + self.token
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        conn.request(method, path, body=data, headers=headers)
        resp = conn.getresponse()
        raw = resp.read()
        conn.close()
        try:
            return resp.status, json.loads(raw or b"null")
        except ValueError:
            return resp.status, raw.decode("utf-8", "replace")

    def open_stream(self, path):
        conn = connect(self.base, self.cafile, self.insecure, self.stream_timeout)
        conn.request("GET", path, headers={
            "Authorization": "Bearer " + self.token,
            "Accept": "text/event-stream",
            "Cache-Control": "no-cache",
        })
        resp = conn.getresponse()
        if resp.status != 200:
            raise RuntimeError("event stream answered %d" % resp.status)
        return conn, resp


class Stream(threading.Thread):
    """Reads one SSE stream and records (monotonic time, event, id, data)."""

    def __init__(self, api, path):
        super().__init__(daemon=True)
        self.api, self.path = api, path
        self.frames = []
        self.ready = threading.Event()  # set on the ready frame OR when the stream ends
        self.got_ready = False
        self.error = None
        self.conn = None

    def run(self):
        try:
            self.conn, resp = self.api.open_stream(self.path)
            event, fid, data = "message", None, []
            while True:
                line = resp.readline()
                if not line:
                    return
                line = line.decode("utf-8", "replace").rstrip("\r\n")
                if line == "":
                    if data or fid is not None or event != "message":
                        self.frames.append((time.monotonic(), event, fid, "\n".join(data)))
                        if event == "ready":
                            self.got_ready = True
                            self.ready.set()
                    event, fid, data = "message", None, []
                elif line.startswith(":"):
                    continue  # heartbeat comment
                elif line.startswith("event:"):
                    event = line[6:].strip()
                elif line.startswith("id:"):
                    fid = line[3:].strip()
                elif line.startswith("data:"):
                    data.append(line[5:].lstrip(" "))
        except Exception as e:  # a cut stream ends the thread; that is expected
            self.error = e
        finally:
            self.ready.set()

    def wait_ready(self, seconds=30):
        """True only when the ready frame arrived (not on a stream that ended or failed)."""
        return self.ready.wait(seconds) and self.got_ready and not self.error

    def cut(self):
        """Close the socket without a goodbye: a dropped connection."""
        try:
            if self.conn is not None and self.conn.sock is not None:
                self.conn.sock.shutdown(2)
                self.conn.sock.close()
        except OSError:
            pass


def entry_seqs(frames):
    return [int(f[2]) for f in frames if f[1] == "entry" and f[2] is not None]


def run_turn(api, sid, prompt, results):
    t0 = time.monotonic()
    try:
        status, body = api.request("POST", "/sessions/%s/prompt" % sid, {"text": prompt})
    except Exception as e:  # a thread must not die silently: record the failure
        status, body = None, repr(e)
    results.update(status=status, body=body, start=t0, end=time.monotonic())


def streaming_phase(api, sid, prompt):
    stream = Stream(api, "/sessions/%s/events" % sid)
    stream.start()
    if not stream.wait_ready():
        return None, "could not open the event stream (no ready frame): %s" % stream.error
    res = {}
    t0 = time.monotonic()
    worker = threading.Thread(target=run_turn, args=(api, sid, prompt, res), daemon=True)
    worker.start()
    worker.join(1800)
    time.sleep(0.5)
    stream.cut()
    deltas = [f[0] for f in stream.frames if f[1] == "text_delta"]
    return dict(t0=t0, res=res, deltas=deltas, frames=stream.frames), None


def report_streaming(r):
    res, deltas, t0 = r["res"], r["deltas"], r["t0"]
    if "end" not in res:
        print("streaming: FAIL the prompt request did not finish within the probe's wait")
        return False
    turn = res["end"] - res["start"]
    print("streaming: prompt answered %s after %.2f s" % (res["status"], turn))
    if res["status"] != 200:
        print("  FAIL the prompt did not succeed: %s" % (res["body"],))
        return False
    if not deltas:
        print("  FAIL no text_delta frame arrived (is the provider streaming? TOLE_STREAM=0 turns it off)")
        return False
    first = deltas[0] - t0
    span = deltas[-1] - deltas[0]
    gaps = [b - a for a, b in zip(deltas, deltas[1:])]
    print("  first text_delta frame: %.0f ms after the prompt was sent" % (first * 1000))
    print("  text_delta frames: %d over %.0f ms" % (len(deltas), span * 1000))
    if gaps:
        print("  gap between frames: median %.0f ms, max %.0f ms" % (
            statistics.median(gaps) * 1000, max(gaps) * 1000))
    # A buffering proxy delivers the frames in one burst when the turn ends:
    # almost all of them land within a small window right before the answer.
    producing = turn - first
    if len(deltas) >= 5 and producing > 1.0 and span < 0.2 * producing:
        print("  FAIL the %d frames arrived within %.0f ms of each other while the turn ran %.1f s "
              "after the first token: the stream looks buffered" % (len(deltas), span * 1000, producing))
        return False
    if len(deltas) < 5:
        print("  note: fewer than 5 text_delta frames, too few to judge batching; use a longer --prompt")
    print("  ok")
    return True


def resume_phase(api, sid, prompt):
    """True passed, False failed, None inconclusive (the cut did not land mid-turn)."""
    a = Stream(api, "/sessions/%s/events" % sid)
    a.start()
    if not a.wait_ready():
        print("resume: FAIL could not open the first stream (no ready frame): %s" % a.error)
        return False
    res = {}
    worker = threading.Thread(target=run_turn, args=(api, sid, prompt, res), daemon=True)
    worker.start()
    # Wait (up to 30 s) for the first sign that the turn is under way, then cut.
    # When the turn emits no ephemeral frame within that time, cut anyway.
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and worker.is_alive() and not any(
            f[1] in ("text_delta", "tool_started") for f in a.frames):
        time.sleep(0.02)
    mid_turn = worker.is_alive()
    a.cut()
    seen = entry_seqs(a.frames)
    last = max(seen) if seen else 0
    print("resume: stream cut after %d entr%s (last seq %d), turn %s" % (
        len(seen), "y" if len(seen) == 1 else "ies", last, "still running" if mid_turn else "already over"))
    worker.join(1800)
    if res.get("status") != 200:
        print("  FAIL the second prompt did not succeed: %s %s" % (res.get("status"), res.get("body")))
        return False
    if not mid_turn:
        print("  INCONCLUSIVE the turn ended before the stream was cut; use a longer --prompt")
        return None
    time.sleep(0.5)
    b = Stream(api, "/sessions/%s/events?after=%d" % (sid, last))
    b.start()
    if not b.wait_ready():
        print("  FAIL could not reopen the stream with after=%d (no ready frame): %s" % (last, b.error))
        return False
    time.sleep(1.0)
    b.cut()
    got = entry_seqs(b.frames)
    status, body = api.request("GET", "/sessions/%s/entries?after=%d&limit=500" % (sid, last))
    if status != 200:
        print("  FAIL GET /entries answered %s" % status)
        return False
    want = [e["seq"] for e in body["entries"]]
    if not want:
        print("  INCONCLUSIVE the log has no entry after seq %d, so nothing was resumed" % last)
        return None
    if body.get("hasMore"):
        print("  note: more than 500 entries after the cut; comparing the first page only")
        got = got[:len(want)]
    ok = got == want and len(got) == len(set(got)) and all(s > last for s in got)
    print("  reopened with after=%d: %d entries on the stream, %d in the log after that seq" % (
        last, len(got), len(want)))
    print("  ok" if ok else "  FAIL the stream after the cut is not exactly the log after seq %d" % last)
    return ok


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("base_url", help="https://host or http://host:port of the serve (or its proxy)")
    ap.add_argument("--token-file", required=True, help="file holding the serve token")
    ap.add_argument("--cwd", default="/workspace", help="session folder (inside serve's --workspace)")
    ap.add_argument("--prompt", default=DEFAULT_PROMPT)
    ap.add_argument("--cafile", help="PEM file to trust (a private CA, Caddy's local root)")
    ap.add_argument("--stream-timeout", type=float, default=120,
                    help="seconds of silence after which a stream counts as broken (default 120)")
    ap.add_argument("--insecure", action="store_true", help="do not verify the certificate (local tests only)")
    args = ap.parse_args()
    with open(args.token_file, encoding="utf-8") as f:
        token = f.read().strip()
    api = Api(args.base_url.rstrip("/"), token, args.cafile, args.insecure, args.stream_timeout)
    try:
        status, health = api.request("GET", "/health", auth=False, timeout=15)
        if status != 200:
            print("health: %s %s" % (status, health))
            return 2
        print("health: %s" % json.dumps(health))
        status, body = api.request("POST", "/sessions", {"cwd": args.cwd}, timeout=60)
        if status not in (200, 201) or not isinstance(body, dict) or "sessionId" not in body:
            print("could not start a session: %s %s" % (status, body))
            return 2
        sid = body["sessionId"]
        print("session: %s" % sid)
        r, err = streaming_phase(api, sid, args.prompt)
        if err:
            print(err)
            return 2
        ok1 = report_streaming(r)
        ok2 = resume_phase(api, sid, args.prompt)
    except (OSError, http.client.HTTPException, RuntimeError) as e:
        print("probe error: %s" % e)
        return 2
    if not ok1 or ok2 is False:
        return 1
    return 0 if ok2 else 2


if __name__ == "__main__":
    sys.exit(main())
