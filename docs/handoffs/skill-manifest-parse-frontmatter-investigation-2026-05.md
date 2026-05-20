# Handoff — SkillManifest frontmatter parsing for visibility fields

*2026-05-17. Target: terminal Claude working in `~/projects/ironclaw`.
Substrate-tier prerequisite for the skill audit's frontmatter
recommendations to actually enforce. Without this, the audit's
visibility-tier recommendations are decorative.*

## Goal

Extend `SkillManifest` to parse two visibility-control fields from
SKILL.md frontmatter:

- `disable-model-invocation: true` — model cannot auto-fire this skill on
  context match; user must explicitly invoke (`/skill-name`).
- `user-invocable: false` — skill is hidden from `/menu` and not directly
  invocable; available only as a knowledge/methodology reference loaded
  when other skills reference it.

Default (field absent) preserves today's behavior: model can auto-fire,
user can invoke. The audit doc at
`docs/skill-audit-2026-05.md` walks each skill against these signals;
this work makes those signals real.

## Evidence — what's broken today

From the audit's load-bearing caveat:

> The IronClaw skill loader (`crates/ironclaw_skills/src/types.rs::SkillManifest`)
> does NOT parse `disable-model-invocation` or `user-invocable` — they're
> silently ignored. Adding them is forward-looking signal, not enforced
> behavior.

A change already landed in `skills/commit/SKILL.md` adding
`disable-model-invocation: true` based on the audit. That field is
currently inert. After this brief's work lands, the field begins
enforcing.

## Investigation surface

```sh
# The struct that parses SKILL.md frontmatter today
rg -n "SkillManifest|setup_marker" ~/projects/ironclaw/crates/ironclaw_skills/src

# The router/dispatcher that decides which skills can auto-fire
rg -n "auto_invoke|model_invocation|skill_match" ~/projects/ironclaw/crates/ironclaw_engine/src
rg -n "auto_invoke|model_invocation|skill_match" ~/projects/ironclaw/src

# The /menu builder that lists invocable skills
rg -n "/menu|skill_menu|list_skills" ~/projects/ironclaw/crates/ironclaw_engine/src
rg -n "/menu|skill_menu|list_skills" ~/projects/ironclaw/src

# Existing setup_marker handling — the engine-enforced exclusion pattern
# that disable-model-invocation should compose with
rg -n "setup_marker" ~/projects/ironclaw
```

Confirm before designing:

- How `setup_marker` is enforced today (the engine-enforced exclusion
  mechanism the audit identified as IronClaw's native equivalent). The
  new fields should compose with it, not replace it.
- Where the dispatch decision lives — when the model considers
  auto-firing a skill, what struct/function does it consult?
- How `/menu` enumerates user-invocable skills.

## The structural answer

### Schema change

Extend `SkillManifest` with two optional fields:

```rust
pub struct SkillManifest {
    // ... existing fields
    #[serde(default, rename = "disable-model-invocation")]
    pub disable_model_invocation: bool,
    #[serde(default = "default_user_invocable", rename = "user-invocable")]
    pub user_invocable: bool,
}

fn default_user_invocable() -> bool { true }
```

`serde(default)` on `disable_model_invocation` means absent → `false`
(today's behavior preserved). `default_user_invocable() -> true` means
absent → invocable (today's behavior preserved). Both fields opt-in.

### Enforcement points

**`disable_model_invocation = true`:** when the engine evaluates whether
to auto-invoke a skill on context match, check this field. If true,
skip auto-invocation regardless of how strong the context match looks.
The skill still appears in `/menu` and responds to explicit invocation.

**`user_invocable = false`:** when `/menu` enumerates available skills,
filter these out. The skill is still loadable by other skills that
reference it (treat as reference material, not as an operator-facing
entry point).

Both fields compose orthogonally:
- Default (both unset): auto-fires, appears in menu — today's behavior.
- `disable-model-invocation: true` only: appears in menu, must be
  explicitly invoked.
- `user-invocable: false` only: invisible to operator, but agent can
  auto-load when context matches.
- Both: hidden from menu AND won't auto-fire — pure reference material
  loadable only when other skills reference it explicitly.

### Compose with `setup_marker`

`setup_marker` is the existing engine-enforced exclusion mechanism. It
should remain authoritative; the new fields layer on top. Priority:

1. `setup_marker` — engine-enforced exclusion (highest)
2. `disable-model-invocation` — model-side gate against auto-fire
3. `user-invocable: false` — menu visibility gate
4. Default — fully available

A skill with `setup_marker` set is excluded regardless of the new
fields. The new fields add finer-grained control for skills without
`setup_marker`.

## Edge cases to trap or accept

| Edge case | Treatment |
|-----------|-----------|
| Skill author sets `disable-model-invocation: true` but model gets context match | Engine skips auto-invoke. Logs at debug level so the author can see the skip happened. |
| User invokes `/skill-name` for a skill with `user-invocable: false` | Two options: (a) execute it anyway since user was explicit, (b) refuse with "this skill is reference-only." Pick (a) — explicit user intent overrides menu visibility. |
| Another skill references a `user-invocable: false` skill | Load it normally. The field controls menu visibility, not loadability. |
| Old SKILL.md files without these fields | `serde(default)` handles it — fields default to today's behavior. No migration needed. |
| Both `disable-model-invocation: true` AND `setup_marker` set | `setup_marker` wins (it's the engine-enforced exclusion). Fields don't conflict; they compose. |
| Field value is a string `"true"` instead of bool `true` | Serde rejects with a clear error. Skill fails to load, operator sees the parse error. Not silent. |

## Deliverable

`docs/handoffs/skill-manifest-parse-frontmatter-design-2026-05.md`
covering:

1. **Schema change** — exact `SkillManifest` field additions with serde
   attributes, including the migration story for existing SKILL.md
   files (should be transparent).
2. **Enforcement points** — concrete file/function locations where the
   engine checks each field. Inserting one gate per field; not a
   refactor.
3. **`setup_marker` composition** — clear precedence rule, with a test
   that exercises all combinations.
4. **Test plan:**
   - Load a SKILL.md with neither field set → today's behavior preserved.
   - Load a SKILL.md with `disable-model-invocation: true` → context-match
     auto-invoke is skipped; explicit invoke works.
   - Load a SKILL.md with `user-invocable: false` → not in menu; explicit
     invoke works; other skills can still reference.
   - Load a SKILL.md with both → hidden from menu AND won't auto-fire.
   - Load a SKILL.md with both new fields AND `setup_marker` → excluded
     entirely; setup_marker wins.
   - Malformed value → parse error, skill rejected, clear diagnostic.
5. **Cleanup notes** — `skills/commit/SKILL.md` already has
   `disable-model-invocation: true` added by the audit. After this
   work lands, that field becomes enforcing. No additional change to
   commit/SKILL.md needed.

Don't ship code in this round. Design the change, propose the field
shape, walk through enforcement points, bring back for review.

## Acceptance criteria

After the design lands (not the implementation yet):

1. **Schema fields specified** — exact serde attributes, default values,
   field names matching the audit's convention.
2. **Enforcement is concrete** — every place the new fields gate
   behavior is named with file/function.
3. **Backwards compatible** — every SKILL.md that loads today still
   loads after the change, with identical behavior.
4. **`setup_marker` composes cleanly** — precedence rule explicit,
   test plan covers the interaction.
5. **Implementation surface is bounded** — one struct change, two
   enforcement gates, a handful of tests. Single focused commit.

## Implementation sequence (post-design)

1. **This brief** → design landed, parser change scoped.
2. **CLIC implements parser** → `SkillManifest` parses fields, gates
   enforce, tests cover the combinations.
3. **CLIC sweeps audit recommendations** → using
   `docs/skill-audit-2026-05.md` as the application brief, walks the
   ~20 flagged items, applies frontmatter additions where the audit
   indicated. Single commit per recommendation cluster.
4. **Verification** → spot-check that a skill with
   `disable-model-invocation: true` actually doesn't auto-fire on
   context match in a live session.

## Out of scope

- The `*-setup` skill duplication refactor (`commitment-setup`,
  `ceo-setup`, `developer-setup`, `content-creator-setup`,
  `trader-setup` share ~60% scaffolding). Separate work, larger
  surface, deserves its own brief.
- Upstream PRs to Cowork knowledge-work-plugins or Apollo MCP. Those
  are external work; this brief is IronClaw-internal.
- The `commit` keyword overlap with the `github` skill — 5-minute
  fix, do it inline as part of the sweep step rather than designing
  it separately.
- Engine-level prompt audit (the 10 files in
  `crates/ironclaw_engine/prompts/`). Different surface, different
  framework.
- The "prose-as-consent-gate" anti-pattern in third-party skills
  (Apollo sequence-load). Upstream concern, not IronClaw-internal.

## Connection to existing architecture

- Composes with `setup_marker` — same surface, finer grain. Doesn't
  replace; layers on top.
- Brings IronClaw skill manifest closer to the Anthropic SKILL.md
  convention, which makes ported skills less surprising.
- The two fields are independent: `disable-model-invocation` is
  about safety (no auto-fire on side-effect surfaces),
  `user-invocable` is about discoverability (don't clutter the
  operator menu with methodology). Combining them gives fine-grained
  control without adding complexity.

## Refs

- `docs/skill-audit-2026-05.md` — the audit doc this enables
- `crates/ironclaw_skills/src/types.rs::SkillManifest` — the struct
  to extend
- `skills/commit/SKILL.md` — already has
  `disable-model-invocation: true` (currently decorative; becomes
  enforcing after this work)
- Audit's load-bearing caveat section — the gap this closes
