# Willrich Homes LLC — AI Lead-Capture Demo

Custom-branded demo site for **Willrich Homes LLC** of Macclenny, FL —
"Making Dreams Come True!"

## What's here

- **Premium black/gold site** (`site/index.html`) — matches their actual logo
  and branding. Featured build: 464 E Shuey Ave spec home.
- **AI receptionist "Amanda"** (`backend/responder.py`) — rule-based,
  handles custom builds, spec homes, pricing, USDA/financing questions,
  lot evaluations, and captures name/phone/lot/timeline/budget.
- **Chat widget** (`widget/chat-widget.js`) — one script tag embed.
- **Owner dashboard** (`/dashboard`) — view captured leads.
- **Fake owner SMS log** — replace with Twilio in production.

## Business details

- Phone: (904) 408-0421 · Email: willrichhomes@gmail.com
- Area: Macclenny & Baker County, FL
- No website currently (Facebook-only) — website build ($997) + AI tool
  ($1,200 setup + $250/mo) pitch.

## Run locally

```bash
cd willrich && python3 backend/server.py 8000
# Site: http://localhost:8000/  Dashboard: http://localhost:8000/dashboard
```
