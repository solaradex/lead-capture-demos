"""Rule-based AI receptionist for the ShieldGuard Pest Control demo.

No external AI API is needed. A small state machine classifies the
customer's intent (bed bugs / wasp nest emergencies vs. routine pests),
then steers the conversation toward capturing: name, phone, address,
service needed, and preferred time.

The closer: push the FREE inspection — most services start with one.

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
# NOTE: explicit urgency words checked first, then pricing — "how much is bed
# bug treatment" is a pricing question first, not an emergency dispatch.
INTENTS = [
    ("urgent pest emergency", ["emergency", "urgent", "asap", "right now",
                               "immediately"], True),
    ("pricing question", ["price", "cost", "how much", "quote", "estimate",
                          "charge", "rate", "free"], False),
    ("bed bug treatment", ["bed bug", "bedbug", "bed bugs"], True),
    ("wasp/hornet nest removal", ["wasp", "hornet", "yellow jacket",
                                  "yellowjacket", "nest"], True),
    ("termite control", ["termite"], False),
    ("rodent control", ["mouse", "mice", "rat", "rats", "rodent"], False),
    ("wildlife removal", ["raccoon", "squirrel", "opossum", "bat ", "attic",
                          "wildlife", "skunk", "chipmunk"], False),
    ("mosquito control", ["mosquito", "mosquitoes", "mosquitos"], False),
    ("ant/roach control", ["ant", "ants", "roach", "cockroach", "roaches",
                           "spider", "spiders"], False),
]


def classify(text):
    """Return (service_label, is_emergency). Falls back to general pest control."""
    low = text.lower()
    for label, keywords, emergency in INTENTS:
        if any(k in low for k in keywords):
            return label, emergency
    return "general pest control", False


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
    "Hi there! \U0001F44B Thanks for visiting ShieldGuard Pest Control — "
    "I'm Sam, the virtual receptionist. What pest problem can I help you "
    "with today? (Inspections are always free!)"
)

OFF_TOPIC = (
    "I can help with pest questions and scheduling a technician. "
    "What pest issue are you dealing with?"
)


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


def _ack_for(service, emergency):
    if emergency:
        if service == "bed bug treatment":
            return ("Ugh, bed bugs — I know how stressful that is. The good news "
                    "is we handle these every single day, and we'll start with "
                    "a FREE inspection.")
        if service == "wasp/hornet nest removal":
            return ("Don't go near it — wasp nests are dangerous to handle "
                    "yourself. We'll take care of it, usually same-day.")
        return ("That sounds urgent — I'll get a technician on it right away.")
    acks = {
        "termite control": ("Smart to get that checked — termite damage only gets "
                            "worse, and our inspections are FREE."),
        "rodent control": ("Got it — mice and rats. We'll find how they're getting "
                           "in and shut it down."),
        "wildlife removal": ("Got it — wildlife in the home. We'll start with a free "
                             "inspection, then give you an exact quote."),
        "mosquito control": ("Nothing ruins a yard like mosquitoes — we can fix that "
                             "fast."),
        "ant/roach control": ("Ants and roaches are our bread and butter — our quarterly "
                              "plan ($39/mo) keeps them gone for good."),
        "pricing question": ("Here's the quick price guide: general pest plan $39/mo, "
                             "termite inspections FREE (treatment from $499), bed bug "
                             "treatment from $299/room, mosquito $59/visit, wasp nest "
                             "removal $149. Wildlife removal starts with a free "
                             "inspection, then a custom quote."),
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
        "address": ("What's the service address? We'll start with a free "
                    "inspection."),
        "phone": "And what's the best phone number to reach you at?",
        "name": "What's your name?",
        "time": "When works best for the visit — today, or tomorrow morning or afternoon?",
    }
    # Non-emergency phone prompt is friendlier on first ask.
    field = _next_missing_field(conv)
    if field == "phone" and not conv.emergency:
        return "Thanks! What's the best phone number for our technician to reach you?"
    return prompts.get(field)


def _capture(conv):
    d = conv.data
    lead = {
        "id": f"lead-{datetime.now(timezone.utc).strftime('%Y%m%d%H%M%S')}",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "name": d["name"] or "Not provided",
        "phone": d["phone"] or "Not provided",
        "service": d["service"] or "general pest control",
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
        lines.append("\U0001F6A8 A technician will call "
                     f"{lead['phone']} within minutes to schedule your free "
                     "inspection.")
    else:
        lines.append("We'll start with a FREE inspection — no obligation. A "
                     f"technician will call {lead['phone']} shortly to confirm. "
                     "Need us sooner? Call (555) 014-7788. \U0001F41C")
    return "\n".join(lines)


def respond(conv, user_text):
    """Process one user message.

    Returns (reply_text, lead_captured_bool, lead_dict_or_None).
    """
    text = (user_text or "").strip()
    conv.turns += 1

    if conv.stage == "done":
        return ("You're all set! A technician will be in touch shortly. "
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
        if service == "general pest control" and any(
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
