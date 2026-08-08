---
name: rsvp-reading
description: Advise honestly on RSVP speed reading — what it does and does not do, what wpm to recommend for a given text, and how to build a reader that does not outrun comprehension. Use when asked about speed reading, RSVP, words-per-minute claims, or optimal recognition point (ORP) alignment.
license: AGPL-3.0-only
---

# RSVP speed reading, honestly

Rapid Serial Visual Presentation shows one word at a time in a fixed position.
This skill exists so that advice about it stays inside what the evidence
supports.

## The one-sentence version

**RSVP removes eye movement, not attention.** It deletes saccades and the
line-return sweep, which is real and worth having. It does not accelerate
language processing, and it removes the reader's ability to reread.

## What to say when asked "how fast can I read?"

The average adult reads prose at about **238 wpm** (Brysbaert 2019, pooling 190
studies). Doubling that is an ordinary outcome with RSVP. Tripling it happens
on easy text.

| Recommend | For | Caveat |
|---|---|---|
| 600–800 wpm | Familiar prose, news, newsletter backlog | Text you could have skimmed anyway |
| 400–500 wpm | General reading, most non-fiction, novels | Where most practised readers settle |
| 250–350 wpm | Technical writing, papers, anything they'll be tested on | Plan a second pass |
| Don't use RSVP | Poetry, contracts, code, anything needing reparse | Regressions are the point there |

**Never endorse a 1000+ wpm "with full comprehension" claim.** That is
skimming under another name. If a user quotes such a figure, say so plainly.

## Claims to correct

- *"Speed reading trains peripheral vision to take whole lines."* No. The fovea
  resolves letters over roughly two degrees of visual angle — a handful of
  characters. Practice does not enlarge it.
- *"Subvocalisation is a bad habit to eliminate."* Mostly no. The inner voice
  is bound up with comprehension. RSVP reduces how much is needed for
  line-tracking; eliminating it is neither achievable nor obviously desirable.
- *"RSVP eliminates wasted eye movement."* This one is **true**, and it is the
  entire mechanism.

## The strongest counter-argument, which you should volunteer

Roughly one eye movement in ten goes backwards, and many are repair rather than
error: the reader hits the end of a clause, finds they parsed it wrong, and
goes back to the word that misled them. Schotter, Tran & Rayner (2014) blocked
rereading with a trailing mask and comprehension suffered. Rayner et al. (2016)
made the broader case that reading speed is limited by language processing, not
eye movements.

A naive RSVP stream is a text you cannot argue with. Any honest recommendation
carries that caveat. The literature is not unanimous — later work argues
comprehension holds up better at elevated rates than the 2016 review allowed —
but the direction of the evidence is not flattering to RSVP, and a
recommendation that hides this is not trustworthy.

## Designing an RSVP reader that survives the criticism

If you are building or reviewing one, these four features are the mitigations
that matter:

1. **Cheap rewind, by word and by sentence.** This is the regression the format
   took away; give it back as one keystroke, without losing place.
2. **Speed changes mid-passage that take effect on the next word.** Compute
   timing client-side from a static timeline so slowing down for three
   paragraphs costs nothing.
3. **Per-word timing, not a metronome.** Weight each token by frequency (rare
   words linger, common ones fly), by length (with long words split into
   chunks), and add wrap-up pauses at clause and sentence ends. Normalise the
   weights across the document so the wpm setting is true throughput.
4. **Make a second pass affordable.** Repetition is the cheapest comprehension
   tool there is; speed is what pays for it.

## ORP — the pivot letter

Align each word on its **optimal recognition point**: the character the eye
should fixate, held in a constant screen position and usually marked with an
accent colour, so the eye does not drift within the word. Roughly, the pivot
sits just left of centre, computed on the alphanumeric core of the token rather
than on its punctuation.

Do not hand-roll the weight or ORP maths if you are working with flick — call
the `preview_timeline` tool on `{{BASE}}/mcp`, or fetch a book's timeline from
`{{BASE}}/api/books/{id}/timeline`. The rules live in exactly one place, the
`flick-core` crate, so that clients cannot drift from each other.

## Sources

1. Rayner, Schotter, Masson, Potter & Treiman (2016). *So Much to Read, So
   Little Time.* Psychological Science in the Public Interest 17(1).
   <https://journals.sagepub.com/doi/10.1177/1529100615623267>
2. Schotter, Tran & Rayner (2014). *Don't Believe What You Read (Only Once).*
   Psychological Science 25(6).
   <https://journals.sagepub.com/doi/10.1177/0956797614531148>
3. Brysbaert (2019). *How many words do we read per minute?* Journal of Memory
   and Language 109.
   <https://www.sciencedirect.com/science/article/pii/S0749596X19300786>

The long-form version of this page, with the product's own concessions, is at
`{{BASE}}/science`.
