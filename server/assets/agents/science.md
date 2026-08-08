# RSVP removes eye movement. It does not remove attention.

*The honest page. This is the markdown rendering of {{BASE}}/science — same
content, no layout.*

We build a speed reader, so treat this page with the suspicion it deserves —
and then check the citations at the bottom, which is why they are there. Here
is what the research supports, what it does not, and what we do about the part
that counts against us.

## What RSVP actually does

Ordinary reading is not a smooth glide along a line. Your eyes jump — a saccade
— land, fixate for a fifth of a second or so, jump again, and at the end of
every line perform a return sweep back to the left margin. You are blind during
the jumps. A meaningful share of reading time is spent moving rather than
reading.

Rapid Serial Visual Presentation deletes that overhead by putting every word in
the same place, one after another. There is nothing to track and nothing to
sweep back to. flick goes one step further and aligns each word on its optimal
recognition point — the pivot letter, in the accent colour — so the eye does
not drift within the word either.

That is the whole mechanism. It is real, it is measurable, and it is worth a
great deal. It is also the *only* thing being claimed here.

## What it does not do

**"You can read 1,000+ wpm with full comprehension."**
No. Comprehension — inferential comprehension especially — degrades as
presentation rate climbs. The effect is well replicated. Anyone selling you
four figures is selling you skimming under another name.

**"Speed reading trains your peripheral vision to take whole lines."**
No. The fovea — the part of your retina with the acuity to resolve letters —
covers about two degrees of visual angle. That is a handful of characters. No
amount of practice enlarges it.

**"Subvocalisation is a bad habit you can eliminate."**
Mostly no. The inner voice is bound up with comprehension, not merely a relic
of learning to read. RSVP does reduce how much of it you need for
line-tracking; it does not switch it off, and switching it off is not obviously
desirable.

**"RSVP eliminates wasted eye movement."**
Yes — this one is true, and it is the entire mechanism. Saccades and the return
sweep at the end of each line are real overhead, and putting every word in the
same spot removes them.

## The strongest argument against us

Readers regress. Roughly one eye movement in ten goes backwards, and many of
those are not motor error — they are repair. You reach the end of a clause,
discover you parsed it wrong, and your eyes go back to the word that misled
you. Schotter, Tran and Rayner (2014) blocked exactly that, using a trailing
mask so each word could be read only once, and comprehension suffered. Rayner
and colleagues (2016) built the broader case: reading speed is limited by
language processing, not by eye movements, and RSVP tools in particular take
away the reader's ability to reread intelligently.

> A naive RSVP stream is a text you cannot argue with. That is a real cost, and
> no amount of product design makes it disappear.

The literature is not unanimous — more recent work argues lexical processing
and comprehension hold up better at elevated rates than the 2016 review
allowed, and the debate is live. But the direction of the evidence is not in
our favour, and you should hear that from us rather than from the top comment
on Hacker News.

## What we do about it

You cannot design regressions back into a stream that has already passed. You
can make going back cheap, and you can stop the stream from outrunning the
reader in the first place. That is most of what the engine is:

- **Rewind by word or sentence.** The repair move regressions exist for, one
  key away, without losing your place.
- **Speed you change mid-passage.** Changing wpm is computed on your device
  from the same timeline, so it takes effect on the next word — no round trip,
  no reason not to slow down for three paragraphs.
- **Per-word timing, not a metronome.** Rare words linger, common ones fly,
  long words split, punctuation and paragraph ends get wrap-up pauses. The
  stream already slows where reading naturally slows.
- **A second pass that costs almost nothing.** A chapter at 450 wpm takes long
  enough to be worth rereading at 600. Repetition is the cheapest comprehension
  tool there is, and speed is what makes it affordable.

## What speed to actually use

| Speed | For | Note |
|---|---|---|
| 600–800 wpm | Familiar prose, news, the newsletter backlog | Text you could have skimmed anyway |
| 400–500 wpm | General reading, most non-fiction, novels | Where most practised readers settle |
| 250–350 wpm | Technical writing, papers, anything you will be asked about | Plan a second pass |
| Don't | Poetry, contracts, code, anything you must reread to parse | Use your eyes; that is what regressions are for |

The average adult reads prose at about 238 wpm (Brysbaert, 2019, pooling 190
studies). Doubling that is an ordinary outcome. Tripling it happens on easy
text. If a passage needs rereading to make sense, RSVP is the wrong tool for
that passage — and a speed reader that will not tell you so is not worth
trusting with the rest.

## Sources

1. Rayner, Schotter, Masson, Potter & Treiman (2016). *So Much to Read, So
   Little Time: How Do We Read, and Can Speed Reading Help?* Psychological
   Science in the Public Interest 17(1).
   <https://journals.sagepub.com/doi/10.1177/1529100615623267>
2. Schotter, Tran & Rayner (2014). *Don't Believe What You Read (Only Once):
   Comprehension Is Supported by Regressions During Reading.* Psychological
   Science 25(6). <https://journals.sagepub.com/doi/10.1177/0956797614531148>
3. Brysbaert (2019). *How many words do we read per minute? A review and
   meta-analysis of reading rate.* Journal of Memory and Language 109.
   <https://www.sciencedirect.com/science/article/pii/S0749596X19300786>

---

Try it, no account: <{{BASE}}/>
