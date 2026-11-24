# Ideas

These are ideas for later. They are not planned work and nobody is building them yet. Each says
where it came from and what it would help with. When an idea is taken up, it moves into a design
in `docs/design/` and its entry here points there.

## Offer the GPU hold when outside memory jumps

*From the device-ledger work, 2026-11-17.*

On a desktop, the GPU hold is what keeps a game from running out of graphics memory. It stops
lmgw's models and makes them answer with their fallback. The catch is that it has to be switched
on before the game starts. A game takes its memory in one large block, and its needs differ from
game to game and even from level to level. So headroom and reacting after the fact can soften this
but cannot prevent it.

lmgw already reads each card's memory and knows which part belongs to other programs. When that
part jumps by a large amount within a few seconds, lmgw could offer the hold: a notice in the web UI
(and on the desktop, where a tray or notification is available) saying "Another program just took
6 GiB on card 0. Hold the GPU while it runs?" with one click to switch it on. lmgw would never switch
the hold on by itself, because it cannot tell a game from a short spike.

Open points:

- **What counts as a jump:** how many GiB within how many seconds, and per card or for the whole
  machine. The per-second log of a four-card test server (2026-11-17) helps tell real jumps from the
  usual drift.
- **How to stop asking:** when the user says no, lmgw does not ask again for the same program until
  it stops.
- **Ending the hold:** whether lmgw also offers to end the hold when that memory is released again.

## A document-conversion API in lmgw

*From the owner, 2026-11-17.*

lmgw already converts files of several types into text for LLM use, for chat attachments and its
knowledge bases. On a server where another program does that job for other tools, lmgw could offer
the conversion as an API of its own. Those tools could then use lmgw, and the other program and its
share of the graphics card could go.

Open points: which formats and which output (plain text, Markdown, structured pages); whether it
runs on the CPU or needs a model on the card; and how it fits lmgw's accounts and keys.

## Put the draft model on another card when only that fits

*From the device-ledger work, 2026-11-17. Not decided yet whether it is worth it; so far
the only use seen is when graphics memory is short.*

With speculative decoding, llama.cpp takes turns: the draft model proposes a few tokens, the main
model checks them in one batch, and the draft model goes again. They do not run at the same time,
so a second card adds no parallel work. A faster card for the draft model would save little,
because a draft model is small and its share of each token's time is small. (Believed true of
llama-server today; not checked against the newest builds.)

The case where it helps is short memory: the main model fits on card A alone but not together with
its draft model's weights and cache, while card B has room. Today lmgw charges the draft model to
the first card. The pair then does not fit, so the row waits, or its main model is split over both
cards and every token crosses between them. With the draft model on card B, the main model stays
whole on one card and only the few draft tokens per step cross. The cost is the engine's own share
on card B (about 0.5 GiB for a second CUDA context).

If it is built, lmgw would decide it itself rather than through a pin: "main model on one card, its
draft model on another" as a placement step before splitting the main model, with the start's
warning saying where the draft model went. llama-server is told through `-devd` with the second
card's number as the container sees it. A user would have no reason to choose that card by hand.

Open points: where it goes in the placement order (before a split of the main model, and whether
before evicting idle models); and whether the engine-cost figures learned per card cover a second
context that holds only a draft model.
