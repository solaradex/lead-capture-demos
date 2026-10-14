# AI Lead-Capture Demo — "Summit General Contracting"

A complete, working demo of the product Brad sells to local service
businesses: a business website with an AI receptionist chat widget that
captures leads, "texts" the owner, and feeds an owner dashboard.

## Run it (one command, no installs — Python 3 stdlib only)

```bash
cd ~/workspace/goals/ai-lead-capture-business-for-local-services/demo-gc/backend
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
demo-gc/
├── site/index.html          # Fake GC company one-pager (embeds the widget)
├── widget/chat-widget.js    # Embeddable widget — ONE <script> tag, vanilla JS
├── backend/
│   ├── server.py            # HTTP server: static files + chat API + dashboard
│   ├── responder.py         # Rule-based AI receptionist (no API key needed)
│   ├── leads.json           # Captured leads (created on first lead)
│   └── sms.log              # Fake "SMS sent to owner" log (Twilio goes here later)
└── README.md                # This file
```

**How the widget talks to the backend:** `POST /api/chat` with
`{"session_id": "...", "message": "..."}` → `{"reply": "...", "lead_captured": true/false}`.
The widget derives the backend origin from its own `<script src>`, so the
same snippet works on any domain (CORS is open for the demo).

**How the AI works (`responder.py`):** a small state machine —
classify intent (storm damage / structural emergency vs. kitchen / bath /
addition / roofing / … / pricing / timeline), then steer toward the missing
fields (name → phone → project address → timeline → budget). Phone/name/
timeline/budget are also extracted opportunistically from any message.
Emergencies skip straight to address → phone. On capture: lead →
`leads.json` + `[SMS -> owner]` log line.

## 🎬 Demo script for the sales call

**Setup (before the call):** start the server, open the demo site in one
browser tab and `http://localhost:8080/dashboard` in another. Delete
`backend/leads.json` for a clean dashboard.

**The pitch:** *"Let me show you what happens when a homeowner visits your
site at 11pm dreaming about a new kitchen — and you're asleep."*

1. **Click the chat bubble** (bottom-right). The AI greets you as "Alex".
2. **Type as the customer:** `i want to remodel my kitchen, how much does it cost?`
   → Watch it answer the pricing question AND keep the conversation moving.
3. **Type:** `Sarah`
   → It asks for the phone number.
4. **Type:** `555-987-6543`
   → It asks for the project address.
5. **Type:** `Springfield`
   → It asks about timeline, then budget.
6. **Type:** `in the next few months` → `around 40k`
   → It confirms the booking with a full summary.
7. **Flip to the dashboard tab** (it auto-refreshes every 5s) — the lead
   is there, with project type, timeline, and budget.
8. **Show the terminal** (or `backend/sms.log`) — *"and at the same moment,
   you get this text on your phone."*

**Killer line:** *"That homeowner just qualified themselves — project,
timeline, and budget — while you were asleep. Your competitor's contact
form got an email they'll read Monday."*

**If they ask "what about storm damage?":** start a new chat (refresh the
page) and type `a tree fell on my roof` → `456 Elm St` → `5551234567`.
Watch it switch to urgent mode and skip the small talk.

## Turning this into the real product

- [ ] Swap `notify_owner()` in `server.py` for Twilio (SMS to the owner)
- [ ] Replace `responder.py` rules with an LLM call (keep the state machine
      as a guardrail — it already defines the fields to capture)
- [ ] Persist sessions + leads in a real DB (currently in-memory + JSON file)
- [ ] Multi-tenant: business name, agent name, and owner phone per client
      (the widget already accepts these as `data-` attributes)
- [ ] Auth on `/dashboard`
