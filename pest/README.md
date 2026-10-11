# AI Lead-Capture Demo — "ShieldGuard Pest Control"

A complete, working demo of the product Brad sells to pest control
companies: a business website with an AI receptionist chat widget that
captures leads, "texts" the owner, and feeds an owner dashboard.

(Sibling demo: the plumbing version lives in `../demo/` — same
architecture, different vertical.)

## Run it (one command, no installs — Python 3 stdlib only)

```bash
cd ~/workspace/goals/ai-lead-capture-business-for-local-services/demo-pest/backend
python3 server.py
```

Then open:

| What | URL |
|---|---|
| Demo business site | http://localhost:8080/ |
| Owner dashboard | http://localhost:8080/dashboard |

Custom port (e.g. to run both demos at once): `python3 server.py 8081`.

## What's inside

```
demo-pest/
├── site/index.html          # Fake pest control one-pager (embeds the widget)
├── widget/chat-widget.js    # Same embeddable widget — ONE <script> tag
├── backend/
│   ├── server.py            # HTTP server: static files + chat API + dashboard
│   ├── responder.py         # Rule-based AI receptionist, pest knowledge base
│   ├── leads.json           # Captured leads (created on first lead)
│   └── sms.log              # Fake "SMS sent to owner" log (Twilio goes here later)
└── README.md                # This file
```

**Knowledge base (`responder.py`):** bed bugs (HIGH urgency), wasp/hornet
nest (urgent — safety), termites, rodents, wildlife removal, mosquitoes,
ants/roaches, plus a full price sheet (pest plan $39/mo, termite inspection
free / treatment from $499, bed bugs from $299/room, mosquito $59/visit,
wasp nest $149, wildlife = free inspection + custom quote).

**Capture flow:** name → phone → address → service → preferred time
(emergencies fast-track: address → phone). The closer on every flow is the
FREE inspection.

## 🎬 Demo script for the sales call

**Setup (before the call):** start the server, open the demo site in one
browser tab and `http://localhost:8080/dashboard` in another. Delete
`backend/leads.json` for a clean dashboard.

**The pitch:** *"Let me show you what happens when a homeowner finds bed
bugs at 10pm and you're closed."*

1. **Click the chat bubble** (bottom-right). The AI greets you as "Sam".
2. **Type as the customer:** `I just found bed bugs in my kid's room!`
   → Watch it switch to urgent mode and ask for the address.
3. **Type:** `47 Maple Drive`
   → It asks for the phone number.
4. **Type:** `555-987-6543`
   → It confirms the booking and promises a tech will call within minutes.
5. **Flip to the dashboard tab** (it auto-refreshes every 5s) — the lead
   is there, flagged URGENT.
6. **Show the terminal** (or `backend/sms.log`) — *"and at the same moment,
   you get this text on your phone."*

**Killer line:** *"That homeowner was about to call your competitor. Instead
they booked with you — while you were asleep."*

**If they ask "what if it's not an emergency?":** start a new chat
(refresh the page) and type `I keep seeing ants in my kitchen` →
`Lisa Carter` → `5551234567` → `88 Pine Street` → `tomorrow morning`.
Same capture, calm tone, free-inspection closer.

## Turning this into the real product

Same roadmap as the plumbing demo — see `../demo/README.md`:

- [ ] Swap `notify_owner()` in `server.py` for Twilio (SMS to the owner)
- [ ] Replace `responder.py` rules with an LLM call (keep the state machine
      as a guardrail — it already defines the fields to capture)
- [ ] Persist sessions + leads in a real DB (currently in-memory + JSON file)
- [ ] Multi-tenant: business name, agent name, and owner phone per client
      (the widget already accepts these as `data-` attributes)
- [ ] Auth on `/dashboard`
