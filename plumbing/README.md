# AI Lead-Capture Demo — "Mike's Plumbing & Heating"

A complete, working demo of the product Brad sells to local service
businesses: a business website with an AI receptionist chat widget that
captures leads, "texts" the owner, and feeds an owner dashboard.

## Run it (one command, no installs — Python 3 stdlib only)

```bash
cd ~/workspace/goals/ai-lead-capture-business-for-local-services/demo/backend
python3 server.py
```

Then open:

| What | URL |
|---|---|
| Demo business site | http://localhost:8080/ |
| Owner dashboard | http://localhost:8080/dashboard |

Custom port: `python3 server.py 9000`.

## What's inside

```
demo/
├── site/index.html          # Fake plumbing company one-pager (embeds the widget)
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
classify intent (emergency / water heater / drain / leak / pricing / …),
then steer toward the missing fields (name → phone → time, or
address → phone for emergencies). Phone/name/time are also extracted
opportunistically from any message. On capture: lead → `leads.json` +
`[SMS -> owner]` log line.

## 🎬 Demo script for the sales call

**Setup (before the call):** start the server, open the demo site in one
browser tab and `http://localhost:8080/dashboard` in another. Delete
`backend/leads.json` for a clean dashboard.

**The pitch:** *"Let me show you what happens when a customer visits your
site at 11pm and you're asleep."*

1. **Click the chat bubble** (bottom-right). The AI greets you as "Sam".
2. **Type as the customer:** `my basement is flooding!`
   → Watch it switch to urgent mode and ask for the address.
3. **Type:** `123 Oak Street`
   → It asks for the phone number.
4. **Type:** `555-234-5678`
   → It confirms the booking with a summary.
5. **Flip to the dashboard tab** (it auto-refreshes every 5s) — the lead
   is there, flagged URGENT.
6. **Show the terminal** (or `backend/sms.log`) — *"and at the same moment,
   you get this text on your phone."*

**Killer line:** *"That customer just booked themselves. You were asleep.
Your competitor's phone went to voicemail."*

**If they ask "what if it's not an emergency?":** start a new chat
(refresh the page) and type `my water heater stopped working` →
`John Miller` → `5551234567` → `tomorrow morning`. Same capture, calm tone.

## Turning this into the real product

- [ ] Swap `notify_owner()` in `server.py` for Twilio (SMS to the owner)
- [ ] Replace `responder.py` rules with an LLM call (keep the state machine
      as a guardrail — it already defines the fields to capture)
- [ ] Persist sessions + leads in a real DB (currently in-memory + JSON file)
- [ ] Multi-tenant: business name, agent name, and owner phone per client
      (the widget already accepts these as `data-` attributes)
- [ ] Auth on `/dashboard`
