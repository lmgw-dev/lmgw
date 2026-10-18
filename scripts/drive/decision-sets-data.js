// Fixtures for scripts/drive/decision-sets.json (labs-store design §7.3):
// 30 made-up support tickets with a ticket-id key and a label for each of
// Ticket triage's questions (team: an option's name; urgency: a level's
// index, 0 "can wait a week" to 3 "right now"; needs_human: yes or no).
// Fake data only: .example names and RFC 5737 addresses.
(() => {
  const rows = [
    ["T-1001", "I was charged twice for order A-1001 this month. Please refund one of the two payments.", "billing", "2", "yes"],
    ["T-1002", "Where can I download the invoice for my March subscription?", "billing", "0", "no"],
    ["T-1003", "My card was declined, but the money left my account anyway.", "billing", "2", "yes"],
    ["T-1004", "Please change the billing address on my invoices to 12 Example Road.", "billing", "0", "no"],
    ["T-1005", "You renewed my plan although I cancelled it last week. I want my money back.", "billing", "2", "yes"],
    ["T-1006", "Is there a discount for paying yearly instead of monthly?", "billing", "0", "no"],
    ["T-1007", "The refund you promised two weeks ago never arrived.", "billing", "1", "yes"],
    ["T-1008", "Can I get a receipt with my company VAT number on it?", "billing", "0", "no"],
    ["T-1009", "The app crashes every time I open the settings page.", "technical", "1", "yes"],
    ["T-1010", "Your API answers 500 for every request from 203.0.113.7 since this morning.", "technical", "3", "yes"],
    ["T-1011", "The export to CSV button does nothing in Firefox.", "technical", "1", "yes"],
    ["T-1012", "Our whole team cannot load the dashboard, it is down for everyone right now.", "technical", "3", "yes"],
    ["T-1013", "How do I turn on dark mode in the mobile app?", "technical", "0", "no"],
    ["T-1014", "Uploads over 10 MB fail with a timeout error.", "technical", "1", "yes"],
    ["T-1015", "The calendar sync stopped working after the last update.", "technical", "2", "yes"],
    ["T-1016", "Where do I find the list of keyboard shortcuts?", "technical", "0", "no"],
    ["T-1017", "I cannot sign in: the password reset email never arrives at jane@shop.example.", "account", "2", "yes"],
    ["T-1018", "How do I change the email address on my profile?", "account", "0", "no"],
    ["T-1019", "Someone signed into my account from 198.51.100.23. Please lock it now!", "account", "3", "yes"],
    ["T-1020", "Please delete my account and all of my data.", "account", "1", "yes"],
    ["T-1021", "How can I add a second user to our team account?", "account", "0", "no"],
    ["T-1022", "My two-factor codes are rejected and I am locked out.", "account", "2", "yes"],
    ["T-1023", "Can I change my username?", "account", "0", "no"],
    ["T-1024", "I want to hand the workspace over to bob@corp.example.", "account", "1", "yes"],
    ["T-1025", "Do you have an office in Berlin?", "other", "0", "no"],
    ["T-1026", "I just wanted to say that the new design looks great.", "other", "0", "no"],
    ["T-1027", "Are you hiring support staff this year?", "other", "0", "no"],
    ["T-1028", "Can you send me your press kit?", "other", "0", "no"],
    ["T-1029", "What are your opening hours on public holidays?", "other", "0", "no"],
    ["T-1030", "Is your company planning a user conference in spring?", "other", "0", "no"],
  ];
  const field = (v) => (/[",\n\r]/.test(v) ? '"' + v.replace(/"/g, '""') + '"' : v);
  const csv = (rs) =>
    ["ticket_id,text,team,urgency,needs_human"]
      .concat(rs.map((r) => r.map(field).join(",")))
      .join("\r\n") + "\r\n";
  window.__tickets = csv(rows);
  // An unknown label: row 3 (line 4) says "Billing", which is no option.
  window.__ticketsBadLabel = csv(rows.map((r, i) => (i === 2 ? [r[0], r[1], "Billing", r[3], r[4]] : r)));
  // A score column mixing a level's text with indices: urgency at row 5 is
  // "today" (level 2), every other row an index.
  window.__ticketsMixed = csv(rows.map((r, i) => (i === 4 ? [r[0], r[1], r[2], "today", r[4]] : r)));
  window.__ticketRows = rows;
  // One unlabelled item whose state is about 6,000 tokens: Laya takes tens
  // of seconds over it, so a call stays in flight long enough for a second
  // Cancel to abort it.
  const long = rows.map((r) => r[1]).join(" ").repeat(12);
  window.__ticketsLong = JSON.stringify({ ticket_id: "T-2001", text: long }) + "\n";
  // Two items: an unlabelled short one, and a labelled one whose state is
  // past Laya's ubatch (8,192 tokens), which llama.cpp answers 500. The run
  // has a label but no labelled answer: a threshold measures nothing.
  const tooLong = rows.map((r) => r[1]).join(" ").repeat(24);
  window.__ticketsFail =
    JSON.stringify({ ticket_id: "T-3001", text: "Where can I download my invoice?" }) + "\n" +
    JSON.stringify({ ticket_id: "T-3002", text: tooLong, team: "billing" }) + "\n";
  // 120 items, so the items table reads more than one page.
  const many = [];
  for (let k = 0; k < 4; k++) {
    for (const r of rows) many.push(["T-" + (4001 + many.length), r[1], r[2], r[3], r[4]]);
  }
  window.__tickets120 = csv(many);
  // Put a file into the New set run's file input, as a pick would.
  window.__pickFile = (text, name) => {
    const input = document.querySelector(".dl-newrun input[type=file]");
    if (!input) return "no file input";
    const dt = new DataTransfer();
    dt.items.add(new File([text], name, { type: "text/csv" }));
    input.files = dt.files;
    input.dispatchEvent(new Event("change", { bubbles: true }));
    return "picked " + name;
  };
  // Hold the answer to the page's next POST to a path ending in `suffix`
  // until the jobs feed no longer lists the job keyed `key`, and `after` ms
  // more: the order a loaded machine gives at random when a Cancel ends the
  // job in milliseconds (an abort, the last call in flight landing), the
  // job's end and the page's re-read of the run before the Cancel's answer,
  // which was built while the job still ran. window.__held says where it is.
  window.__holdAnswer = (suffix, key, after = 400) => {
    const f = window.fetch;
    window.__held = "armed";
    window.fetch = function (input, init) {
      const url = typeof input === "string" ? input : input.url;
      const method = ((init && init.method) || (input && input.method) || "GET").toUpperCase();
      if (method !== "POST" || !url.endsWith(suffix)) return f.apply(this, arguments);
      window.fetch = f;
      window.__held = "holding";
      const es = window.__eventSources.filter((e) => e.readyState !== 2).at(-1);
      const gone = new Promise((done) => {
        const on = (e) => {
          if (!JSON.parse(e.data).some((j) => j.key === key)) {
            es.removeEventListener("jobs", on);
            window.__held = "job gone";
            done();
          }
        };
        es.addEventListener("jobs", on);
        setTimeout(done, 20000);
      });
      return f.apply(this, arguments).then((r) =>
        gone
          .then(() => new Promise((ok) => setTimeout(ok, after)))
          .then(() => {
            window.__held = "released after: " + window.__held;
            return r;
          }),
      );
    };
    return "holding the next POST …" + suffix + " until " + key + " leaves the feed";
  };
  // Type into a textarea or input the way an input event says.
  window.__typeInto = (sel, text) => {
    const el = document.querySelector(sel);
    if (!el) return "no " + sel;
    el.value = text;
    el.dispatchEvent(new Event("input", { bubbles: true }));
    return "typed " + text.length;
  };
  return "fixtures ready";
})();
