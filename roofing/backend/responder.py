"""Rule-based AI receptionist for the Summit Ridge Roofing demo.

No external AI API is needed. A small state machine classifies the
customer's intent (storm damage / active leak emergencies vs. replacement
quotes vs. insurance questions), then steers the conversation toward
capturing: name, phone, address, issue, and preferred time.

The closer: the FREE drone roof inspection — every flow ends with one.
The insurance angle is woven in: "we handle the insurance paperwork"
is the roofer's #1 sales lever.

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

# (service_label, keywords, is_emergency)
# NOTE: explicit urgency words checked first, then pricing — "how much is a
# new roof" is a pricing question first, not an emergency dispatch.
INTENTS = [
    ("urgent roofing emergency", ["emergency", "urgent", "asap", "right now",
                                  "immediately", "pouring", "flooding",
                                  "water coming in"], True),
    ("pricing question", ["price", "cost", "how much", "quote", "estimate",
                          "charge", "rate", "free"], False),
    ("storm/hail damage", ["storm", "hail", "wind damage", "hurricane",
                            "tornado", "tree fell", "tree on"], True),
    ("active roof leak", ["leak", "leaking", "drip", "dripping", "stain",
                          "water spot", "ceiling"], True),
    ("roof replacement", ["replace", "replacement", "new roof", "reroof",
                          "re-roof", "old roof", "aging", "20 years",
                          "30 years"], False),
    ("roof repair", ["repair", "shingle", "shingles", "missing", "blown off",
                     "curling", "cracked", "flashing"], False),
    ("insurance claim", ["insurance", "claim", "adjuster", "covered",
                         "deductible"], False),
    ("gutter installation", ["gutter", "gutters", "downspout"], False),
]


def classify(text):
    """Return (service_label, is_emergency). Falls back to general roofing."""
    low = text.lower()
    for label, keywords, emergency in INTENTS:
        if any(k in low for k in keywords):
            return label, emergency
    return "general roofing inquiry", False


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


TIME_KEYWORDS = [
    ("today", ["today", "this afternoon", "this morning", "this evening"]),
    ("tomorrow morning", ["tomorrow morning"]),
    ("tomorrow afternoon", ["tomorrow afternoon"]),
    ("tomorrow", ["tomorrow"]),
    ("this morning", ["this morning", "morning"]),
    ("this afternoon", ["afternoon"]),
    ("this evening", ["evening", "tonight"]),
    ("as soon as possible", ["as soon as possible", "asap", "right away",
                             "right now", "immediately", "urgent"]),
    ("anytime", ["anytime", "whenever", "flexible"]),
    ("this weekend", ["weekend", "saturday", "sunday"]),
    ("next week", ["next week", "monday", "tuesday", "wednesday", "thursday",
                   "friday"]),
]


def extract_time(text):
    low = text.lower()
    # Specific clock times like "3pm" or "10:30am".
    m = re.search(r"\b(\d{1,2}(?::\d{2})?\s?(?:am|pm))\b", low)
    if m:
        return m.group(1)
    for label, keywords in TIME_KEYWORDS:
        if any(k in low for k in keywords):
            return label
    return None


# ---------------------------------------------------------------------------
# Conversation state machine
# ---------------------------------------------------------------------------

GREETING = (
    "Hi there! \U0001F44B Thanks for visiting Summit Ridge Roofing — "
    "I'm Jake, the virtual receptionist. What can I help you with today? "
    "Roof leak, storm damage, or a replacement quote? (Drone inspections are always free!)"
)

OFF_TOPIC = (
    "I can help with roofing questions and scheduling an inspection. "
    "What's going on with your roof?"
)

PRICE_GUIDE = (
    "Here's the quick guide: roof repairs $300\u2013$1,500, full asphalt "
    "replacement $8k\u2013$15k, metal roofing $15k\u2013$30k, emergency "
    "tarping $399, gutters $1,200\u2013$2,500. Storm damage inspections are "
    "FREE — and if insurance covers it, we handle the whole claim for you."
)


def _ack_for(service, emergency):
    if emergency:
        if service == "storm/hail damage":
            return ("That storm hit a lot of homes — you're not alone. The "
                    "important thing is getting eyes on it fast: we'll do a "
                    "FREE drone inspection and check for hail/wind damage. "
                    "If insurance should cover it, we handle the entire claim "
                    "for you.")
        if service == "active roof leak":
            return ("A leak can't wait — water damage gets expensive fast. "
                    "We can get a crew out to tarp it TODAY so nothing else "
                    "gets damaged, then handle the full repair.")
        return ("That sounds urgent — I'll get a crew on it right away. We can "
                "tarp it today to stop any further damage.")
    acks = {
        "roof replacement": ("Smart to plan ahead — a new roof is the best "
                             "investment you can make in your home. Asphalt "
                             "runs $8k\u2013$15k, metal $15k\u2013$30k, and we "
                             "start every project with a FREE drone inspection "
                             "so you see exactly what you need."),
        "roof repair": ("Got it — repairs run $300\u2013$1,500 depending on "
                        "the damage. Our free drone inspection will pinpoint "
                        "exactly what's needed, no guesswork."),
        "insurance claim": ("Good news: this is what we do every day. We "
                            "document everything for your insurer, meet the "
                            "adjuster on-site, and handle the paperwork — "
                            "most of our storm jobs end up costing the "
                            "homeowner just their deductible."),
        "gutter installation": ("Got it — gutters run $1,200\u2013$2,500 "
                                "installed, and we can do them with the roof "
                                "or on their own."),
        "pricing question": PRICE_GUIDE,
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
    else:
        if not d["name"]:
            return "name"
        if not d["phone"]:
            return "phone"
        if not d["address"]:
            return "address"
        if not d["time"]:
            return "time"
    return None


def _next_prompt(conv):
    """Return the next question needed, or None when ready to capture."""
    prompts = {
        "address": ("What's the property address? We'll start with a free "
                    "drone inspection."),
        "phone": "And what's the best phone number to reach you at?",
        "name": "What's your name?",
        "time": "When works best for the inspection — today, or tomorrow morning or afternoon?",
    }
    field = _next_missing_field(conv)
    if field == "phone" and not conv.emergency:
        return "Thanks! What's the best phone number for our project manager to reach you?"
    return prompts.get(field)


def _capture(conv):
    d = conv.data
    lead = {
        "id": f"lead-{datetime.now(timezone.utc).strftime('%Y%m%d%H%M%S')}",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "name": d["name"] or "Not provided",
        "phone": d["phone"] or "Not provided",
        "service": d["service"] or "general roofing inquiry",
        "address": d["address"] or "Not provided",
        "preferred_time": d["time"] or "as soon as possible",
        "emergency": conv.emergency,
    }
    return lead


def _confirmation(lead):
    lines = [
        "You're all booked in! Here's what I have:",
        f"\u2022 Name: {lead['name']}",
        f"\u2022 Phone: {lead['phone']}",
        f"\u2022 Issue: {lead['service']}",
    ]
    if lead["address"] != "Not provided":
        lines.append(f"\u2022 Address: {lead['address']}")
    lines.append(f"\u2022 Preferred time: {lead['preferred_time']}")
    lines.append("")
    if lead["emergency"]:
        lines.append("\U0001F6A8 A project manager will call "
                     f"{lead['phone']} within minutes. We'll tarp it today "
                     "to stop further damage, then schedule the free drone "
                     "inspection.")
    else:
        lines.append("We'll start with a FREE drone inspection — you'll get "
                     "photos of your whole roof and an exact quote, no "
                     f"obligation. A project manager will call {lead['phone']} "
                     "shortly to confirm. Storm damage? Ask us about the "
                     "insurance claim — we handle the paperwork. \U0001F3E0")
    return "\n".join(lines)


def respond(conv, user_text):
    """Process one user message.

    Returns (reply_text, lead_captured_bool, lead_dict_or_None).
    """
    text = (user_text or "").strip()
    conv.turns += 1

    if conv.stage == "done":
        return ("You're all set! A project manager will be in touch shortly. "
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
    tm = extract_time(text)
    if tm and not conv.data["time"]:
        conv.data["time"] = tm

    # --- stage: figure out the problem ---
    if conv.stage == "problem":
        service, emergency = classify(text)
        # "just looking" / greetings with no real problem -> redirect politely.
        low = text.lower()
        if service == "general roofing inquiry" and any(
            w in low for w in ["just looking", "browsing", "hello", "hi"]
        ) and len(low.split()) <= 3:
            return (OFF_TOPIC, False, None)
        conv.data["service"] = service
        conv.emergency = emergency
        conv.stage = "collect"
        prompt = _next_prompt(conv)
        return (f"{_ack_for(service, emergency)} {prompt}", False, None)

    # --- stage: collect missing fields ---
    d = conv.data
    # Address: accept any substantive non-phone text as the address when it's
    # the next missing field.
    if (_next_missing_field(conv) == "address" and not phone
            and len(text) > 4):
        d["address"] = text
    # Name fallback: short digit-free reply while the name is the next
    # missing field (e.g. user just types "John").
    if (_next_missing_field(conv) == "name"
            and not any(ch.isdigit() for ch in text)
            and len(text.split()) <= 4
            and not extract_time(text)):
        d["name"] = text.strip().title()

    prompt = _next_prompt(conv)
    if prompt is None:
        lead = _capture(conv)
        conv.stage = "done"
        return (_confirmation(lead), True, lead)
    return (prompt, False, None)


class Conversation:
    def __init__(self):
        self.stage = "problem"  # problem -> collect -> done
        self.emergency = False
        self.data = {
            "service": None,
            "name": None,
            "phone": None,
            "address": None,
            "time": None,
        }
        self.turns = 0


def start_conversation():
    return Conversation(), GREETING
