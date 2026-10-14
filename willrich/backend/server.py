"""Demo backend for the AI lead-capture system.

Run:  python3 server.py [port]        (default port 8080)

Serves:
    /                          the Willrich Homes demo site
    /widget/chat-widget.js      the embeddable chat widget
    POST /api/chat             chat API  -> {"session_id","message"} / {"reply","lead_captured"}
    GET  /dashboard            owner dashboard (captured leads table)
    GET  /api/leads            captured leads as JSON

On every captured lead the server appends to leads.json and logs a fake
"SMS sent to owner" line (swap for Twilio in production).
"""

import json
import os
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from responder import respond, start_conversation

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
SITE_DIR = os.path.join(BASE_DIR, "..", "site")
WIDGET_DIR = os.path.join(BASE_DIR, "..", "widget")
LEADS_FILE = os.path.join(BASE_DIR, "leads.json")
SMS_LOG = os.path.join(BASE_DIR, "sms.log")

# In-memory chat sessions (fine for a demo; use Redis/DB in production).
_sessions = {}
_sessions_lock = threading.Lock()


def load_leads():
    if not os.path.exists(LEADS_FILE):
        return []
    with open(LEADS_FILE) as f:
        try:
            return json.load(f)
        except json.JSONDecodeError:
            return []


def save_lead(lead):
    leads = load_leads()
    leads.append(lead)
    with open(LEADS_FILE, "w") as f:
        json.dump(leads, f, indent=2)


def notify_owner(lead):
    """Fake SMS to the business owner. Replace with Twilio in production."""
    urgent = " [URGENT]" if lead.get("emergency") else ""
    msg = (
        f"[SMS -> owner (Willrich){urgent}] New lead: {lead['name']} "
        f"({lead['phone']}) — {lead['service']} — lot: {lead.get('lot_status','n/a')} — "
        f"budget {lead.get('budget','n/a')} — timeline {lead['preferred_time']}"
    )
    print(msg, flush=True)
    with open(SMS_LOG, "a") as f:
        f.write(msg + "\n")


DASHBOARD_HTML = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Willrich Homes — Lead Dashboard</title>
<style>
  body { font-family: -apple-system, 'Segoe UI', Roboto, sans-serif;
         background: #f1f5f9; margin: 0; padding: 24px; color: #0f172a; }
  h1 { font-size: 22px; margin: 0 0 4px; }
  p.sub { color: #64748b; margin: 0 0 20px; }
  .card { background: #fff; border-radius: 12px; padding: 20px;
          box-shadow: 0 1px 3px rgba(0,0,0,.08); overflow-x: auto; }
  table { width: 100%; border-collapse: collapse; font-size: 14px; }
  th { text-align: left; padding: 10px; background: #f8fafc; color: #475569;
       font-weight: 600; white-space: nowrap; }
  td { padding: 10px; border-top: 1px solid #e2e8f0; vertical-align: top; }
  .pill { display: inline-block; padding: 2px 10px; border-radius: 999px;
          font-size: 12px; font-weight: 600; }
  .urgent { background: #fee2e2; color: #b91c1c; }
  .normal { background: #dcfce7; color: #15803d; }
  .empty { color: #94a3b8; text-align: center; padding: 40px; }
  .badge { display:inline-block; background:#0ea5e9; color:#fff; font-size:12px;
           padding:3px 10px; border-radius:999px; margin-left:8px; }
</style>
</head>
<body>
  <h1>🏡 Lead Dashboard <span class="badge" id="count">0</span></h1>
  <p class="sub">Leads captured by Casey, your AI concierge. Updates live.</p>
  <div class="card"><div id="table">Loading…</div></div>
<script>
async function refresh() {
  const res = await fetch('/api/leads');
  const leads = await res.json();
  document.getElementById('count').textContent = leads.length;
  if (!leads.length) {
    document.getElementById('table').innerHTML =
      '<div class="empty">No leads yet — they\\'ll appear here the moment the chat captures one.</div>';
    return;
  }
  const rows = leads.slice().reverse().map(l => `
    <tr>
      <td>${new Date(l.timestamp).toLocaleString()}</td>
      <td><strong>${esc(l.name)}</strong></td>
      <td>${esc(l.phone)}</td>
      <td>${esc(l.service)}</td>
      <td>${esc(l.lot_status)}</td>
      <td>${esc(l.preferred_time)}</td>
      <td>${esc(l.budget)}</td>
      <td>${l.emergency ? '<span class="pill urgent">URGENT</span>'
                       : '<span class="pill normal">standard</span>'}</td>
    </tr>`).join('');
  document.getElementById('table').innerHTML = `
    <table><tr><th>Time</th><th>Name</th><th>Phone</th><th>Build Type</th>
    <th>Lot</th><th>Timeline</th><th>Budget</th><th>Priority</th></tr>${rows}</table>`;
}
function esc(s){ return String(s ?? '').replace(/[&<>"']/g,
  c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }
refresh();
setInterval(refresh, 5000);
</script>
</body>
</html>
"""


class Handler(BaseHTTPRequestHandler):
    server_version = "LeadCaptureDemo/1.0"

    # -- helpers -----------------------------------------------------------
    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")

    def _send(self, code, body, ctype="text/html; charset=utf-8"):
        data = body.encode("utf-8") if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self._cors()
        self.end_headers()
        self.wfile.write(data)

    def _json(self, obj, code=200):
        self._send(code, json.dumps(obj), "application/json")

    def _static(self, directory, filename, ctype):
        # Prevent path traversal.
        safe = os.path.normpath(filename).lstrip("/")
        base = os.path.abspath(directory)
        path = os.path.normpath(os.path.join(base, safe))
        if not path.startswith(base + os.sep):
            return self._send(403, "Forbidden", "text/plain")
        if not os.path.isfile(path):
            return self._send(404, "Not found", "text/plain")
        with open(path, "rb") as f:
            self._send(200, f.read(), ctype)

    # -- routes ------------------------------------------------------------
    def do_OPTIONS(self):
        self.send_response(204)
        self._cors()
        self.end_headers()

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path

        if path == "/":
            return self._static(SITE_DIR, "index.html", "text/html; charset=utf-8")
        if path == "/widget/chat-widget.js":
            return self._static(WIDGET_DIR, "chat-widget.js",
                                "application/javascript; charset=utf-8")
        if path == "/dashboard":
            return self._send(200, DASHBOARD_HTML)
        if path == "/api/leads":
            return self._json(load_leads())
        # Static assets under /site/ (images etc. if added later).
        if path.startswith("/site/"):
            name = path[len("/site/"):]
            ctype = "text/html; charset=utf-8" if name.endswith(".html") else \
                    "text/css" if name.endswith(".css") else \
                    "application/javascript" if name.endswith(".js") else \
                    "application/octet-stream"
            return self._static(SITE_DIR, name, ctype)
        return self._send(404, "Not found", "text/plain")

    def do_POST(self):
        parsed = urllib.parse.urlparse(self.path)
        if parsed.path != "/api/chat":
            return self._send(404, "Not found", "text/plain")
        try:
            length = int(self.headers.get("Content-Length", 0))
            payload = json.loads(self.rfile.read(length) or b"{}")
        except (ValueError, json.JSONDecodeError):
            return self._json({"error": "invalid JSON"}, 400)

        session_id = str(payload.get("session_id") or "anon")
        message = str(payload.get("message") or "")

        with _sessions_lock:
            conv = _sessions.get(session_id)
            if conv is None:
                conv, greeting = start_conversation()
                _sessions[session_id] = conv
            else:
                greeting = None

        if message == "__init__":
            return self._json({"reply": greeting, "lead_captured": False})

        reply, captured, lead = respond(conv, message)
        if captured and lead:
            save_lead(lead)
            notify_owner(lead)
        return self._json({"reply": reply, "lead_captured": captured})

    def log_message(self, fmt, *args):  # quieter logs
        if self.path != "/api/leads":
            super().log_message(fmt, *args)


def main():
    import sys
    port = int(sys.argv[1]) if len(sys.argv) > 1 else int(os.environ.get("PORT", 8080))
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"Willrich Homes demo running at http://localhost:{port}")
    print(f"Owner dashboard at http://localhost:{port}/dashboard")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.")


if __name__ == "__main__":
    main()
