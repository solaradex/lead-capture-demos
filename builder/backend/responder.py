"""Rule-based AI receptionist for the Oakridge Custom Homes demo.

No external AI API is needed. A small state machine classifies the
customer's intent (custom build / spec home / lot questions / model tour /
financing), then steers the conversation toward capturing: name, phone,
build type, lot status (own lot vs. need lot), timeline, and budget range.

The closer: push the FREE consultation + model home tour.

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

# (service_label, keywords)
# Order: pricing/timeline/financing questions first (they can combine with a
# build type, e.g. "how much does a custom home cost"), then build types.
QUESTION_KEYWORDS = {
    "pricing question": ["price", "cost", "how much", "quote", "estimate",
                         "charge", "rate", "per square foot", "per sq ft",
                         "psf"],
    "timeline question": ["how long", "timeline", "how soon", "when can",
                          "lead time", "build time", "move in"],
    "financing question": ["financ", "loan", "mortgage", "lender", "down payment",
                           "pre-approv", "construction loan"],
    "model tour request": ["tour", "model home", "show home", "visit", "see one",
                           "open house", "walk through", "walkthrough"],
    "floor plan question": ["floor plan", "floorplan", "plan", "layout",
                            "blueprint", "how many bed", "bedroom",
                            "square foot", "sq ft"],
    "lot/land question": ["lot", "land", "acre", "property", " homesite",
                          "where to build", "neighborhood", "community",
                          "subdivision"],
}

BUILD_INTENTS = [
    ("custom home build", ["custom", "dream home", "build from scratch",
                           "design my own", "architect"]),
    ("spec home purchase", ["spec home", "spec", "quick move", "move-in ready",
                            "inventory home", "already built"]),
    ("new construction", ["new home", "new build", "new construction",
                          "building a home", "build a home", "construct"]),
]


def classify(text):
    """Return (service_label, question_label_or_None).

    Falls back to "new construction". A question label (pricing, timeline,
    …) is returned separately so the ack can answer it AND name the build
    type in one breath.
    """
    low = text.lower()
    question = None
    for label, keywords in QUESTION_KEYWORDS.items():
        if any(k in low for k in keywords):
            question = label
            break
    service = "new construction"
    for label, keywords in BUILD_INTENTS:
        if any(k in low for k in keywords):
            service = label
            break
    return service, question


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


LOT_KEYWORDS = [
    ("own lot", ["own a lot", "have land", "already own", "my lot", "our land",
                 "purchased lot", "bought land", "have a lot", "own land",
                 "family land"]),
    ("need lot", ["need a lot", "looking for land", "don't have land",
                  "dont have land", "no lot", "find us a lot", "lot included",
                  "help finding land", "in your community", "in a neighborhood"]),
]


def extract_lot_status(text):
    low = text.lower()
    for label, keywords in LOT_KEYWORDS:
        if any(k in low for k in keywords):
            return label
    return None


TIMELINE_KEYWORDS = [
    ("ASAP", ["asap", "right away", "immediately", "urgent",
              "as soon as possible", "right now"]),
    ("within 3 months", ["this month", "within a month", "next few weeks",
                         "few weeks", "1-3 months", "couple of months"]),
    ("3-6 months", ["3-6 months", "three to six", "half a year",
                    "later this year", "this fall", "this winter"]),
    ("6-12 months", ["6-12 months", "six to twelve", "next year",
                     "within a year", "next spring", "next summer"]),
    ("1+ years / just researching", ["just looking", "researching", "browsing",
                                     "not sure yet", "exploring", "idea stage",
                                     "planning ahead", "someday", "dreaming",
                                     "over a year", "couple of years"]),
]


def extract_timeline(text):
    low = text.lower()
    for label, keywords in TIMELINE_KEYWORDS:
        if any(k in low for k in keywords):
            return label
    return None


def extract_budget(text):
    """Return a normalized budget string, or None.

    Handles "$450k", "$400k-$600k", "under $500k", "$1,200,000".
    """
    low = text.lower()
    # Range like "$400k-$600k" or "400-600k".
    m = re.search(r"\$?\s?(\d+)\s?[kK]?\s?[-–to]+\s?\$?\s?(\d+)\s?[kK]\b", low)
    if m:
        lo, hi = m.group(1), m.group(2)
        lo_s = f"${lo}k" if int(lo) < 1000 else f"${int(lo):,}"
        hi_s = f"${hi}k" if int(hi) < 1000 else f"${int(hi):,}"
        return f"{lo_s}-{hi_s}"
    # "under $500k" / "up to $750k" / "around $600k".
    m = re.search(r"\b(?:under|up to|less than|around|about|~)\s?\$?\s?(\d+)\s?[kK]\b",
                  low)
    if m:
        return f"~${m.group(1)}k"
    # Single "$450k" / "$1,200,000".
    m = re.search(r"\$\s?(\d{1,3}(?:,\d{3})+|\d+)\s?[kK]?\b", low)
    if m:
        raw = m.group(1).replace(",", "")
        if m.group(0).strip().endswith(("k", "K")):
            return f"${raw}k"
        val = int(raw)
        return f"${val}k" if val < 1000 else f"${val:,}"
    # Bare "500k".
    m = re.search(r"\b(\d+)\s?[kK]\b", low)
    if m:
        return f"${m.group(1)}k"
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
    "Hi there! \U0001F44B Welcome to Oakridge Custom Homes — I'm Casey, the "
    "virtual concierge. Are you dreaming of a custom build, or looking at "
    "one of our move-in-ready homes? (Free consultations + model tours!)"
)

OFF_TOPIC = (
    "I can help with new-home questions and scheduling a model home tour. "
    "What are you looking for in your next home?"
)


class Conversation:
    def __init__(self):
        self.stage = "problem"  # problem -> collect -> done
        self.question = None
        self.data = {
            "service": None,
            "name": None,
            "phone": None,
            "lot_status": None,
            "timeline": None,
            "budget": None,
        }
        self.turns = 0


def start_conversation():
    return Conversation(), GREETING


PRICE_GUIDE = (
    "Quick numbers: our custom builds typically run $225-$325 per sq ft "
    "depending on finishes (so a 2,500 sq ft home lands around $560k-$810k), "
    "and our move-in-ready spec homes start in the high $400s."
)

TIMELINE_GUIDE = (
    "Typical timelines: custom builds run 8-12 months from permits to keys, "
    "and our spec homes can close in 30-45 days."
)


def _ack_for(service, question):
    # Answer the question first, then name the build type.
    if question == "pricing question":
        tail = f" And for reference, {service}s: {PRICE_GUIDE[16:]}"
        return PRICE_GUIDE + (" " if service == "new construction" else tail)
    if question == "timeline question":
        return TIMELINE_GUIDE + " Want me to get you set up with a free consultation?"
    if question == "financing question":
        return ("Great question — most of our buyers use a one-time-close "
                "construction loan (one closing, converts to a mortgage at move-in). "
                "We work with several local lenders and can introduce you. "
                "Typical down payments run 10-20%.")
    if question == "model tour request":
        return ("We'd love to show you around! Our model home is open daily "
                "10am-6pm — no appointment needed, but I can set up a private "
                "tour with our design consultant if you'd like.")
    if question == "floor plan question":
        return ("We have 14 floor plans from 1,800 to 4,200 sq ft — 3 to 5 "
                "bedrooms, and every plan is fully customizable. Which size "
                "range are you thinking?")
    if question == "lot/land question":
        return ("We build on your lot OR ours — we have homesites in three "
                "Oakridge communities, plus we do free lot evaluations if you "
                "already own land.")
    acks = {
        "custom home build": ("A fully custom home — that's what we do best. "
                              "You'll work one-on-one with our design team from "
                              "first sketch to final walkthrough."),
        "spec home purchase": ("Smart move — our spec homes are designer-finished "
                               "and can close in 30-45 days. I'll get you the "
                               "current inventory."),
        "new construction": "Exciting! Building new is a big decision — happy to walk you through it.",
    }
    return acks.get(service, "Got it — I can help with that.")


def _next_missing_field(conv):
    """Name of the next field the bot needs, or None when ready to capture."""
    d = conv.data
    if not d["name"]:
        return "name"
    if not d["phone"]:
        return "phone"
    if not d["lot_status"]:
        return "lot_status"
    if not d["timeline"]:
        return "timeline"
    if not d["budget"]:
        return "budget"
    return None


def _next_prompt(conv):
    """Return the next question needed, or None when ready to capture."""
    prompts = {
        "phone": "And what's the best phone number to reach you at?",
        "name": "What's your name?",
        "lot_status": ("Do you already own a lot or land to build on, or would "
                       "you need one of our homesites?"),
        "timeline": ("When are you hoping to be in the new home — in the next "
                     "few months, 6-12 months out, or just researching for now?"),
        "budget": ("And what budget range are you thinking for the build? "
                   "(A ballpark like \"$500k-$700k\" is perfect.)"),
    }
    field = _next_missing_field(conv)
    if field == "phone":
        return "Thanks! What's the best phone number for our new-home consultant to reach you?"
    if field == "budget":
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
        "service": d["service"] or "new construction",
        "lot_status": d["lot_status"] or "not specified",
        "preferred_time": d["timeline"] or "not specified",
        "budget": d["budget"] or "not specified",
        "emergency": False,
    }
    return lead


def _confirmation(lead):
    lines = [
        "Wonderful — you're all set! Here's what I have:",
        f"\u2022 Name: {lead['name']}",
        f"\u2022 Phone: {lead['phone']}",
        f"\u2022 Interested in: {lead['service']}",
        f"\u2022 Lot: {lead['lot_status']}",
        f"\u2022 Timeline: {lead['preferred_time']}",
        f"\u2022 Budget: {lead['budget']}",
        "",
        "Next step: a FREE consultation + model home tour. Our new-home "
        f"consultant will call {lead['phone']} shortly to schedule it. "
        "Can't wait? Call (555) 042-8873 — model home open daily 10am-6pm. \U0001F3E1",
    ]
    return "\n".join(lines)


def respond(conv, user_text):
    """Process one user message.

    Returns (reply_text, lead_captured_bool, lead_dict_or_None).
    """
    text = (user_text or "").strip()
    conv.turns += 1

    if conv.stage == "done":
        return ("You're all set! Our consultant will be in touch shortly. "
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
    lot = extract_lot_status(text)
    if lot and not conv.data["lot_status"]:
        conv.data["lot_status"] = lot
    tm = extract_timeline(text)
    if tm and not conv.data["timeline"]:
        conv.data["timeline"] = tm
    budget = extract_budget(text)
    if budget and not conv.data["budget"]:
        conv.data["budget"] = budget

    # --- stage: figure out what they're after ---
    if conv.stage == "problem":
        service, question = classify(text)
        low = text.lower()
        if (service == "new construction" and question is None and any(
            w in low for w in ["just looking", "browsing", "hello", "hi"]
        ) and len(low.split()) <= 3):
            return (OFF_TOPIC, False, None)
        conv.data["service"] = service
        conv.question = question
        conv.stage = "collect"
        prompt = _next_prompt(conv)
        return (f"{_ack_for(service, question)} {prompt}", False, None)

    # --- stage: collect missing fields ---
    d = conv.data
    # Lot status: accept a direct answer when it's the next missing field.
    if (_next_missing_field(conv) == "lot_status"
            and len(text) > 1):
        low = text.lower()
        if any(w in low for w in ["own", "have", "yes", "my land", "already"]):
            d["lot_status"] = "own lot"
        elif any(w in low for w in ["need", "no", "don't", "dont", "looking",
                                    "yours", "community"]):
            d["lot_status"] = "need lot"
        elif len(text.split()) <= 6 and not any(ch.isdigit() for ch in text):
            d["lot_status"] = text.strip()
    # Name fallback: short digit-free reply while the name is the next
    # missing field (e.g. user just types "John").
    if (_next_missing_field(conv) == "name"
            and not any(ch.isdigit() for ch in text)
            and len(text.split()) <= 4
            and not extract_timeline(text)
            and not extract_budget(text)
            and not extract_lot_status(text)):
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
