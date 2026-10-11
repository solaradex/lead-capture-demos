# AI Lead-Capture Demo — "Summit Ridge Roofing"

A complete, working demo of the product Brad sells to roofing companies:
a business website with an AI receptionist chat widget that captures
leads, "texts" the owner, and feeds an owner dashboard.

This is Phase 1 of the roofing vertical SaaS (goal: `roofing-vertical-saas`).
(Sibling demos: plumbing at `~/workspace/goals/ai-lead-capture-business-for-local-services/demo/`
and pest control at `.../demo-pest/` — same architecture, different vertical.)

## Run it (one command, no installs — Python 3 stdlib only)

```bash
cd ~/workspace/goals/roofing-vertical-saas/demo/backend
python3 server.py
```

Then open:

| What | URL |
|---|---|
| Demo business site | http://localhost:8080/ |
| Owner dashboard | http://localhost:8080/dashboard |

Custom port (e.g. to run alongside another demo): `python3 server.py 8082`.

## What's inside

```
demo/
├── site/index.html          # Fake roofing one-pager (embeds the widget)
├── widget/chat-widget.js    # Same embeddable widget — ONE <script> tag
├── backend/
│   ├── server.py            # HTTP server: static files + chat API + dashboard
│   ├── responder.py         # Rule-based AI receptionist, roofing knowledge base
│   ├── leads.json           # Captured leads (created on first lead)
│   └── sms.log              # Fake "SMS sent to owner" log (Twilio goes here later)
└── README.md                # This file
```

**Knowledge base (`responder.py`):** storm/hail damage (HIGH urgency),
active roof leak (urgent — same-day tarping), roof replacement
($8k–$15k asphalt, $15k–$30k metal), roof repair ($300–$1,500), insurance
claims ("we handle the paperwork"), gutters ($1,200–$2,500), emergency
tarping ($399). The closer on every flow is the FREE drone inspection.

**Capture flow:** name → phone → address → issue → preferred time
(emergencies fast-track: address → phone). Urgent flows promise same-day
tarping; insurance flows promise "just your deductible."

## 🎬 Demo script for the sales call

**Setup (before the call):** start the server, open the demo site in one
browser tab and `http://localhost:8080/dashboard` in another. Delete
`backend/leads.json` for a clean dashboard.

**The pitch:** *"Let me show you what happens when a homeowner's ceiling is
leaking at 10pm and you're closed."*

1. **Click the chat bubble** (bottom-right). The AI greets you as "Jake".
2. **Type as the customer:** `Water is pouring through my ceiling right now!`
   → Watch it switch to urgent mode and promise same-day tarping, then ask
   for the address.
3. **Type:** `12 Hillcrest Lane`
   → It asks for the phone number.
4. **Type:** `555-321-9876`
   → It confirms: tarp today, free drone inspection, project manager calls
   within minutes.
5. **Flip to the dashboard tab** (it auto-refreshes every 5s) — the lead
   is there, flagged URGENT.
6. **Show the terminal** (or `backend/sms.log`) — *"and at the same moment,
   you get this text on your phone."*

**Killer line:** *"That homeowner was about to call the storm-chaser with
the truck. Instead they booked with you — while you were asleep."*

**If they ask "what if it's not an emergency?":** start a new chat
(refresh the page) and type `my roof is 25 years old, how much to replace it` →
`Tom Bradley` → `5553211234` → `88 Pine Street` → `next week`.
Same capture, calm tone, free-drone-inspection closer.

**If they ask about insurance:** type `will insurance cover hail damage` →
the AI explains the claim process and that most jobs cost just the
deductible.

## Turning this into the real product

- [ ] Swap `notify_owner()` in `server.py` for Twilio (SMS to the owner)
- [ ] Replace `responder.py` rules with an LLM call (keep the state machine
      as a guardrail — it already defines the fields to capture)
- [ ] Persist sessions + leads in a real DB (currently in-memory + JSON file)
- [ ] Multi-tenant: business name, agent name, and owner phone per client
      (the widget already accepts these as `data-` attributes)
- [ ] Auth on `/dashboard`
- [ ] Phase 2 (SaaS depth): job scheduling, crew dispatch, insurance-claim
      tracker, review-request automation
