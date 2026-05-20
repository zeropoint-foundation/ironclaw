# Addendum — Skill manifest parse frontmatter design

> **Superseded 2026-05-17.** Contents folded into the main design at
> `skill-manifest-parse-frontmatter-design-2026-05.md` as §4
> (Dispatch ordering and logging), §5.5–§5.7 (new tests), and §8
> (Open questions resolved). Read the main design only; this file is
> kept as a record of the addendum-as-separate-doc shape and the
> intermediate decision to consolidate.

*2026-05-17. Target: terminal Claude working in `~/projects/ironclaw`.
Amend the existing design at
`docs/handoffs/skill-manifest-parse-frontmatter-design-2026-05.md`
with three specifications that came out of the open-questions
discussion. Add as new sections; don't remove existing content.*

## Goal

Make the gate's semantics in the broader dispatch pipeline
unambiguous. The parser change is straightforward; what's been left
implicit is *when* gates run relative to other dispatch filters, what
counts as "explicit invocation" in v2, and which log events fire at
which level. Document these so future readers adding a new gate, a
new filter, or a new dispatch path have a clear reference.

## Three sections to add

### Section 1: Dispatch ordering

Add to the design near the existing enforcement-points section.

Specification:

**Filter gates run before resource/scoring filters, which run before
dispatch.** This ordering is invariant under any reasonable definition
of resource enforcement.

Canonical sequence:

```
candidates
  → gate_filter   (setup_marker, disable-model-invocation, user-invocable)
  → score
  → top_N / enforce_limits
  → dispatch
```

Why this ordering: gates answer "is this skill eligible at all?" If a
gated skill is in the top-N before filtering, you can end up
dispatching fewer skills than expected (or zero), because the gate is
removing eligible candidates post-selection. Filtering first means the
top-N is computed over invokable candidates only, and the dispatch
slot is always full and viable.

Verify by reading `enforce_limits` (locate via
`rg -n "enforce_limits|limit_skills|top_n" ~/projects/ironclaw/crates`)
and confirming today's sequence matches. If it doesn't, the
implementation should reorder — gate filters must precede scoring/
limit filters.

State the principle explicitly: **a new gate added to the pipeline
inserts at the gate_filter stage, not the scoring stage.** Future
authors adding gates inherit this discipline.

### Section 2: V2 explicit-invocation semantics

Add to the design near the existing v2 + user_invocable section.

Specification:

**V2 uses the same syntactic definition of "explicit invocation" as
v1: literal `/skill-name` substring in the user message triggers
explicit-invocation routing that bypasses gates. Anything else is
auto-fire and respects gates.**

Rationale: the gate's value is precisely *not relying on intent
inference*. If "use the commit skill" counts as explicit, the gate
becomes fuzzy — its protection moves with the orchestrator's intent
classifier. Tight today is recoverable; loose today is hard to
tighten without breaking workflows that came to rely on the loose
behavior.

Path forward: v2's orchestrator should preserve the syntactic
explicit-invoke surface mirrored from v1 (`extract_skill_mentions`
equivalent). When v2 grows a natural-language explicit-invoke (e.g.,
"please use the commit skill"), that becomes a separate decision —
not a default, an explicit relaxation of the syntactic line.

Test plan addition: a v2 test that confirms "use the commit skill"
(natural language) auto-fires through the gate, while `/commit`
(syntactic) bypasses. Both behaviors should be intentional.

### Section 3: Log event breakdown

Add to the design as a new section (no existing logging section to
amend).

Four distinct events deserve distinct treatment:

| Event | When | Level | Format | Audience |
|-------|------|-------|--------|----------|
| **Skill loaded with field set** | At startup, once per skill | INFO | `skill_loaded: <name>, disable_model_invocation=true` (or user_invocable=false) | Deployment-posture audit — grep-able snapshot of gate configuration on this machine |
| **Skill filtered during auto-fire** | Per dispatch decision when gate triggers | DEBUG | `skill_skipped: <name>, reason=disable_model_invocation` | Operator debugging "why didn't this fire?" — visible when needed, quiet otherwise |
| **Skill explicitly invoked despite gate** | Per invocation when `/skill-name` bypasses gate | DEBUG | `skill_explicit_invoke: <name>, gate_bypassed=true` | Post-hoc audit — "how often is this gate being bypassed?" Useful for tuning. |
| **Menu filtered N skills** | At `/menu` render | DEBUG, aggregated | `menu_filtered: N user_invocable_false skills hidden` | Single line per render, not one per skill — keeps menu rendering low-noise |

Principle: **one-shot config audit at INFO (deployment posture
visible); per-event filtering at DEBUG (no normal-operations noise).**

Match `setup_marker`'s existing log level for consistency on
filtering events (locate via
`rg -n "setup_marker.*log\|log.*setup_marker"`). If `setup_marker`
filters silently, the new gates should also filter silently — easier
to learn one convention than two.

Optional consideration to surface (not a decision for this brief):
the **skill_explicit_invoke** event is in the gray zone for whether
it should be a structured receipt in the audit chain rather than a
log line. A user explicitly bypassing a side-effect gate (e.g.,
explicitly invoking `commit` even though `disable-model-invocation:
true` is set) IS a moment where the operator overrode a substrate
guard. If the chain should record that override for accountability,
the event wants to be a signed receipt, not a log. Flag for design
review; default is log-only unless there's a specific accountability
requirement.

## Acceptance criteria

After the addendum lands:

1. **Dispatch ordering section** exists and names the canonical
   sequence (gate → score → top_N → dispatch) with explicit guidance
   for future gate authors.
2. **V2 explicit-invocation section** specifies syntactic-only
   bypass, mirroring v1, with the test plan extended to cover the
   natural-language vs syntactic distinction.
3. **Log events section** documents the four events with levels,
   formats, and audiences, and references the `setup_marker`
   consistency check.
4. **The three open questions originally flagged** (log level, v2
   explicit-invocation parity, enforce_limits) are now closed by
   reference to the new sections.

## Out of scope

- Implementing the parser change. That comes after this addendum
  lands and the design is unambiguous.
- The "signed receipt vs log line" decision for explicit-bypass
  events — flagged for future design review, not for this round.
- Audit chain integration of any kind. Today's gates emit log lines
  only.

## Refs

- `docs/handoffs/skill-manifest-parse-frontmatter-investigation-2026-05.md` — original brief
- `docs/handoffs/skill-manifest-parse-frontmatter-design-2026-05.md` — design this amends
- `docs/skill-audit-2026-05.md` — the audit this work enables
