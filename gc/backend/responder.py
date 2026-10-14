"""Rule-based AI receptionist for the Summit General Contracting demo.

No external AI API is needed. A small state machine classifies the
customer's intent (storm damage / structural emergencies vs. remodel
projects), then steers the conversation toward capturing: name, phone,
project type, project address/area, timeline, and budget range.

The closer: push the FREE on-site estimate — every project starts with one.

Public API:
    conv, greeting = start_conversation()
    reply, lead_captured, lead = respond(conv, user_text)

`lead` is a dict ready to be stored when `lead_captured` is True.
"""

import re
from datetime import datetime, timezone

# ---------------------------------------------------------------------------
# Intent classification
# ---------------------------------------------------------------------------

# Emergency keywords are checked first — storm damage and structural issues
# jump the line. Pricing is checked next ("how much does a kitchen remodel
# cost" is a pricing question first, a kitchen job second), then services.
EMERGENCY_INTENTS = [
    ("storm damage", ["storm", "hail", "wind damage", "tree fell", "tree on",
                      "roof caved", "flood damage"]),
    ("structural issue", ["foundation", "crack in the wall", "cracks in wall",
                          "sagging", "structural", "collapse", "collapsing",
                          "sinking"]),
    ("urgent request", ["emergency", "urgent", "asap", "right now",
                        "immediately"]),
]

PRICING_KEYWORDS = ["price", "cost", "how much", "quote", "estimate", "charge",
                    "rate", "free", "budget"]

SERVICE_INTENTS = [
    ("kitchen remodel", ["kitchen", "cabinet", "countertop", "counter top",
                         "backsplash"]),
    ("bathroom remodel", ["bathroom", "bath ", "shower", "tub", "vanity",
                          "toilet"]),
    ("home addition", ["addition", "add a room", "extra bedroom", "second story",
                       "bump out", "bump-out", "in-law"]),
    ("roofing", ["roof", "shingle", "reroof", "re-roof", "new roof"]),
    ("flooring", ["floor", "hardwood", "laminate", "vinyl plank", "tile floor",
                  "carpet"]),
    ("painting", ["paint", "repaint", "exterior paint", "interior paint"]),
    ("deck/patio", ["deck", "patio", "porch", "outdoor living", "pergola"]),
    ("basement finishing", ["basement", "finish my basement", "finished basement"]),
    ("general renovation", ["renovat", "remodel", "whole house", "fix up",
                            "flip", "update my home"]),
]


def classify(text):
    """Return (service_label, is_emergency, is_pricing).

    Falls back to "general renovation". Pricing is flagged separately so the
    ack can answer the price question AND name the detected project type.
    """
    low = text.lower()
    for label, keywords in EMERGENCY_INTENTS:
        if any(k in low for k in keywords):
            return label, True, False
    pricing = any(k in low for k in PRICING_KEYWORDS)
    service = "general renovation"
    for label, keywords in SERVICE_INTENTS:
        if any(k in low for k in keywords):
            service = label
            break
    return service, False, pricing


# ---------------------------------------------------------------------------
# Field extraction (runs opportunistically on every message)
# ---------------------------------------------------------------------------

PHONE_RE = re.compile(
    r"(?:\+?1[\s\-\.]?)?"          # optional country code
    r"(?:\(?(\d{3})\)?[\s\-\.]?)"  # area code
    r"(\d{3})[\s\-\.]?"            # exchange
    r"(\d{4})"                     # line number
)


def extract_phone(text):
    """Return a normalized 10-digit phone number, or None."""
    m = PHONE_RE.search(text)
    if not m:
        return None
    digits = "".join(m.groups())
    if len(digits) == 10:
        return f"({digits[:3]}) {digits[3:6]}-{digits[6:]}"
    return None


# Explicit "my name is X" / "this is X" / "I'm X" (capitalized, 1-2 words).
NAME_PATTERNS = [
    re.compile(r"\bmy name is\s+([A-Z][a-zA-Z'\-]{1,20}(?:\s+[A-Z][a-zA-Z'\-]{1,20})?)",
               re.IGNORECASE),
    re.compile(r"\bthis is\s+([A-Z][a-zA-Z'\-]{1,20}(?:\s+[A-Z][a-zA-Z'\-]{1,20})?)",
               re.IGNORECASE),
    re.compile(r"\bi['\u2019]m\s+([A-Z][a-zA-Z'\-]{1,20}(?:\s+[A-Z][a-zA-Z'\-]{1,20})?)\b",
               re.IGNORECASE),
]


def extract_name(text):
    for pat in NAME_PATTERNS:
        m = pat.search(text)
        if m:
            return m.group(1).strip()
    return None


TIMELINE_KEYWORDS = [
    ("ASAP", ["asap", "right away", "immediately", "urgent",
              "as soon as possible", "right now"]),
    ("within a month", ["this month", "within a month", "next few weeks",
                        "few weeks"]),
    ("1-3 months", ["1-3 months", "one to three months", "couple of months",
                    "few months"]),
    ("3-6 months", ["3-6 months", "three to six months", "half a year",
                    "later this year"]),
    ("6-12 months", ["6-12 months", "six to twelve", "next year",
                     "within a year"]),
    ("just researching", ["just looking", "researching", "browsing",
                          "not sure yet", "exploring", "idea stage",
                          "planning ahead", "someday"]),
]


def extract_timeline(text):
    low = text.lower()
    for label, keywords in TIMELINE_KEYWORDS:
        if any(k in low for k in keywords):
            return label
    return None


def extract_budget(text):
    """Return a normalized budget string, or None.

    Handles "$75k", "$50k-$100k", "under $100k", "$250,000", "100k".
    """
    low = text.lower()
    # Range like "$50k-$100k" or "50-100k".
    m = re.search(r"\$?\s?(\d+)\s?[kK]?\s?[-–to]+\s?\$?\s?(\d+)\s?[kK]\b", low)
    if m:
        lo, hi = m.group(1), m.group(2)
        lo_s = f"${lo}k" if int(lo) < 1000 else f"${int(lo):,}"
        hi_s = f"${hi}k" if int(hi) < 1000 else f"${int(hi):,}"
        return f"{lo_s}-{hi_s}"
    # "under $100k" / "up to $75k".
    m = re.search(r"\b(?:under|up to|less than)\s?\$?\s?(\d+)\s?[kK]\b", low)
    if m:
        return f"under ${m.group(1)}k"
    # Single "$75k" / "$250,000".
    m = re.search(r"\$\s?(\d{1,3}(?:,\d{3})+|\d+)\s?[kK]?\b", low)
    if m:
        raw = m.group(1).replace(",", "")
        if m.group(0).strip().endswith(("k", "K")):
            return f"${raw}k"
        val = int(raw)
        return f"${val}k" if val < 1000 else f"${val:,}"
    # Bare "100k".
    m = re.search(r"\b(\d+)\s?[kK]\b", low)
    if m:
        return f"${m.group(1)}k"
    # Vague budget signals.
    if any(k in low for k in ["tight budget", "small budget", "on a budget"]):
        return "tight budget"
    if any(k in low for k in ["no budget limit", "money is no object",
                              "budget isn't an issue", "flexible budget"]):
        return "flexible / no limit"
    return None


# ---------------------------------------------------------------------------
# Conversation state machine
# ---------------------------------------------------------------------------

GREETING = (
    "Hi there! \U0001F44B Thanks for visiting Summit General Contracting — "
    "I'm Alex, the virtual receptionist. Thinking about a remodel, an "
    "addition, or something else for your home? (Free on-site estimates!)"
)

OFF_TOPIC = (
    "I can help with remodeling questions and scheduling a free estimate. "
    "What project are you thinking about for your home?"
)


class Conversation:
    def __init__(self):
        self.stage = "problem"  # problem -> collect -> done
        self.emergency = False
        self.pricing = False
        self.data = {
            "service": None,
            "name": None,
            "phone": None,
            "address": None,
            "timeline": None,
            "budget": None,
        }
        self.turns = 0


def start_conversation():
    return Conversation(), GREETING


PRICE_GUIDE = (
    "Here's a rough guide so you can plan: kitchen remodels typically run "
    "$25k-$75k, bathrooms $12k-$30k, basement finishing $30k-$60k, decks "
    "$15k-$35k, and full additions $80k-$200k+. Every project starts with a "
    "FREE on-site estimate with exact pricing — no surprises, ever."
)


def _ack_for(service, emergency, pricing):
    if emergency:
        if service == "storm damage":
            return ("I'm sorry — storm damage is stressful. We'll get someone "
                    "out to tarp and assess it fast.")
        if service == "structural issue":
            return ("That needs a professional look right away — foundation and "
                    "structural issues only get worse. I'll get you prioritized.")
        return "That sounds urgent — I'll get you prioritized right away."
    if pricing:
        guide = PRICE_GUIDE.split(": ", 1)[1]  # strip "Here's a rough guide…:"
        if service != "general renovation":
            return (f"Great question — for a {service}, {guide}")
        return PRICE_GUIDE
    acks = {
        "kitchen remodel": ("The kitchen is the heart of the home — and our most "
                            "popular project. You're in good hands."),
        "bathroom remodel": ("Bathrooms are where we shine — most of ours finish "
                             "in 2-3 weeks."),
        "home addition": ("An addition is a big decision — smart to talk to a "
                          "licensed GC first. We'll handle design through permits."),
        "roofing": ("Got it — roofing. We do full replacements and repairs, "
                    "and we handle the insurance paperwork on storm claims."),
        "flooring": "New floors change everything — what rooms are you thinking?",
        "painting": ("Fresh paint is the fastest facelift there is. Interior, "
                     "exterior, or both?"),
        "deck/patio": ("Outdoor living space — love it. Spring bookings fill up "
                       "fast, so good timing to talk."),
        "basement finishing": ("Finished basements add the cheapest square footage "
                               "you'll ever buy. Let's talk layout."),
        "general renovation": "Got it — I can help with that.",
    }
    return acks.get(service, "Got it — I can help with that.")


def _next_missing_field(conv):
    """Name of the next field the bot needs, or None when ready to capture."""
    d = conv.data
    if conv.emergency:
        if not d["address"]:
            return "address"
        if not d["phone"]:
            return "phone"
        if not d["name"]:
            return "name"
    else:
        if not d["name"]:
            return "name"
        if not d["phone"]:
            return "phone"
        if not d["address"]:
            return "address"
        if not d["timeline"]:
            return "timeline"
        if not d["budget"]:
            return "budget"
    return None


def _next_prompt(conv):
    """Return the next question needed, or None when ready to capture."""
    prompts = {
        "address": ("What's the project address? (Or just the town — we'll "
                    "nail down details at the free estimate.)"),
        "phone": "And what's the best phone number to reach you at?",
        "name": "What's your name?",
        "timeline": ("When are you hoping to get started — ASAP, in the next "
                     "few months, or just researching for now?"),
        "budget": ("And roughly what budget range are you thinking for the "
                   "project? (A ballpark like \"$30k-$50k\" is perfect.)"),
    }
    field = _next_missing_field(conv)
    if field == "phone" and not conv.emergency:
        return "Thanks! What's the best phone number for our project manager to reach you?"
    if field == "budget":
        # Budget is optional-ish: offer a skip so the conversation never stalls.
        return (prompts["budget"] + " If you're not sure yet, just say "
                "'not sure' and we'll figure it out together.")
    return prompts.get(field)


def _capture(conv):
    d = conv.data
    lead = {
        "id": f"lead-{datetime.now(timezone.utc).strftime('%Y%m%d%H%M%S')}",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "name": d["name"] or "Not provided",
        "phone": d["phone"] or "Not provided",
        "service": d["service"] or "general renovation",
        "address": d["address"] or "Not provided",
        "preferred_time": d["timeline"] or "not specified",
        "budget": d["budget"] or "not specified",
        "emergency": conv.emergency,
    }
    return lead


def _confirmation(lead):
    lines = [
        "You're all set! Here's what I have:",
        f"\u2022 Name: {lead['name']}",
        f"\u2022 Phone: {lead['phone']}",
        f"\u2022 Project: {lead['service']}",
    ]
    if lead["address"] != "Not provided":
        lines.append(f"\u2022 Project address: {lead['address']}")
    lines.append(f"\u2022 Timeline: {lead['preferred_time']}")
    lines.append(f"\u2022 Budget: {lead['budget']}")
    lines.append("")
    if lead["emergency"]:
        lines.append("\U0001F6A8 A project manager will call "
                     f"{lead['phone']} within minutes — we'll get someone out "
                     "to assess the damage today.")
    else:
        lines.append("Next step: a FREE on-site estimate. Our project manager "
                     f"will call {lead['phone']} shortly to schedule it. "
                     "Prefer to talk now? Call (555) 036-2210. \U0001F3D7️")
    return "\n".join(lines)


def respond(conv, user_text):
    """Process one user message.

    Returns (reply_text, lead_captured_bool, lead_dict_or_None).
    """
    text = (user_text or "").strip()
    conv.turns += 1

    if conv.stage == "done":
        return ("You're all set! Our project manager will be in touch shortly. "
                "Anything else I can help with?", False, None)

    if not text:
        return ("I didn't quite catch that — could you say it again?", False, None)

    # --- opportunistic extraction: grab whatever the customer volunteers ---
    phone = extract_phone(text)
    if phone and not conv.data["phone"]:
        conv.data["phone"] = phone
    name = extract_name(text)
    if name and not conv.data["name"]:
        conv.data["name"] = name
    tm = extract_timeline(text)
    if tm and not conv.data["timeline"]:
        conv.data["timeline"] = tm
    budget = extract_budget(text)
    if budget and not conv.data["budget"]:
        conv.data["budget"] = budget

    # --- stage: figure out the project ---
    if conv.stage == "problem":
        service, emergency, pricing = classify(text)
        low = text.lower()
        if (service == "general renovation" and not pricing and any(
            w in low for w in ["just looking", "browsing", "hello", "hi"]
        ) and len(low.split()) <= 3):
            return (OFF_TOPIC, False, None)
        conv.data["service"] = service
        conv.emergency = emergency
        conv.pricing = pricing
        conv.stage = "collect"
        prompt = _next_prompt(conv)
        return (f"{_ack_for(service, emergency, pricing)} {prompt}", False, None)

    # --- stage: collect missing fields ---
    d = conv.data
    # Address/area: accept any substantive non-phone text when it's the next
    # missing field (e.g. "Springfield" or "123 Oak Street").
    if (_next_missing_field(conv) == "address" and not phone
            and len(text) > 2):
        d["address"] = text
    # Timeline fallback: if the reply mentions nothing extractable but the
    # timeline is next, keep asking (prompt handles it).
    # Name fallback: short digit-free reply while the name is the next
    # missing field (e.g. user just types "John").
    if (_next_missing_field(conv) == "name"
            and not any(ch.isdigit() for ch in text)
            and len(text.split()) <= 4
            and not extract_timeline(text)
            and not extract_budget(text)):
        d["name"] = text.strip().title()
    # Budget skip: let "not sure" / "skip" pass without stalling.
    if (_next_missing_field(conv) == "budget"
            and any(w in text.lower() for w in ["not sure", "don't know",
                                               "dont know", "skip", "unsure"])):
        d["budget"] = "not specified yet"

    prompt = _next_prompt(conv)
    if prompt is None:
        lead = _capture(conv)
        conv.stage = "done"
        return (_confirmation(lead), True, lead)
    return (prompt, False, None)
