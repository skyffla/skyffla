from __future__ import annotations

import json
import threading
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import unquote


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


@dataclass
class SubmissionView:
    artist_name: str
    persona_name: str
    status: str = "waiting"
    title: str | None = None
    tagline: str | None = None
    concept: str | None = None
    prompt_summary: str | None = None
    image_url: str | None = None
    updated_at: str = field(default_factory=_now_iso)


@dataclass
class RoundView:
    round_id: str
    topic: str
    started_at: str = field(default_factory=_now_iso)
    status: str = "collecting"
    winner_name: str | None = None
    jury_reason: str | None = None
    submissions: dict[str, SubmissionView] = field(default_factory=dict)


class GalleryState:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._artists: dict[str, str] = {}
        self._current_round_id: str | None = None
        self._rounds: list[RoundView] = []

    def set_connected_artists(self, artists: dict[str, str]) -> None:
        with self._lock:
            self._artists = dict(artists)

    def start_round(self, *, round_id: str, topic: str, participants: dict[str, str], personas: dict[str, str]) -> None:
        with self._lock:
            round_view = RoundView(round_id=round_id, topic=topic, status="waiting_for_submissions")
            for member_id, artist_name in participants.items():
                round_view.submissions[member_id] = SubmissionView(
                    artist_name=artist_name,
                    persona_name=personas[member_id],
                )
            self._current_round_id = round_id
            self._rounds.insert(0, round_view)

    def mark_submission_metadata(
        self,
        *,
        round_id: str,
        member_id: str,
        title: str,
        tagline: str,
        concept: str,
        prompt_summary: str,
    ) -> None:
        with self._lock:
            submission = self._find_submission(round_id, member_id)
            if submission is None:
                return
            submission.status = "image_in_flight"
            submission.title = title
            submission.tagline = tagline
            submission.concept = concept
            submission.prompt_summary = prompt_summary
            submission.updated_at = _now_iso()

    def mark_submission_image(self, *, round_id: str, member_id: str, image_url: str) -> None:
        with self._lock:
            submission = self._find_submission(round_id, member_id)
            if submission is None:
                return
            submission.status = "ready"
            submission.image_url = image_url
            submission.updated_at = _now_iso()

    def drop_submission(self, *, round_id: str, member_id: str) -> None:
        with self._lock:
            submission = self._find_submission(round_id, member_id)
            if submission is None:
                return
            submission.status = "left"
            submission.updated_at = _now_iso()

    def finish_round(self, *, round_id: str, winner_name: str, jury_reason: str) -> None:
        with self._lock:
            round_view = self._find_round(round_id)
            if round_view is None:
                return
            round_view.status = "complete"
            round_view.winner_name = winner_name
            round_view.jury_reason = jury_reason

    def snapshot(self) -> dict[str, Any]:
        with self._lock:
            current_round = self._find_round(self._current_round_id) if self._current_round_id else None
            return {
                "generated_at": _now_iso(),
                "connected_artists": sorted(self._artists.values()),
                "current_round_id": self._current_round_id,
                "current_round": asdict(current_round) if current_round is not None else None,
                "rounds": [asdict(round_view) for round_view in self._rounds],
            }

    def _find_round(self, round_id: str | None) -> RoundView | None:
        if round_id is None:
            return None
        for round_view in self._rounds:
            if round_view.round_id == round_id:
                return round_view
        return None

    def _find_submission(self, round_id: str, member_id: str) -> SubmissionView | None:
        round_view = self._find_round(round_id)
        if round_view is None:
            return None
        return round_view.submissions.get(member_id)


class LiveGalleryServer:
    def __init__(self, *, state: GalleryState, output_dir: Path, host: str, port: int) -> None:
        self._state = state
        self._output_dir = output_dir
        self._host = host
        self._port = port
        self._httpd: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    @property
    def url(self) -> str:
        return f"http://{self._host}:{self._port}"

    def start(self) -> None:
        state = self._state
        output_dir = self._output_dir

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802
                if self.path == "/" or self.path == "/index.html":
                    self._send_html(_render_index_html())
                    return
                if self.path == "/state.json":
                    self._send_json(state.snapshot())
                    return
                if self.path.startswith("/files/"):
                    relative = Path(unquote(self.path.removeprefix("/files/")))
                    target = (output_dir / relative).resolve()
                    if output_dir.resolve() not in target.parents and target != output_dir.resolve():
                        self.send_error(HTTPStatus.NOT_FOUND)
                        return
                    if not target.is_file():
                        self.send_error(HTTPStatus.NOT_FOUND)
                        return
                    self._send_file(target)
                    return
                self.send_error(HTTPStatus.NOT_FOUND)

            def log_message(self, format: str, *args: object) -> None:  # noqa: A003
                return

            def _send_html(self, body: str) -> None:
                payload = body.encode("utf-8")
                self.send_response(HTTPStatus.OK)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def _send_json(self, body: dict[str, Any]) -> None:
                payload = json.dumps(body, indent=2).encode("utf-8")
                self.send_response(HTTPStatus.OK)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def _send_file(self, path: Path) -> None:
                payload = path.read_bytes()
                content_type = "image/png" if path.suffix.lower() == ".png" else "application/octet-stream"
                self.send_response(HTTPStatus.OK)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self._httpd = ThreadingHTTPServer((self._host, self._port), Handler)
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)
        self._thread.start()

    def close(self) -> None:
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()
            self._httpd = None
        if self._thread is not None:
            self._thread.join(timeout=5)
            self._thread = None


def _render_index_html() -> str:
    return """<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Skyffla Image Studio</title>
  <style>
    :root {
      --bg: #08131f;
      --bg2: #10273a;
      --ink: #f6f2e8;
      --muted: #9fc0ce;
      --card: rgba(11, 27, 38, 0.86);
      --line: rgba(159, 192, 206, 0.22);
      --accent: #ffd166;
      --accent2: #7ae582;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      font-family: Georgia, "Times New Roman", serif;
      color: var(--ink);
      background:
        radial-gradient(circle at top, rgba(122, 229, 130, 0.14), transparent 28%),
        radial-gradient(circle at 20% 20%, rgba(255, 209, 102, 0.14), transparent 22%),
        linear-gradient(180deg, var(--bg2), var(--bg));
      min-height: 100vh;
    }
    .page {
      max-width: 1440px;
      margin: 0 auto;
      padding: 28px;
    }
    h1 {
      margin: 0 0 8px;
      font-size: clamp(2rem, 4vw, 3.5rem);
      letter-spacing: 0.04em;
      text-transform: uppercase;
    }
    .lede {
      color: var(--muted);
      margin: 0 0 24px;
      font-size: 1.05rem;
    }
    .hero, .card, .verdict {
      background: var(--card);
      border: 1px solid var(--line);
      border-radius: 18px;
      backdrop-filter: blur(10px);
    }
    .hero {
      padding: 20px;
      margin-bottom: 20px;
    }
    .verdict {
      display: none;
      margin-bottom: 20px;
      padding: 18px 22px;
      background:
        linear-gradient(135deg, rgba(255, 209, 102, 0.18), rgba(122, 229, 130, 0.08)),
        var(--card);
      border-color: rgba(255, 209, 102, 0.28);
    }
    .verdict.show {
      display: block;
    }
    .verdict-label {
      color: var(--accent);
      font-size: 0.82rem;
      letter-spacing: 0.12em;
      text-transform: uppercase;
      margin-bottom: 8px;
    }
    .verdict-title {
      display: flex;
      flex-wrap: wrap;
      gap: 10px 14px;
      align-items: baseline;
      margin-bottom: 8px;
    }
    .verdict-title strong {
      font-size: clamp(1.4rem, 3vw, 2.2rem);
      letter-spacing: 0.02em;
    }
    .verdict-reason {
      color: var(--ink);
      font-size: 1.08rem;
      line-height: 1.45;
      max-width: 72ch;
    }
    .meta {
      display: grid;
      gap: 10px;
      grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
    }
    .meta strong {
      display: block;
      color: var(--accent);
      font-size: 0.82rem;
      letter-spacing: 0.08em;
      text-transform: uppercase;
      margin-bottom: 4px;
    }
    .grid {
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 20px;
    }
    .card { padding: 18px; }
    .badge {
      display: inline-block;
      padding: 4px 10px;
      border-radius: 999px;
      background: rgba(255, 209, 102, 0.14);
      color: var(--accent);
      font-size: 0.78rem;
      letter-spacing: 0.08em;
      text-transform: uppercase;
    }
    .winner {
      background: rgba(122, 229, 130, 0.14);
      color: var(--accent2);
    }
    img {
      width: 100%;
      aspect-ratio: 3 / 2;
      object-fit: cover;
      border-radius: 12px;
      background: rgba(255,255,255,0.04);
      margin: 12px 0;
      display: block;
    }
    .empty {
      aspect-ratio: 3 / 2;
      border-radius: 12px;
      border: 1px dashed var(--line);
      display: grid;
      place-items: center;
      color: var(--muted);
      margin: 12px 0;
      background: rgba(255,255,255,0.02);
    }
    .small { color: var(--muted); font-size: 0.95rem; }
    .card h2 {
      margin: 12px 0 10px;
      font-size: 1.4rem;
    }
    @media (max-width: 960px) {
      .grid {
        grid-template-columns: minmax(0, 1fr);
      }
    }
  </style>
</head>
<body>
  <div class="page">
    <h1>Skyffla Image Studio</h1>
    <p class="lede">Live room view from the director. Artists submit metadata first, then PNGs over Skyffla.</p>
    <section class="hero">
      <div class="meta" id="meta"></div>
    </section>
    <section class="verdict" id="verdict"></section>
    <section class="grid" id="cards"></section>
  </div>
  <script>
    function esc(text) {
      return (text ?? "").replace(/[&<>"]/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[ch]));
    }
    function render(state) {
      const current = state.current_round;
      const meta = document.getElementById("meta");
      const verdict = document.getElementById("verdict");
      const cards = document.getElementById("cards");
      if (!current) {
        meta.innerHTML = `
          <div><strong>Status</strong><div>Waiting for the first round</div></div>
          <div><strong>Connected Artists</strong><div>${esc(state.connected_artists.join(", ") || "none yet")}</div></div>
        `;
        verdict.className = "verdict";
        verdict.innerHTML = "";
        cards.innerHTML = "";
        return;
      }
      meta.innerHTML = `
        <div><strong>Round</strong><div>${esc(current.round_id)}</div></div>
        <div><strong>Topic</strong><div>${esc(current.topic)}</div></div>
        <div><strong>Status</strong><div>${esc(current.status)}</div></div>
        <div><strong>Connected Artists</strong><div>${esc(state.connected_artists.join(", "))}</div></div>
        <div><strong>Submissions</strong><div>${Object.keys(current.submissions || {}).length}</div></div>
      `;
      if (current.winner_name || current.jury_reason) {
        verdict.className = "verdict show";
        verdict.innerHTML = `
          <div class="verdict-label">Jury Verdict</div>
          <div class="verdict-title">
            <strong>${esc(current.winner_name || "Winner pending")}</strong>
            <span class="badge winner">selected poster</span>
          </div>
          <div class="verdict-reason">${esc(current.jury_reason || "The jury is still deliberating.")}</div>
        `;
      } else {
        verdict.className = "verdict";
        verdict.innerHTML = "";
      }
      const entries = Object.values(current.submissions || {});
      cards.innerHTML = entries.map((entry) => {
        const image = entry.image_url
          ? `<img src="${esc(entry.image_url)}" alt="${esc(entry.title || entry.artist_name)}">`
          : `<div class="empty">image pending</div>`;
        const badge = current.winner_name === entry.artist_name
          ? `<span class="badge winner">winner</span>`
          : `<span class="badge">${esc(entry.status)}</span>`;
        return `
          <article class="card">
            ${badge}
            <h2>${esc(entry.artist_name)}</h2>
            <div class="small"><strong>Persona:</strong> ${esc(entry.persona_name)}</div>
            <div class="small"><strong>Title:</strong> ${esc(entry.title || "pending")}</div>
            <div class="small"><strong>Tagline:</strong> ${esc(entry.tagline || "pending")}</div>
            ${image}
            <div class="small"><strong>Concept:</strong> ${esc(entry.concept || "waiting for metadata")}</div>
            <div class="small"><strong>Prompt:</strong> ${esc(entry.prompt_summary || "waiting for metadata")}</div>
          </article>
        `;
      }).join("");
    }
    async function tick() {
      try {
        const response = await fetch("/state.json", { cache: "no-store" });
        const state = await response.json();
        render(state);
      } catch (error) {
        console.error(error);
      }
    }
    tick();
    setInterval(tick, 1000);
  </script>
</body>
</html>
"""
