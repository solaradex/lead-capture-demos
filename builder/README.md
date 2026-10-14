# AI Lead-Capture Demo — "Oakridge Custom Homes"

A complete, working demo of the product Brad sells to local service
businesses: a business website with an AI concierge chat widget that
captures leads, "texts" the owner, and feeds an owner dashboard.

## Run it (one command, no installs — Python 3 stdlib only)

```bash
cd ~/workspace/goals/ai-lead-capture-business-for-local-services/demo-builder/backend
python3 server.py
```

Then open:

| What | URL |
|---|---|
| Demo business site | http://localhost:8080/ |
| Owner dashboard | http://localhost:8080/dashboard |

Custom port: `python3 server.py 9000`. (Run each demo on its own port if
you want several live at once: plumbing 8080, pest 8081, GC 8082, builder 8083.)

## What's inside

```
demo-builder/
├── site/index.html          # Fake home builder one-pager (embeds the widget)
├── widget/chat-widget.js    # Embeddable widget — ONE <script> tag, vanilla JS
├── backend/
│   ├── server.py            # HTTP server: static files + chat API + dashboard
│   ├── responder.py         # Rule-based AI concierge (no API key needed)
│   ├── leads.json           # Captured leads (created on first lead)
│   └── sms.log              # Fake "SMS sent to owner" log (Twilio goes here later)
└── README.md                # This file
```

**How the widget talks to the backend:** `POST /api/chat` with
`{"session_id": "...", "message": "..."}` → `{"reply": "...", "lead_captured": true/false}`.
The widget derives the backend origin from its own `<script src>`, so the
same snippet works on any domain (CORS is open for the demo).

**How the AI works (`responder.py`):** a small state machine —
classify intent (custom build / spec home / lot question / model tour /
floor plans / pricing / timeline / financing), then steer toward the
missing fields (name → phone → lot status → timeline → budget).
Phone/name/lot/timeline/budget are also extracted opportunistically from
any message. On capture: lead → `leads.json` + `[SMS -> owner]` log line.

## 🎬 Demo script for the sales call

**Setup (before the call):** start the server, open the demo site in one
browser tab and `http://localhost:8080/dashboard` in another. Delete
`backend/leads.json` for a clean dashboard.

**The pitch:** *"Let me show you what happens when a couple visits your
site on a Sunday afternoon dreaming about building — and your sales
office is closed."*

1. **Click the chat bubble** (bottom-right). The AI greets you as "Casey".
2. **Type as the customer:** `we want to build a custom home, how long does it take?`
   → Watch it answer the timeline question AND keep qualifying.
3. **Type:** `Mike and Lisa`
   → It asks for the phone number.
4. **Type:** `555-321-9876`
   → It asks about lot status (own land vs. need a homesite).
5. **Type:** `we need a lot`
   → It asks about timeline, then budget.
6. **Type:** `6-12 months` → `$500k-$700k`
   → It confirms with a full summary and books the free consultation.
7. **Flip to the dashboard tab** (it auto-refreshes every 5s) — the lead
   is there, with build type, lot status, timeline, and budget.
8. **Show the terminal** (or `backend/sms.log`) — *"and at the same moment,
   you get this text on your phone."*

**Killer line:** *"That couple just told you their budget, their timeline,
and that they need a lot — before your competitor's sales rep even checks
Monday's emails."*

**If they ask "what about financing?":** start a new chat (refresh the
page) and type `do you help with construction loans?` → watch it answer
the financing question, then steer right back to capturing the lead.

## Turning this into the real product

- [ ] Swap `notify_owner()` in `server.py` for Twilio (SMS to the owner)
- [ ] Replace `responder.py` rules with an LLM call (keep the state machine
      as a guardrail — it already defines the fields to capture)
- [ ] Persist sessions + leads in a real DB (currently in-memory + JSON file)
- [ ] Multi-tenant: business name, agent name, and owner phone per client
      (the widget already accepts these as `data-` attributes)
- [ ] Auth on `/dashboard`
