---
title: "Design panel runbook: adversarial plan authoring for one phase or slice"
kind: prompt
corpus: blackbox-prompts
audience: interactive-orchestrator
topic:
  - prompts
  - design-review
brief: "Operator-pointed runbook for producing a reviewed implementation plan for ONE decomposition phase or slice: an author-critic default shape, a three-author panel escalation, an adjudicated repair loop, and an independent review bookend. Prototype-stage; not yet a bbox workflow."
---

# Design panel runbook

You are a live interactive orchestrator. The operator has pointed you at
this file with a subject: one decomposition phase or slice that needs an
implementation plan (or a design document of comparable weight). This
runbook produces a reviewed, PASS-certified plan for that ONE subject.
It is deliberately phase-scoped: if the operator wants several subjects
run at once, they will tell you to fan out, and the fan-out unit is a
child orchestrator (an in-harness subagent per subject, each pointed at
this runbook) coordinated by you. Do not fan out on your own initiative.

Status: prototype. This shape has two full panel runs and one controlled
author-critic comparison behind it (recorded in the bbox thread
`design-panel-prototype`). The operator is iterating on it before any
promotion to a bbox workflow. Record timings, token costs, finding
counts, and process observations to the tracking thread as you go; they
are the iteration record.

## Choosing the shape

**Default: AUTHOR-CRITIC.** One strong author writes the complete plan;
two adversarial critics attack it without ever authoring an alternative;
you adjudicate on evidence; the author repairs in its own session. In the
comparison run this matched or beat panel first-draft quality at roughly
half the cost.

**Escalate to the THREE-AUTHOR PANEL when the subject has a genuinely
open design fork**: multiple plausible architectures, a contested scope
decision, or a domain where you cannot predict the shape of the right
answer. The panel's unique value is three independently derived
positions; blind authoring is what makes the later critique sharp. Its
cost is roughly double, and majority opinion among authors is worthless
(see hard rules), so buy the panel only for the independence.

The shape choice above assumes you can rank the pool ex ante (the
prototype regime: one clearly dominant author). When you cannot, use the
casting option below instead.

Provider profile observed across runs (revisit as models move): brodex
(gpt-5.6-sol, xhigh) is the strongest author, slowest and costliest,
differentially right on hard design forks; deepseek (deepseek-v4-pro,
xhigh) is fast and cheap, weak on verification as an author but sharp as
a critic when forced to quote lines; glm (glm-5.2, xhigh) is the best
balance and a reliable repair/consolidation executor. Kimi (k3 via the
`claudew` review path) is the independent review bookend, never a
participant. In the panel shape, pick the consolidation author per round
on merit (whose final position matched the adjudication and whose
corrections held), never by habit.

## Option two: casting by role requirements, with an audition (peer pools)

Use this option when the available pool is peer-class at the top (for
example several frontier models of comparable tier) and "who is
strongest" is not knowable in advance. In that regime, per-subject
variance among peers typically exceeds their mean differences, so the
right question is not "which model is best" but "what does each seat
actually need". Status: designed from the prototype's data but NOT yet
exercised end to end; treat the first run under this option as an
iteration experiment and record it to the thread.

Three of the four seats are SATISFICING seats: define the requirement
floor and the independence constraints, then fill each with the cheapest
qualifying model.

- **Reviewer (bookend):** frontier-tier consequence-walking (re-deriving
  state machines, reproducing findings from failure sequences; a mid
  model rubber-stamps this work). Structural requirements: independence
  from the authoring pool, no stake in the deliberation, the fixed lens
  and exact-PASS bar, and preferably a different prior family than the
  author, since shared-prior blind spots are the one thing role
  structure cannot fix. No specific model is magic in this seat.
- **Adjudicator/orchestrator:** frontier tier, tool-loop discipline, the
  longest context in the system, and one hard constraint: it must not
  have authored anything it rules on. The prototype's one adjudication
  failure was procedural (a skipped caller-path walk by a
  frontier-class agent), not a capability gap, which is why firsthand
  verification is a gate in this runbook rather than a trusted
  property of the seat. The live interactive agent is the natural
  occupant for structural reasons: operator trust, accumulated context,
  the tool loop.
- **Critics:** the floor is genuinely lower, because the role's
  structure substitutes for capability: quote-the-line, the scored
  verified-sound list, and mandatory caller-path walks turned the
  prototype's weakest author into a sharp critic. Requirements:
  adversarial reading under that scaffolding, plus pairwise prior
  diversity and diversity from the author.

The AUTHOR is the one MAXIMIZING seat: its output is the quality ceiling
of the whole pipeline, and it is the only seat where per-subject peer
variance matters and cannot be substituted by structure. Fill it with
the audition:

1. Each candidate blind-writes a SKETCH only: the scope decision, the
   fixed-decisions section, and the milestone spine, roughly 100 lines,
   grounded to the same brief and verification rules as a full draft.
   Cost is a small fraction of a full blind round.
2. Adjudicate the sketches on evidence quality, exactly as you would
   critiques: grounding of claims, quality of rejected alternatives,
   scope judgment against the governing text. The winner authors the
   full plan.
3. The audition pays for itself twice more. Losers become PRE-GROUNDED
   critics: they already read the governing sources, so their critique
   pass is warmer, cheaper, and sharper than a cold critic's. And the
   audition doubles as the FORK-OPENNESS DETECTOR: if the sketches
   converge on one spine, proceed author-critic with the winner; if
   they diverge structurally, you have discovered you are in panel
   territory, and the sketches are already the seeds of a blind round.
   The shape decision stops being an advance judgment call and becomes
   something the first stage measures.

In the known-dominant regime the audition collapses to zero and this
option degenerates to the default shape, which makes option one a
special case rather than a competing philosophy. Voting never becomes
valid in peer pools: frontier models train on overlapping distributions,
so two peers confidently agreeing on an unverified claim is still shared
prior, not evidence. All adjudication and grounding rules apply
unchanged.

## Workspace and tracking

Create `.bbox/local/design-panel/<subject>/` (gitignored) with `blind/`
or `author/`, `critique/` (panel: `cross/`), and `final/`. Open or
continue a bbox work thread and record every dispatch `{taskId,
sessionId}` immediately; resume-based continuity is load-bearing and the
handles must survive your session. The repo working tree is multi-tenant:
every dispatched agent gets told it may create or modify exactly one
assigned file and must treat everything else as read-only, with committed
HEAD as evidence authority. No dispatched agent runs cargo or mutates
git.

## The brief

Author `brief.md` yourself before any dispatch. It must fix:

- the subject scope VERBATIM from its governing source, with the
  substance-section anchors, and which scope decisions the author may fix
  as plan-stated decisions;
- the baseline contracts (which documents are fixed, which are
  provisional-but-binding with flag-if-changed, which prior related
  proposals must be explicitly superseded/consumed/deferred per section);
- the exemplar document whose shape and discipline the plan must match;
- the verification rules below, verbatim in force.

Non-negotiable brief rules (each one earned by a concrete defect in the
prototype runs):

1. Every cited identifier is verified against source at authoring time:
   Decision Ledger numbers, error codes, functions, types, enum
   variants, CLI flags, config keys, file paths, cross-document section
   numbers. Fabrication migrates to whatever identifier kind is not
   policed, so police all of them.
2. Cross-document section citations quote a phrase from the cited
   section, so renumbering is detectable.
3. Absence claims ("X never happens", "lock-free", "no caller does Y")
   require a caller-path walk, stated. A module-scoped grep once ratified
   a falsehood through three independent passes.
4. New identifiers the plan invents are marked plan-defined.
5. No em dashes. A line target with self-compaction expected; overshoot
   without compaction is a low-care signal.

## Stage mechanics

**Authoring (both shapes).** Dispatch with explicit pins
(`pin_provider`/`pin_model`/`pin_effort`), `cwd` at repo root,
`durable=true`. Panel shape: three authors in parallel, blind (nobody
reads the panel directory beyond the brief and their own file). Babysit
with long-poll waits; check `bro_status(tail=N)` before ever declaring a
task dead. An empty output directory means nothing: authors write their
single file at the END of a 10-40 minute grounding-and-writing run.

**Critique.** Critics are resumed panel authors (panel shape, context
continuity via `bro_resume`) or fresh cheap dispatches (author-critic
shape). Blind to each other. Attack-only in the author-critic shape: no
alternative designs. Required structure: findings severity-ordered, each
QUOTING the exact target line plus the disproving evidence; a scored
verified-sound list (claims checked that held, with the file:symbol
evidence) so skimming is distinguishable from verification; an honest
verification log. Panel shape adds: disputes stated fairly with a
verdict, concessions (each quoting the text that changed the panelist's
mind), adoptions, and a final position.

**Adjudication (you, firsthand).** Never merge by tally or self-report:
across the prototype runs, majority vote among authors was wrong on most
material forks, and every stage boundary leaked a defect class only
orchestrator verification caught. Re-verify contested code claims by
reading the code yourself before ruling. Score concession grounding, not
concession count. In the panel shape, write a PRESERVE section listing
uncontested substantive mechanics: consensus content is exactly what
consolidation silently drops, because no ruling defends it. Write the
rulings as a numbered binding list with an explicit corrections section.

**Repair.** Always `bro_resume` the original author or consolidation
session; never a fresh dispatch. Repairs get the SAME firsthand
verification standard as original authoring: four defects in the
prototype runs were introduced BY repairs, written under narrower
context. Require the repairer to re-read every edited region in full and
to report a content hash of the finished document; verify the hash and
key changes yourself before proceeding.

**Independent review bookend.** For documents in the durable project
catalog family, use the frozen `scripts/kimi-review.sh plan-review` /
`plan-resume` flow unchanged. For any other subject, author a one-off
subject-scoped lens (adapt `prompts/agents/kimi-plan-review.md`: name the
document path, its governing sources, the fixed-contract posture toward
certified plans including the surgical-amendment requirement, and the
verification discipline including caller-path walks and per-document
line-count reporting) and invoke `claudew` directly with the same flags,
env, and read-only tool allowlist the script uses. Always: capture and
record the session id and pass it EXPLICITLY on every resume (the
recorded-session pointer file is shared state and concurrent orchestrators
clobber it); pin the document's content hash in every resume prompt so
the reviewer certifies identifiable text; iterate repair-then-resume in
the same reviewer session until exact PASS. Reviewer policy questions
are resolved as plan-stated decisions per precedent, stated visibly so
the operator can override; zero open questions is the PASS posture.

**Landing.** After PASS: copy the certified document to its canonical
`design/` path (byte-identical, hash-verified), add the hub index entry,
and commit with explicit paths only. Once text is hash-certified, the
bar for touching it rises: cosmetic residuals batch into the next
substantive pass rather than triggering re-certification.

## Fan-out (operator-directed only)

When the operator asks for several subjects at once: spawn one child
orchestrator per subject (in-harness subagent, e.g. Opus-class), each
pointed at this runbook with its subject and baselines, each noting
progress to the shared thread under a subject prefix. You then own the
cross-subject coordination, which the prototype showed is where the real
hazards live:

- Reviews of a shared document family race peer repairs and produce
  phantom-open findings against already-fixed text. Quiesce peer edits
  during family-wide certification rounds, or at minimum require every
  reviewer to report the line count or hash of each document it read so
  stale reads self-identify.
- Findings against a peer lane's document are ROUTED, never repaired in
  place: the owning lane repairs, the reporting lane collects. State
  ownership explicitly when a finding crosses lanes.
- Readiness handshakes between lanes must be a hash or a phrase the
  sender has actually grepped; an unverified "grep for X" hint cost an
  hour of false holding in the prototype.
- Deliberately overlap reviewer scopes across lanes. Both HIGH-class
  late catches in the prototype were invisible to every single-lane
  participant and reviewer, and were caught only by a reviewer whose
  scope crossed lanes. A per-lane PASS is not family coherence; finish a
  shared family with one certification round over the whole set.

## Why the hard rules are hard

The prototype's failure record, compressed: panels of models share
priors, so consensus regresses to the shared prior rather than the
truth; grounding every claim in quoted code or governing text is the
only decorrelator. The orchestrator's firsthand verification is not a
formality on top of a good panel: it is the only thing standing between
a confident consensus and a shipped falsehood. Treat every rule in this
file as load-bearing until the operator's further iterations retire it.
