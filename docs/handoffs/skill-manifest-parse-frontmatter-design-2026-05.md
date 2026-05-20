# Design — SkillManifest frontmatter parsing for visibility fields

*2026-05-17. Design output for the investigation at
`docs/handoffs/skill-manifest-parse-frontmatter-investigation-2026-05.md`.
This document proposes the schema change, names every enforcement
point in concrete file/function terms, specifies precedence against
`setup_marker`, and lays out the test plan. No code in this round;
implementation follows once the design is approved.*

## TL;DR

Two new top-level fields on `SkillManifest` — `disable-model-invocation`
and `user-invocable`. Auto-fire is gated in one place (`prefilter_skills`
on the v1 path; `handle_list_skills` on the v2 path). Menu visibility is
gated in two places (`/skills list` CLI handler and the web
`skills_list_handler`). Explicit `/skill-name` invocation is never
gated — explicit operator intent overrides menu visibility.
`setup_marker` keeps its precedence as the engine-enforced exclusion,
and the new gates layer on top without conflict. Total surface: one
struct change, four enforcement insertions, ~ten new tests covering
schema parsing, selector behavior, menu visibility, v2 path,
dispatch ordering invariant, and log emission levels. Dispatch
ordering (gate → score → top_N → dispatch), v2 explicit-invocation
semantics (syntactic only), and log event taxonomy (INFO at startup,
DEBUG per dispatch) are all specified in §4.

## 1. Schema change

### 1.1 Field definitions

Add two fields to `SkillManifest` in
`crates/ironclaw_skills/src/types.rs` (insertion point: after
`requires: GatingRequirements` at line 163, before the closing brace
at line 164):

```rust
/// Parsed skill manifest from SKILL.md YAML frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub activation: ActivationCriteria,
    #[serde(default)]
    pub credentials: Vec<SkillCredentialSpec>,
    #[serde(default)]
    pub requires: GatingRequirements,

    /// When `true`, the model cannot auto-fire this skill on context
    /// match. The skill is only loaded when the operator explicitly
    /// invokes it with `/skill-name`. Defaults to `false` (auto-fire
    /// allowed), preserving today's behavior for skills that omit
    /// the field.
    ///
    /// Use this for skills whose action has side effects the operator
    /// should always intend explicitly (e.g. `commit`, where an
    /// auto-fire could commit unintended changes).
    #[serde(default, rename = "disable-model-invocation")]
    pub disable_model_invocation: bool,

    /// When `false`, the skill is hidden from operator-facing menus
    /// (`/skills list`, the web skill list API). The skill is still
    /// loadable when another skill references it via `requires.skills`
    /// or when the operator explicitly invokes it. Defaults to `true`
    /// (visible in menus), preserving today's behavior.
    ///
    /// Use this for methodology/reference skills that should not
    /// clutter the operator menu but exist as composable knowledge
    /// for other skills to chain-load.
    #[serde(default = "default_user_invocable", rename = "user-invocable")]
    pub user_invocable: bool,
}

fn default_user_invocable() -> bool {
    true
}
```

### 1.2 Why top-level (not nested under `activation`)

`setup_marker` lives under `activation` because it gates *activation
scoring*. The two new fields are different in shape:

- `disable_model_invocation` gates *all* auto-firing, regardless of
  score. It is a property of the skill as a whole.
- `user_invocable` gates *menu visibility*. It is metadata about how
  the skill is discovered, not how it is scored.

Putting them at top level matches the Anthropic SKILL.md convention
(which the audit doc tracks) and makes them easier to reason about
without nesting them inside an activation-scoring container they have
no semantic relationship to.

### 1.3 Migration story (transparent)

- `#[serde(default)]` on `disable_model_invocation` → absent field
  deserializes as `false` (auto-fire allowed).
- `#[serde(default = "default_user_invocable")]` on `user_invocable`
  → absent field deserializes as `true` (visible in menus).

Every SKILL.md that loads today loads after the change with identical
behavior. No migration step, no transitional config. The two skills
that *already* declare these fields in their frontmatter
(`skills/commit/SKILL.md` declares `disable-model-invocation: true`)
become enforcing the moment this lands.

### 1.4 V2 metadata path

In v2 (`crates/ironclaw_engine/src/executor/orchestrator.rs::handle_list_skills`,
line 2506) skills are read from `MemoryDoc.metadata` JSON, not from
parsed `SkillManifest`. The current code reads
`metadata.activation.setup_marker` directly from the JSON Value blob.

The new top-level fields will be serialized into `MemoryDoc.metadata`
the same way the existing fields are (via the SKILL.md → MemoryDoc
conversion that already runs through `serde_json::to_value(&manifest)`
or equivalent). The v2 enforcement gate (§2.4 below) reads them out
of the JSON the same way `setup_marker` is read today. No
additional plumbing.

**Pre-implementation verification (load-bearing):** confirm the
SKILL.md → MemoryDoc conversion path before writing §5.4's v2 test.
If conversion uses `serde_json::to_value(&manifest)` (full-struct
serialization), the new fields auto-appear in `metadata` and the
assumption holds. If conversion is field-by-field (struct destructure
→ manual JSON assembly), the new fields will NOT appear until they
are explicitly included — the v2 test would pass trivially (field
never reaches metadata, so the gate never sees a `true` value to
filter on) while v2 enforcement is silently broken. Locate the
conversion site via `rg -n "to_value.*manifest|MemoryDoc.*from.*Skill"
~/projects/ironclaw/crates`. If field-by-field, add the new fields
to the assembly as part of this patch.

### 1.5 No `enforce_limits` work needed

The new fields are booleans with no per-skill sanitization required
(no length caps, no path traversal, no validity range). They do not
need entries in `ActivationCriteria::enforce_limits()` or a new
sanitizer on `SkillManifest`.

## 2. Enforcement points

Each gate is a focused insertion at an existing decision site —
not a refactor. The four sites are:

### 2.1 V1 auto-fire gate — `disable_model_invocation`

**File:** `crates/ironclaw_skills/src/selector.rs`
**Function:** `prefilter_skills` (line 157)
**Insertion:** inside the `filter_map` at line 178, alongside the
existing `setup_marker` check at lines 182–186.

```rust
let mut scored: Vec<ScoredSkill<'a>> = available_skills
    .iter()
    .filter_map(|skill| {
        // Setup-marker exclusion (existing).
        if let Some(marker) = &skill.manifest.activation.setup_marker
            && satisfied_setup_markers.contains(marker)
        {
            return None;
        }
        // NEW: disable-model-invocation exclusion. Skill author has
        // declared this skill must not auto-fire on context match.
        // It can still be force-activated via /skill-name.
        if skill.manifest.disable_model_invocation {
            return None;
        }
        let score = score_skill(skill, &message_lower, message);
        if score > 0 {
            Some(ScoredSkill { skill, score })
        } else {
            None
        }
    })
    .collect();
```

The `try_select` path at line 90 (used for chain-loaded companions)
must NOT check `disable_model_invocation`. Chain-load is initiated
by another skill that explicitly references this one in
`requires.skills` — that *is* an explicit reference, not a context
match. Same logic as why `user_invocable: false` doesn't block
chain-load: the field gates auto-fire on the operator's message,
not loadability as a dependency.

This is the only enforcement point needed for auto-fire on v1.
`extract_skill_mentions` (the `/skill-name` explicit path,
`selector.rs:350`) runs *before* `prefilter_skills` in
`agent_loop.rs:752–763` and feeds its results through a separate
merge step. Explicit invocations pass through unchecked, which is
what the investigation specifies: explicit operator intent overrides
menu/auto-fire gates.

### 2.2 V1 menu gate — `user_invocable` (CLI surface)

**File:** `src/agent/commands.rs`
**Function:** `handle_skills_list` (line 929)
**Insertion:** filter the iterator at line 952 before formatting:

```rust
let visible: Vec<_> = skills
    .iter()
    .filter(|s| s.manifest.user_invocable)
    .collect();

if visible.is_empty() {
    return Ok(SubmissionResult::response(
        "No skills installed.\n\nUse /skills search <query> to find skills on ClawHub.",
    ));
}

let mut out = String::from("Installed skills:\n\n");
for s in &visible {
    // ... existing formatting unchanged
}
```

Note that the empty-state branch must also consider the post-filter
list (a skill list containing only reference-only skills should
display as empty rather than emitting blank lines under the header).

### 2.3 V1 menu gate — `user_invocable` (web surface)

**File:** `src/channels/web/handlers/skills.rs`
**Function:** `skills_list_handler` (line 89)
**Insertion:** filter `skill_snapshot` at line 105 before mapping to
`SkillInfo`:

```rust
let skill_snapshot = {
    let guard = registry.read().map_err(/* ... */)?;
    guard.skills().to_vec()
};

let visible: Vec<_> = skill_snapshot
    .into_iter()
    .filter(|s| s.manifest.user_invocable)
    .collect();

let skills: Vec<SkillInfo> = join_all(visible.into_iter().map(skill_info)).await;
```

The neighboring `skills_search_handler` at line 114 should NOT be
filtered. Search is a deliberate query for a named skill, which is
explicit user intent — same semantic as `/skill-name`. Operators
should be able to find reference-only skills if they search for them
specifically.

**Note on `disable_model_invocation` and search:** neither gate
applies to `skills_search_handler`. The principle is that gate
applicability depends on what each surface *does*, not on
field-by-field symmetry. Search displays results for an explicit
operator query; it does not auto-invoke skills. Neither
`user_invocable: false` (menu visibility) nor
`disable_model_invocation: true` (auto-fire) is the right gate for
search. A future reader noticing the `user_invocable` exemption
should not add a `disable_model_invocation` filter "for symmetry" —
search is a display surface, and gates that protect dispatch don't
apply.

### 2.4 V2 auto-fire gate — both fields

**File:** `crates/ironclaw_engine/src/executor/orchestrator.rs`
**Function:** `handle_list_skills` (line 2506)
**Insertion:** extend the existing `.filter(|d| { ... })` block at
lines 2552–2573 (the setup-marker filter) with checks for the new
fields.

```rust
.filter(|d| {
    // Setup-marker exclusion (existing).
    let marker = d.metadata.get("activation")
        .and_then(|a| a.get("setup_marker"))
        .and_then(|m| m.as_str());
    if let Some(m) = marker {
        if existing_titles.contains(m) {
            debug!(skill = %d.title, marker = %m,
                "__list_skills__: excluding setup skill — marker already present");
            return false;
        }
    }
    // NEW: disable-model-invocation exclusion.
    let disabled = d.metadata.get("disable-model-invocation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if disabled {
        debug!(skill = %d.title,
            "__list_skills__: excluding skill — disable-model-invocation");
        return false;
    }
    true
})
```

**On v2 and `user_invocable`:** v2's `__list_skills__` is consumed by
the Python orchestrator's scoring loop — it is the auto-fire
candidate set, not an operator-facing menu. The v2 path does not
expose a `/skills list` analogue to the operator today; the visible
menu surfaces are all on v1. So in v2, the auto-fire gate is the
*only* gate that needs to fire, and `user_invocable: false` becomes
relevant only when v2 grows a menu surface.

Treatment: in v2 today, filter on `disable_model_invocation` only.
`user_invocable` is propagated into `MemoryDoc.metadata` and surfaces
in any future v2 menu, but does not gate auto-fire on v2.

**This is intentional, not deferred work.** The two fields gate two
different surfaces with orthogonal semantics:

- `disable_model_invocation` = "this skill must not auto-fire on
  context match." Applies to any dispatch path the model uses to
  pick skills autonomously.
- `user_invocable` = "this skill is not directly operator-facing."
  Applies to any *menu* the operator browses for invocation.

A skill can be one without the other. A reference-only methodology
skill (`user_invocable: false`) can still validly auto-fire when
another skill chain-loads it or when context matches its
description — it just shouldn't clutter the operator's menu. A
side-effect skill (`disable_model_invocation: true`) should still
appear in the menu so operators can find and invoke it explicitly
when they want.

The v2 auto-fire path therefore only gates on
`disable_model_invocation`. When v2 grows a menu surface, that
surface adds a `user_invocable` filter — same shape as v1's
`/skills list` and web handler, not extending the auto-fire gate.

If v2 grows an explicit-invocation path later (analogous to v1's
`extract_skill_mentions`), that path will need to bypass the
`disable_model_invocation` filter in `handle_list_skills` — likely
by adding an opt-in argument to `__list_skills__` that the Python
orchestrator passes when resolving an explicit `/skill-name`. Out
of scope for this work; tracked for future v2 parity.

## 3. Composition with `setup_marker`

### 3.1 Precedence rule

`setup_marker` is the engine-enforced exclusion that already
short-circuits scoring on v1 and document listing on v2. The new
fields layer on top and *also* short-circuit; in the v1 case all
three are evaluated in the same `filter_map` block (§2.1), so they
are effectively a logical OR — a skill is excluded if **any** of
the three predicates fire:

1. `setup_marker` exists and is satisfied in workspace
2. `disable_model_invocation == true`
3. (v2 only, future) `user_invocable == false` AND the call is from
   a menu surface

The investigation states a numerical priority (setup_marker
highest, etc.) — that ordering matters only for *attribution* in
debug logs ("which check excluded this skill?"), not for the
semantics. The semantics are short-circuit OR.

### 3.2 Why this is safe

The three fields gate three orthogonal concerns:

- `setup_marker` → "this skill has finished its one-time setup"
- `disable_model_invocation` → "this skill cannot auto-fire"
- `user_invocable` → "this skill is not directly operator-facing"

A skill with all three set is excluded from auto-fire AND from
menus AND its setup is already done — which is a coherent state
(a setup skill that has run, that never auto-fires, that isn't in
the menu — i.e. a tombstone). No combination produces contradiction.

### 3.3 Debug logging

Each gate emits a debug log identifying which check fired. When
multiple checks would fire for the same skill, the v1 `filter_map`
short-circuits on the first one (setup_marker → disable → score).
This is fine for diagnostics: operators rarely need to know "this
skill was excluded by THREE conditions"; one is sufficient.

## 4. Dispatch ordering and logging

These three subsections close the three open questions originally
flagged at the bottom of this doc and make the gate's semantics in
the broader dispatch pipeline unambiguous.

### 4.1 Dispatch ordering

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

The current v1 design satisfies this: in `prefilter_skills`
(`crates/ironclaw_skills/src/selector.rs:178`), the `filter_map`
short-circuits on all three gates (`setup_marker`,
`disable_model_invocation`, score>0) before the post-filter sort/take
that enforces the score-based top-N. The gate inserts at the start of
the same `filter_map`, which means it runs *before* scoring computes
on the surviving candidates — correct ordering by construction.

The v2 design also satisfies this: `handle_list_skills`
(`crates/ironclaw_engine/src/executor/orchestrator.rs:2552`) filters
candidates via a `.filter(|d| { ... })` block *before* the result is
serialized for the orchestrator's scoring step.

**Principle for future authors:** a new gate added to the dispatch
pipeline inserts at the gate_filter stage (the `filter_map` /
`.filter` block before scoring), not at the scoring stage. This
discipline is named here so future readers adding a fourth gate (or
porting these gates to a new dispatch path) inherit the pattern.

If `enforce_limits` is ever added to v1 dispatch as a separate step
(today it's only on sub-types within `ActivationCriteria`, not at
dispatch), it must insert after scoring and before dispatch, not
before the gate filter.

### 4.2 V2 explicit-invocation semantics

**V2 uses the same syntactic definition of "explicit invocation" as
v1: a literal `/skill-name` token in the user message triggers
explicit-invocation routing that bypasses gates. Anything else is
auto-fire and respects gates.**

Rationale: the gate's value is precisely *not* relying on intent
inference. If "use the commit skill" (natural-language reference)
counted as explicit, the gate becomes fuzzy — its protection moves
with the orchestrator's intent classifier. Tight today is recoverable;
loose today is hard to tighten without breaking workflows that came
to rely on the loose behavior. The line is intentionally drawn at
*syntactic* explicit invocation, not *semantic*.

Today v2 has no `/skill-name` analogue in the orchestrator's dispatch
path (the orchestrator's `__list_skills__` is the auto-fire candidate
set; explicit invocation in v2 doesn't yet exist as a separate code
path). When v2 grows an explicit-invoke path — likely an opt-in
argument to `__list_skills__` that the Python orchestrator passes
when it detects a literal `/skill-name` in the user message — that
path bypasses the `disable_model_invocation` filter in
`handle_list_skills` by the same logic v1 uses for
`extract_skill_mentions`.

**Out of scope for this work** but stated explicitly so the future
v2-explicit-invoke implementer doesn't accidentally route through
the auto-fire gate: when v2 grows that path, the bypass is required
and mirrors v1.

**Test plan addition** (§5.5 below) confirms the semantic line:
"use the commit skill" (natural language) auto-fires through the
gate, while `/commit` (syntactic) bypasses. Both behaviors are
intentional and tested.

### 4.3 Log event breakdown

Four distinct events deserve distinct treatment:

| Event | When | Level | Format | Audience |
|-------|------|-------|--------|----------|
| **Skill loaded with field set** | At startup, once per skill | INFO | `skill_loaded: <name>, disable_model_invocation=true` (or `user_invocable=false`) | Deployment-posture audit — grep-able snapshot of gate configuration on this machine |
| **Skill filtered during auto-fire** | Per dispatch decision when gate triggers | DEBUG | `skill_skipped: <name>, reason=disable_model_invocation` | Operator debugging "why didn't this fire?" — visible when needed, quiet otherwise |
| **Skill explicitly invoked despite gate** | Per invocation when `/skill-name` bypasses gate | DEBUG | `skill_explicit_invoke: <name>, gate_bypassed=true` | Post-hoc audit — "how often is this gate being bypassed?" Useful for tuning. |
| **Menu filtered N skills** | At `/menu` or `/skills list` render | DEBUG, aggregated | `menu_filtered: N user_invocable_false skills hidden` | Single line per render, not one per skill — keeps menu rendering low-noise |

Principle: **one-shot config audit at INFO (deployment posture
visible without watching every dispatch); per-event filtering at
DEBUG (no normal-operations noise).**

Match `setup_marker`'s existing log level for the per-event filtering
case so operators learn one convention, not two. The existing
`debug!(...)` calls at
`crates/ironclaw_engine/src/executor/orchestrator.rs:2551–2572` are
the precedent — the new gates emit at the same level with parallel
structure.

**Open consideration to surface (flag for future design review, not
a decision for this brief):** the `skill_explicit_invoke` event when
the gate bypassed is `disable-model-invocation` (i.e., the operator
explicitly overrode a side-effect gate on a skill like `commit`) is
in the gray zone between log line and signed receipt. The chain
records substrate-significant events; an operator overriding a gate
that exists specifically to prevent unintended side effects is
arguably one of those. Default for this work is log-only; signed
receipt would be the right shape if the chain should record gate
overrides for accountability. Decision deferred — no audit-chain
integration in this patch.

**Condition under which this deferral activates as load-bearing
work:** when an operator uses `/commit` to bypass the gate that
exists specifically to prevent unintended commits — and then
disputes whether they authorized that commit — a log line is a
weaker artifact than a signed chain receipt for post-hoc dispute
resolution. The moment commit attestation enters the accountability
story (governance spec §6 territory), this deferred item activates.
Add a TODO breadcrumb in code at the explicit-invoke site:

```rust
// TODO(gate-override-receipt): when commit attestation becomes
// load-bearing for accountability, promote this log line to a
// signed chain receipt. See design doc §4.3.
```

The code-side TODO is more discoverable to a future implementer
than a design-doc-only flag.

## 5. Test plan

All tests added in `crates/ironclaw_skills/src/types.rs` and
`crates/ironclaw_skills/src/selector.rs` for the unit cases, and in
`src/agent/commands.rs` (or a new integration test) for the
menu-filter cases.

### 5.1 Schema tests (types.rs)

```rust
#[test]
fn manifest_defaults_preserve_today_behavior() {
    let yaml = "name: foo\ndescription: bar\n";
    let m: SkillManifest = serde_yml::from_str(yaml).unwrap();
    assert_eq!(m.disable_model_invocation, false);
    assert_eq!(m.user_invocable, true);
}

#[test]
fn manifest_parses_disable_model_invocation() {
    let yaml = "name: foo\ndisable-model-invocation: true\n";
    let m: SkillManifest = serde_yml::from_str(yaml).unwrap();
    assert!(m.disable_model_invocation);
    assert!(m.user_invocable); // unchanged default
}

#[test]
fn manifest_parses_user_invocable_false() {
    let yaml = "name: foo\nuser-invocable: false\n";
    let m: SkillManifest = serde_yml::from_str(yaml).unwrap();
    assert!(!m.user_invocable);
    assert!(!m.disable_model_invocation); // unchanged default
}

#[test]
fn manifest_rejects_string_for_boolean_field() {
    // serde must reject `"true"` (string) for a bool field with a
    // clear diagnostic — not silently coerce.
    let yaml = "name: foo\ndisable-model-invocation: \"true\"\n";
    let r: Result<SkillManifest, _> = serde_yml::from_str(yaml);
    assert!(r.is_err(), "string for bool must fail");
}
```

### 5.2 Selector tests (selector.rs)

```rust
#[test]
fn prefilter_excludes_disable_model_invocation() {
    let mut skill = make_test_skill("guarded", &["keyword"]);
    skill.manifest.disable_model_invocation = true;
    let skills = vec![skill];
    let outcome = prefilter_skills("keyword match", &skills, 5, 10_000, &empty_marker_set());
    assert!(outcome.selected.is_empty(),
        "disable-model-invocation must block auto-fire");
}

#[test]
fn prefilter_does_not_affect_skills_without_field() {
    let skill = make_test_skill("normal", &["keyword"]);
    let skills = vec![skill];
    let outcome = prefilter_skills("keyword match", &skills, 5, 10_000, &empty_marker_set());
    assert_eq!(outcome.selected.len(), 1);
}

#[test]
fn extract_skill_mentions_ignores_disable_model_invocation() {
    let mut skill = make_test_skill("guarded", &[]);
    skill.manifest.disable_model_invocation = true;
    let skills = vec![skill];
    let (matched, _) = extract_skill_mentions("please /guarded help", &skills);
    assert_eq!(matched.len(), 1,
        "explicit /mention must override disable-model-invocation");
}

#[test]
fn extract_skill_mentions_ignores_user_invocable_false() {
    let mut skill = make_test_skill("reference", &[]);
    skill.manifest.user_invocable = false;
    let skills = vec![skill];
    let (matched, _) = extract_skill_mentions("use /reference please", &skills);
    assert_eq!(matched.len(), 1,
        "explicit /mention must override user-invocable=false");
}

#[test]
fn chain_load_ignores_disable_model_invocation() {
    // Parent skill chain-loads a child whose disable_model_invocation is true.
    // Child must load — chain-load is explicit reference, not auto-fire.
    //
    // PRE-TEST VERIFICATION: confirm prefilter_skills's return shape before
    // writing this assertion. If chain-loaded companions land in a separate
    // field (e.g., outcome.companions) rather than outcome.selected, retarget
    // the assertion at the correct field. The test must exercise the
    // chain-load path specifically — if a wrong field is asserted, the test
    // either fails spuriously or passes for the wrong reason (e.g., child
    // scores on its own from a keyword match).
    let mut child = make_test_skill("child", &[]);
    child.manifest.disable_model_invocation = true;
    let mut parent = make_test_skill("parent", &["trigger"]);
    parent.manifest.requires.skills = vec!["child".into()];
    let skills = vec![parent, child];
    let outcome = prefilter_skills("trigger word", &skills, 5, 10_000, &empty_marker_set());
    let names: Vec<&str> = outcome.selected.iter().map(|s| s.name()).collect();
    assert!(names.contains(&"child"),
        "chain-loaded companion must not be blocked by disable-model-invocation");
}

#[test]
fn setup_marker_wins_when_combined_with_new_fields() {
    let mut skill = make_test_skill("done-setup", &["any"]);
    skill.manifest.activation.setup_marker = Some("done".into());
    skill.manifest.disable_model_invocation = true;
    skill.manifest.user_invocable = false;
    let skills = vec![skill];
    let mut satisfied = std::collections::HashSet::new();
    satisfied.insert("done".to_string());
    let outcome = prefilter_skills("any text here", &skills, 5, 10_000, &satisfied);
    assert!(outcome.selected.is_empty(),
        "all three exclusion conditions compose cleanly");
}
```

### 5.3 Menu-filter test (commands.rs)

```rust
#[tokio::test]
async fn skills_list_hides_user_invocable_false() {
    // Build a fake registry with one visible and one reference-only skill.
    // Invoke handle_skills_list. Assert the output contains "visible"
    // and does NOT contain "reference-only".
    // (Concrete harness depends on how commands.rs is tested elsewhere
    // in the repo — match the existing pattern.)
}

#[tokio::test]
async fn skills_list_empty_when_only_reference_skills_loaded() {
    // Registry contains only skills with user_invocable: false.
    // handle_skills_list should respond with the "No skills installed"
    // empty-state, not a blank "Installed skills:" header.
}
```

### 5.4 V2 orchestrator test

The existing `handle_list_skills` is exercised by
`crates/ironclaw_engine/src/executor/orchestrator.rs::tests` and by
`tests/skill_setup_marker_lifecycle.rs`. Add one v2 test mirroring
the setup-marker test:

```rust
#[tokio::test]
async fn list_skills_excludes_disable_model_invocation() {
    // Store a Skill MemoryDoc with disable-model-invocation: true in
    // metadata. Call handle_list_skills. Assert the returned list
    // does not contain that skill.
}
```

### 5.5 V2 explicit-invocation semantics — syntactic vs natural language

Confirms the line drawn in §4.2: syntactic `/skill-name` bypasses the
gate; natural-language reference does not.

```rust
#[tokio::test]
async fn v2_syntactic_slash_bypasses_disable_model_invocation() {
    // User message: "/commit my changes"
    // Skill `commit` has disable-model-invocation: true in metadata.
    // Expected: commit IS included in the resolved skill set (explicit
    // operator intent), because the orchestrator's explicit-invoke path
    // bypasses the auto-fire gate.
    //
    // NOTE: This test will be unimplementable until v2 grows the
    // explicit-invoke path. Mark `#[ignore]` with a comment pointing at
    // the future task. The behavioral assertion still belongs in the
    // test corpus so the future implementer can un-ignore it when
    // landing the explicit-invoke path.
}

#[tokio::test]
async fn v2_natural_language_does_not_bypass_disable_model_invocation() {
    // User message: "please use the commit skill to push my changes"
    // Skill `commit` has disable-model-invocation: true in metadata.
    // Expected: commit is NOT included in the candidate set returned
    // by handle_list_skills. Natural-language reference is auto-fire
    // territory and respects the gate.
}
```

### 5.6 Dispatch-ordering invariant

Confirms the ordering rule in §4.1: gates run before scoring/limits.

```rust
#[test]
fn prefilter_runs_gates_before_scoring() {
    // Construct two skills:
    //   - "weak":     score=1, no gate set
    //   - "strong":   score=100, disable_model_invocation=true
    // Call prefilter_skills with a max_skills=1 limit.
    // Expected: "weak" is selected. If the gate ran AFTER scoring,
    // "strong" would win the top-1 slot and then be filtered out,
    // leaving an empty selection. The test asserts the correct shape
    // (gate-filter then score-top-N).
    let weak = make_test_skill_with_score("weak", 1);
    let mut strong = make_test_skill_with_score("strong", 100);
    strong.manifest.disable_model_invocation = true;
    let skills = vec![weak, strong];
    let outcome = prefilter_skills("trigger", &skills, 1, 10_000, &empty_marker_set());
    assert_eq!(outcome.selected.len(), 1);
    assert_eq!(outcome.selected[0].name(), "weak",
        "gate must filter before score-based top-N");
}
```

### 5.7 Log emission tests

The four log events in §4.3 are observable via the tracing test
subscriber. Add two tests to confirm levels and structure:

```rust
#[test]
fn config_audit_logs_at_info_on_skill_load() {
    // Load a SKILL.md with disable-model-invocation: true.
    // Capture tracing events via tracing_test or similar.
    // Assert: an INFO-level event was emitted with field
    // disable_model_invocation=true and the skill name.
}

#[test]
fn auto_fire_skip_logs_at_debug() {
    // Trigger prefilter_skills with a disable-model-invocation skill
    // that would otherwise score. Capture tracing events.
    // Assert: a DEBUG-level "skill_skipped" event was emitted with
    // reason=disable_model_invocation.
}
```

The explicit-bypass and menu-filter events are similarly testable but
optional in this patch — the principle is captured in §4.3 and the
event signature is the contract.

## 6. Cleanup notes

- `skills/commit/SKILL.md` already has `disable-model-invocation: true`
  in frontmatter (per the investigation doc). Today that field is
  silently dropped by serde. After this design lands and is
  implemented, the field begins enforcing. No edit to
  `skills/commit/SKILL.md` is required.
- The audit doc (`docs/skill-audit-2026-05.md`) flags ~20 skills for
  one or both fields. Applying those flags is the *sweep step*
  described in the investigation's "Implementation sequence" §3 —
  follow-on work after this design is implemented, not part of this
  patch.
- No additional changes to `enforce_limits`, no new constants, no
  new validation paths. The booleans need no sanitization.

## 7. Surface check against acceptance criteria

| Criterion | Where it's satisfied |
|---|---|
| Schema fields specified with exact serde attributes | §1.1 |
| Backwards compatible — no behavior change for existing SKILL.md | §1.3 |
| Every enforcement point named with file/function | §2.1–§2.4 |
| `setup_marker` composition explicit | §3.1, §3.2 |
| Dispatch ordering invariant explicit | §4.1 |
| V2 explicit-invocation semantics specified | §4.2 |
| Log event semantics specified at each level | §4.3 |
| Test plan covers all combinations + malformed values + ordering + log levels | §5.1–§5.7 |
| Implementation is bounded (one struct change, four insertions) | §2 (count: 4 sites) |

## 8. Open questions — resolved

The three open questions originally flagged in this design were
discussed and resolved during review. Their conclusions are now
incorporated as §4:

1. **Log level for filtering events** — resolved at §4.3. INFO at
   startup once per gate-bearing skill (deployment-posture audit);
   DEBUG per dispatch decision (matches `setup_marker` precedent).
   Four distinct events documented with formats and audiences. One
   open consideration deferred: should the `skill_explicit_invoke`
   event when bypassing `disable-model-invocation` be a signed
   audit-chain receipt rather than a log line? Default for this
   patch is log-only.

2. **V2 explicit-invocation parity** — resolved at §4.2. V2 uses
   the same syntactic definition as v1 (literal `/skill-name`
   bypasses gates; natural-language reference does not). Today's
   v2 has no explicit-invoke path so the bypass logic is stubbed for
   future implementation; the principle is documented and a future
   test is scaffolded at §5.5 with `#[ignore]` so it activates when
   the v2 explicit-invoke path lands.

3. **`enforce_limits` participation** — resolved at §4.1. Filter
   gates run before scoring/limits in the canonical dispatch
   sequence (`candidates → gate_filter → score → top_N/enforce_limits
   → dispatch`). Current v1 and v2 designs both satisfy this by
   construction. Booleans need no per-skill sanitization, so no
   `enforce_limits` method is added to `SkillManifest`. The ordering
   principle is stated explicitly so future gate authors inherit
   the discipline; test §5.6 verifies the invariant.

## 9. Implementation surface — final scope

| Site | File | Lines | Change |
|---|---|---|---|
| Schema | `crates/ironclaw_skills/src/types.rs` | ~164 | +14 lines (2 fields + default fn + doc comments) |
| V1 auto-fire gate | `crates/ironclaw_skills/src/selector.rs` | ~186 | +4 lines |
| V1 CLI menu gate | `src/agent/commands.rs` | ~952 | +4 lines |
| V1 web menu gate | `src/channels/web/handlers/skills.rs` | ~108 | +4 lines |
| V2 auto-fire gate | `crates/ironclaw_engine/src/executor/orchestrator.rs` | ~2552 | +10 lines |
| Tests | (above + new integration test + ordering + log emission) | — | ~200 lines |

Single focused commit. No refactor. No new modules. No new feature
flags.

## Refs

- Investigation:
  `docs/handoffs/skill-manifest-parse-frontmatter-investigation-2026-05.md`
- Audit:
  `docs/skill-audit-2026-05.md`
- Existing `setup_marker` precedent:
  `crates/ironclaw_skills/src/selector.rs:96–112, 178–186`
  `crates/ironclaw_engine/src/executor/orchestrator.rs:2549–2573`
- Existing menu surfaces:
  `src/agent/commands.rs:929` (CLI)
  `src/channels/web/handlers/skills.rs:89` (web)
- Already-decorative declaration:
  `skills/commit/SKILL.md` (`disable-model-invocation: true`)
