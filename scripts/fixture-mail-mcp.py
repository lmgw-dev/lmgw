#!/usr/bin/env python3
# Usage: register as a stdio MCP server on a DEV copy: command `python3`, args `scripts/fixture-mail-mcp.py`,
#        tool_prefix `gws`, env FIXTURE_MAIL_LOG=<file> (JSONL of every tool call it saw);
#        `scripts/fixture-mail-mcp.py --dump-truth` prints id, category, long (JSONL) for labelling.
"""A fake stand-in for the Gmail workspace server the mail labeler names (gws).

Serves the five tools the mail labeler calls (gmail_search, gmail_get,
gmail_listLabels, gmail_createLabel, gmail_batchModify) over ~30 fake unread
mails in mixed categories: senders are `.example` hosts and RFC 5737 addresses
only, four newsletters are long enough (> 8,192 tokens) to pass a non-causal
decision model's ubatch, and one mail is gibberish that fits no category.
Every call is appended as one JSON line to $FIXTURE_MAIL_LOG (default
fixture-mail-calls.jsonl in the cwd), so a drive can check the label writes
`apply` made. Labels created are kept in memory only.
"""
import json
import os
import random
import sys

CATS = {
    "Newsletter": [
        ("digest@news.example", "Weekly digest: build systems and linkers"),
        ("editor@gazette.example", "The Gazette, issue 212: what changed this month"),
        ("hello@rustletter.example", "This week in Rust-like things"),
        ("team@devweekly.example", "Dev Weekly: five links worth your Friday"),
    ],
    "Promotions": [
        ("sale@shop.example", "48 hours only: 30% off everything"),
        ("deals@outlet.example", "Your coupon expires tonight"),
        ("offers@gearhub.example", "Members get early access to the autumn sale"),
        ("promo@books.example", "Buy two, get one free on all paperbacks"),
    ],
    "Notifications": [
        ("noreply@forge.example", "[repo] Pipeline #4821 failed on main"),
        ("alerts@monitor.example", "Disk usage on host-3 passed 85%"),
        ("noreply@calendar.example", "Reminder: dentist, Tuesday 09:30"),
    ],
    "Social": [
        ("notify@friends.example", "Anna commented on your photo"),
        ("updates@circle.example", "You have 3 new connection requests"),
    ],
    "Personal": [
        ("mia@family.example", "Sunday lunch at ours?"),
        ("jonas@mail.example", "Re: the hiking trip, dates"),
        ("grandpa@family.example", "Photos from the garden"),
    ],
    "Work": [
        ("lead@team.example", "Planning for the Q4 migration"),
        ("hr@company.example", "Reminder: submit your timesheet"),
        ("pm@projects.example", "Re: spec review notes"),
    ],
    "Finance": [
        ("statements@bank.example", "Your October statement is ready"),
        ("billing@insure.example", "Premium due on 2026-11-01"),
        ("tax@revenue.example", "Notice: advance payment schedule"),
    ],
    "Receipts": [
        ("orders@shop.example", "Your receipt for order #88213"),
        ("invoice@cloudhost.example", "Invoice 2026-10 for your hosting plan"),
    ],
    "Travel": [
        ("tickets@rail.example", "Your e-ticket, Berlin to Hamburg"),
        ("booking@stays.example", "Booking confirmed: 2 nights in Lisbon"),
    ],
    "Spam": [
        ("prize@198.51.100.7", "You have won a prize, claim now"),
        ("support@203.0.113.9", "Account locked, verify immediately"),
    ],
}
SAMPLE = {
    "Newsletter": "Welcome to this issue. Below you find the articles of the week, a reader question, and the links we liked. Unsubscribe at the bottom.",
    "Promotions": "Do not miss our biggest sale of the season. Use the code at checkout. Offer ends soon, while stocks last.",
    "Notifications": "This is an automated message. No reply is needed. See the details in the dashboard linked below.",
    "Social": "Someone interacted with your profile. Open the app to see what they said and to respond.",
    "Personal": "Hi, hope you are well. Just wanted to ask about the weekend and whether the plan still stands. Talk soon.",
    "Work": "Hi all, following up on the agenda for next week. Please review the document and send comments before Thursday.",
    "Finance": "Your account activity for the period is attached. The balance and the due amount are shown on the first page.",
    "Receipts": "Thank you for your purchase. Your receipt and the order summary are below. Keep this message for your records.",
    "Travel": "Your itinerary is confirmed. Departure and arrival times, the booking reference and the check-in rules follow.",
    "Spam": "Dear friend, you have been selected. Send your details and a small fee to the address below to receive the funds.",
}
WORDS = ("compiler linker cache symbol object archive module release kernel scheduler queue buffer socket "
         "thread future runtime allocator profile benchmark regression patch review merge branch commit "
         "tracing metrics gateway proxy tokenizer prompt context window decoder encoder attention latency").split()


def long_body(seed):
    r = random.Random(seed)
    # varied words with numbers so a tokenizer cannot compress them
    return SAMPLE["Newsletter"] + "\n\n" + " ".join(r.choice(WORDS) + str(r.randrange(10, 99)) for _ in range(9000))


def build():
    mails = []
    n = 0
    for cat, senders in CATS.items():
        for sender, subject in senders:
            n += 1
            long = cat == "Newsletter"  # all four newsletters are long
            body = long_body(n) if long else SAMPLE[cat] + f"\n\nRef {1000 + n}."
            mails.append({"id": f"fx{n:02d}", "from": sender, "to": "owner@home.example",
                          "subject": subject, "date": f"Fri, 16 Oct 2026 {n % 24:02d}:{(n * 7) % 60:02d}:00 +0000",
                          "body": body, "category": cat, "long": long})
    # one that fits no category: should land on the fallback
    n += 1
    mails.append({"id": f"fx{n:02d}", "from": "x7q@198.51.100.44", "to": "owner@home.example",
                  "subject": "zqx vrt 0xA9", "date": "Fri, 16 Oct 2026 03:03:00 +0000",
                  "body": "vrt qzx plm 77 ::: kk kk kk ~~ ) ( zz", "category": "Other", "long": False})
    random.Random(7).shuffle(mails)
    return mails


MAILS = build()
BY_ID = {m["id"]: m for m in MAILS}
LABELS = [{"id": "INBOX", "name": "INBOX"}, {"id": "UNREAD", "name": "UNREAD"}]
LOG = os.environ.get("FIXTURE_MAIL_LOG", "fixture-mail-calls.jsonl")


def tool(name):
    return {"name": name, "description": name, "inputSchema": {"type": "object", "properties": {}}}


TOOLS = [tool(n) for n in ("gmail_search", "gmail_get", "gmail_listLabels", "gmail_createLabel", "gmail_batchModify")]


def call(name, args):
    with open(LOG, "a") as f:
        f.write(json.dumps({"tool": name, "args": args}) + "\n")
    if name == "gmail_search":
        k = int(args.get("maxResults") or 50)
        return {"messages": [{"id": m["id"], "threadId": m["id"]} for m in MAILS[:k]]}
    if name == "gmail_get":
        m = BY_ID[args["messageId"]]
        return {k: m[k] for k in ("id", "from", "to", "subject", "date", "body")}
    if name == "gmail_listLabels":
        return {"labels": LABELS}
    if name == "gmail_createLabel":
        lab = {"id": f"Label_{len(LABELS)}", "name": args["name"]}
        LABELS.append(lab)
        return lab
    if name == "gmail_batchModify":
        return {"ok": True}
    raise KeyError(name)


def main():
    if "--dump-truth" in sys.argv:
        for m in MAILS:
            print(json.dumps({"id": m["id"], "category": m["category"], "long": m["long"]}))
        return
    for line in sys.stdin:
        if not line.strip():
            continue
        req = json.loads(line)
        rid, method = req.get("id"), req.get("method")
        if rid is None:
            continue
        if method == "initialize":
            res = {"protocolVersion": req["params"].get("protocolVersion", "2025-06-18"),
                   "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture-mail", "version": "0.1.0"}}
        elif method == "tools/list":
            res = {"tools": TOOLS}
        elif method == "tools/call":
            try:
                s = call(req["params"]["name"], req["params"].get("arguments") or {})
                res = {"content": [{"type": "text", "text": json.dumps(s)}], "structuredContent": s, "isError": False}
            except Exception as e:  # noqa: BLE001
                res = {"content": [{"type": "text", "text": f"error: {e}"}], "isError": True}
        else:
            res = {}
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": res}) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
