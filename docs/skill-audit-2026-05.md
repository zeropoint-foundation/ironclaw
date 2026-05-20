# IronClaw Skill Audit — 2026-05-17

Audit of the IronClaw skill library against the three-lens framework
(visibility frontmatter, determinism vs judgment, composability). Scope:
`skills/` (32 skills) and `.claude/skills/` (2 skills). Engine-internal
prompts under `crates/ironclaw_engine/prompts/` were spot-checked but
not edited.

## 0. Important context — frontmatter field status

The framework's `disable-model-invocation` and `user-invocable` fields
are conventions from Claude Code's skill format. The IronClaw skill
loader (`crates/ironclaw_skills/src/types.rs::SkillManifest`) **does not
currently parse these fields** — they are silently ignored by serde
(no `deny_unknown_fields`). That means:

- Adding them today is **forward-looking documentation**, not enforced
  behavior. The skill selector will still consider the skill for
  auto-invocation regardless.
- The repo already has precedent for this pattern in
  `.claude/commands/*.md` (e.g. `respond-pr.md`, `triage-prs.md` —
  these use `disable-model-invocation: true` for the Claude Code path).
  Adding the same field to skill manifests keeps a consistent
  vocabulary across both surfaces and prepares for a future engine
  patch that honors it.
- IronClaw's existing native equivalent is `setup_marker`: the
  `*-setup` skills are excluded from selection after the marker file
  exists. This already covers the "fire once, then disable" intent
  for setup bundles.

Where this audit recommends `disable-model-invocation`, it is
recommending a **signal**, not a mechanical guard. The real guards are
the in-body confirmation gates ("ask for confirmation before
committing", "ask the user which findings to post").

## 1. Inventory

### High side-effect risk (writes outside workspace: git, GitHub API mutations, cron mission install, external messages)

| Skill | Side effects |
|-------|-------------|
| `commit` | Runs `git commit` (with confirmation gate in body) |
| `github` | Creates issues, PRs, comments via GitHub API. Wide keyword surface ("commit", "branch", "issues") |
| `github-workflow` | Installs 6 cron-driven missions per repo via `mission_create`; configures webhook events |
| `project-setup` | Creates per-repo workspace project + installs workflow missions (via github-workflow) |
| `code-review` | Posts PR-level + line-level review comments to GitHub (asks for confirmation first) |
| `security-review` | Writes findings as commitments + signals; "auto-fix obvious" pattern |
| `qa-review` | Generates test code and presents for approval; `[TEST GENERATED]` marker |
| `portfolio` | Reads/writes per-project workspace state; explicitly forbids signing/private keys; builds *unsigned* intents only |
| `local-test` | Runs `docker build`, `docker run`, `docker stop` — local container side effects |

### Medium side-effect risk (workspace-only memory writes, mission creation scoped to workspace)

| Skill | Side effects |
|-------|-------------|
| `commitment-setup` | Workspace writes (schema, dirs); creates two missions (`commitment-triage`, `commitment-digest`) |
| `ceo-setup` | Workspace + dashboard widgets + missions |
| `developer-setup` | Workspace + 6 missions + per-repo workflow installs |
| `content-creator-setup` | Workspace + 3 missions |
| `trader-setup` | Workspace + 3 missions |
| `new-project` | Creates project workspace + project-scoped missions |
| `delegation` | `memory_write` + `routine_create`; confirms plan before executing |
| `commitment-triage` | `memory_write` for signals/commitments — passive extraction (Mode A) by design |
| `delegation-tracker` | `memory_write` for delegated commitments |
| `decision-capture` | `memory_write` to decisions/ + commitments + intel |
| `idea-parking` | `memory_write` to parked-ideas/ |
| `tech-debt-tracker` | `memory_write` to tech-debt/ |
| `plan-mode` | `memory_write` plan docs + `mission_create` + `mission_fire` |
| `commitment-digest` | `message` send (when invoked from mission); chat-only when invoked conversationally |
| `linear` | GraphQL mutations (`issueCreate`, `issueUpdate`) + identity cache write |
| `web-ui-test` | Runs `cargo run`, deletes installed skills via `rm -rf` |
| `routine-advisor` | `routine_create` after explicit user confirmation |

### Low side-effect (read-mostly, evidence-based scoring, dashboards)

| Skill |
|-------|
| `review-readiness` |
| `product-prioritization` |
| `llm-council` |

### Methodology-only (no side effects; behavioral guidance for the agent)

| Skill |
|-------|
| `coding` (file editing discipline) |
| `review-checklist` (pre-merge verification list) |

### .claude/skills (Claude Code path, not IronClaw skill loader)

| Skill | Side effects |
|-------|-------------|
| `architecture-video` | Runs Remotion CLI to render video |
| `mintlify-docs` | Methodology / writing guidance |

## 2. Changelog of applied edits

### `skills/commit/SKILL.md`

**Change:** Added `disable-model-invocation: true` to frontmatter
(adjacent to `description`).

**Reason:** This is the unambiguous case for the framework:
- The skill is literally named after a side-effect operation.
- The bare keyword `commit` (single token) is enough to fire it,
  meaning any conversational mention of the word — "let me commit to
  this plan" — could match.
- Conflicts with `github` skill which also lists `"commit"` as a
  keyword; both could be selected simultaneously.
- The body of the skill DOES contain a confirmation gate ("Show the
  proposed commit message to the user and **ask for confirmation**
  before running `git commit`"), so the safety story is intact, but
  the frontmatter should reinforce the same posture.
- Aligns with the convention already used in
  `.claude/commands/respond-pr.md` and other PR-mutating commands.

Note: IronClaw's skill loader currently ignores this field (see §0).
The edit is forward-looking signal, not enforced behavior. The actual
runtime protection remains the in-body confirmation gate.

## 3. Recommendations not applied

These are flagged for Ken's review because they alter agent behavior
in non-obvious ways, are cross-skill refactors, or involve judgment
calls about intended scope.

### 3.1 Visibility — frontmatter recommendations

#### Recommend `disable-model-invocation: true` on:

**`github-workflow`** — Installs 6 cron missions per invocation. The
activation patterns are specific ("set up workflow for owner/repo")
but the keyword `github workflow` could match looser contexts.
Installing automated cron is exactly the kind of operation that
should require explicit operator intent. The body has no confirmation
gate before `mission_create` calls — it just walks the install procedure.

**`project-setup`** — Wraps `github-workflow`. Same reasoning: each
invocation installs ≥5 missions. Should be explicit.

**`local-test`** — Runs `docker build` and `docker run`. Mostly safe
side effects (local containers) but still long-running and consumes
disk + CPU. Adding `disable-model-invocation: true` would prevent
the skill from auto-firing on tangential mentions of "testing".

**`web-ui-test`** — Runs `cargo run`, can `rm -rf ~/.ironclaw/installed_skills/<x>`.
That `rm -rf` step is enough to recommend explicit-only invocation.

**`code-review`** (PR path only) — Posts review comments to GitHub.
The body asks "which findings to post as PR comments. Default: all
Critical, High, and Medium" before posting, so there's a gate. But
the activation triggers (`review`, `code review`, `review changes`)
are broad enough that the skill could fire on read-only requests
("can you review this code") and confuse the operator about which
mode it's in. Recommend `disable-model-invocation: true` to force
explicit invocation, with the local-diff path remaining handy
through `/review` as the explicit command.

Alternative: split into `code-review` (local diff, methodology) and
`code-review-pr` (GitHub posting) so the auto-invokable surface
only does read-only work.

**`security-review`, `qa-review`** — Both write findings to
`projects/commitments/signals/pending/` and `projects/commitments/open/`
("P1 findings also create a commitment in projects/commitments/open/
automatically with urgency: critical"). The `security-review` body
also says "Auto-fix and mark `[AUTO-FIXED]`" for obvious fixes.
That's a code-write side effect on auto-invocation match. Recommend
`disable-model-invocation: true` on both so they only fire when the
operator explicitly requests an audit. The reactive
"hey, is this secure?" path stays in the agent's general behavior.

#### Recommend `user-invocable: false` on:

**`coding`** — Pure behavioral guidance (tool usage discipline,
"prefer apply_patch over write_file"). No procedure, no
side-effecting recipe. It should be loaded for context but never
"run." Marking `user-invocable: false` keeps it as context-only.

**`review-checklist`** — Static pre-merge checklist. Methodology, not
a procedure. Same reasoning.

**`routine-advisor`** — The body explicitly says the skill *suggests*
routines and "Wait for the user to confirm before creating." It's
methodology for *when* to propose a routine. Marking
`user-invocable: false` clarifies that "run routine-advisor" isn't
meaningful — the skill is just context for the agent's proposing
behavior.

**`plan-mode`** — Tricky case. The body is procedural and *does*
expect the user to type `[PLAN MODE] Approve` to fire `mission_create`
+ `mission_fire`. So it IS user-invocable in practice. Leave alone.

**`commitment-triage`** — Designed for passive signal extraction
(Mode A); the agent should silently fire memory_writes when it
detects an obligation. Marking `user-invocable: false` would be
*wrong* — auto-invocation is the point. Leave alone.

**`commitment-digest`, `decision-capture`, `delegation-tracker`,
`idea-parking`, `tech-debt-tracker`** — These are all "the agent
should proactively notice X and write Y" patterns. The framework
calls these benign operational skills — keep defaults. Leave alone.

#### Setup skills (`*-setup`)

Already gated by `setup_marker`. Once `projects/commitments/.developer-setup-complete`
exists, the skill is excluded from selection. This is IronClaw's
native equivalent of `user-invocable: false` and is *engine-enforced*
(unlike the Claude Code field). For the *first run*, the activation
patterns are reasonably specific ("I'm a developer", "set up dev workflow")
and the auto-invocation risk is low.

Recommendation: leave the setup skills alone. The marker mechanism
already does the structural work.

### 3.2 Determinism vs judgment — script extraction recommendations

These are places where SKILL.md asks the AI to perform a fixed
operation that could be a script call. Each is a recommendation
because the design tradeoff (composability, audit trail, project
conventions about workspace-vs-host writes) needs Ken's call.

#### `commit/SKILL.md` — git inspection commands

Steps 1-3 are:
```
git status
git diff --cached
git log --oneline -5
```

These are deterministic shell calls. Could become a tiny
`scripts/inspect.sh` that emits a structured JSON envelope (staged
files, diff text, recent commit style examples). The AI step then
narrows to (a) drafting the commit message from the diff and
(b) checking for secret-containing files (which is its own
deterministic regex sweep — could also be a script).

**Recommendation:** Extract a `scripts/inspect.sh` and a
`scripts/check-secrets.sh`. Keeps the message drafting (judgment)
as the AI's job; pre-flight inspection becomes deterministic.

#### `local-test/SKILL.md` — Docker invocations

The "Building the Image" and "Running Containers" sections have
templated docker commands with env-var substitutions and per-LLM-backend
recipes. This is exactly the shape that wants a `scripts/run.sh`
with flags: `scripts/run.sh --backend nearai --port 3003`.

**Recommendation:** Extract `scripts/build.sh`, `scripts/run.sh`,
`scripts/cleanup.sh`. The SKILL.md becomes a thin "how to use" doc
pointing at the scripts. This also eliminates the multiple near-duplicate
docker-run blocks (one per backend), each of which has independent
drift risk.

#### `commitment-setup/SKILL.md` — directory placeholder writes (Step 4)

Six identical `memory_write(target=..., content="<one-line readme>", append=false)`
calls. This is mechanical scaffolding.

**Recommendation:** Wrap in a single
`scaffold_workspace(project_path)` host helper, or have the skill
invoke a small Python script via CodeAct. The current shape forces
the AI to walk through six near-identical tool calls. Six chances
to drop one silently.

#### `ceo-setup/SKILL.md` — widget asset writes (Step 5)

The skill embeds three files of widget JS/CSS as escaped JSON
strings inside `memory_write` content. That JS/CSS lives nowhere
else — it's encoded into the SKILL.md body. Maintaining it means
editing escaped strings.

**Recommendation:** Move the widget files into
`skills/ceo-setup/widget/` (mirror `skills/portfolio/widget/` which
already does this correctly). The SKILL.md then says "copy files
from `widget/` to `projects/commitments/.system/widgets/...`" —
which can itself be a script. Removes the escape-string maintenance
burden and matches an existing pattern in the same library.

#### `developer-setup/SKILL.md` — calibration document body

The entire calibration markdown is embedded as a `\n`-escaped string
inside a memory_write call. Same maintenance issue as ceo-setup
widgets.

**Recommendation:** Move calibration content to
`skills/developer-setup/calibration.md` and have the skill do
`memory_write(target="...", content=<read from skill assets>)`. Or
better: a `scaffold-developer-workspace.sh` script that copies the
whole asset set.

This pattern repeats in **`content-creator-setup`**, **`trader-setup`**,
**`ceo-setup`** — all of them embed calibration text as escaped strings.

#### `project-setup/SKILL.md` — template substitution

Step 4 says "Replace `{{repository}}` with `owner/repo`, replace
`{{slug}}` with `<owner>-<repo>`," etc. That's template
substitution, not judgment. The `github-workflow` references file
`workflow-routines.md` holds the templates.

**Recommendation:** Extract a `scripts/install-workflow.sh` (or
.py) that takes flags `--repo owner/repo --maintainers ... --staging-branch ...`
and emits the rendered `mission_create` calls (or invokes them
directly). The AI's role becomes "collect parameters from the user"
and "fire the script" — the substitution happens deterministically.

#### `delegation-tracker/SKILL.md` and `tech-debt-tracker/SKILL.md` — slugify

Both skills include "Slugify: lowercase, hyphens, no special chars,
max 50 chars" as an AI instruction. That's a pure function.

**Recommendation:** Either provide a `slugify(text)` host helper
that's documented once and referenced from every skill that needs
it, or extract a `scripts/slugify.sh`. Multiple skills doing
slugification in prompt-space invite drift (one normalizes
underscores, another doesn't).

### 3.3 Composability — duplicated patterns

#### Schema duplication across setup skills

`ceo-setup`, `developer-setup`, `content-creator-setup`,
`trader-setup`, and `commitment-setup` all need to write the same
core schema (signal, commitment, decision, parked-idea) into
`projects/commitments/README.md`. The current pattern: `commitment-setup`
holds the canonical schema in its Step 3, and the persona setup
skills say "see commitment-setup for the complete schema." This
relies on the AI navigating to the other skill and copying.

**Recommendation:** Hoist the schema to a shared asset, e.g.
`skills/_shared/commitments-schema.md`, and have every setup skill
do `memory_write` reading from that asset. Then there is one source
of truth. The persona-specific calibration stays per-skill, but the
shared schema is genuinely shared.

This is the most impactful refactor in the library: schema drift
between personas is currently the most likely correctness bug.

#### Identical mission preambles

Every triage and digest mission across personas starts with "Read
projects/commitments/README.md for schema." then walks the same
gather → group → emit pattern with persona-specific tuning. This is
a ~10-line common preamble repeated 5 times with small variations.

**Recommendation:** Move the common preamble into the
`commitment-triage` and `commitment-digest` skill bodies (which
already exist as standalone skills) and have the per-persona
mission goals reference them by name. Mission goals shrink to the
persona-specific tuning ("trader: position-aware; expire in 4h on
market days"). Skill text doesn't get re-typed in mission goal
strings.

#### Persona-conditional behavior in shared skills

`commitment-triage/SKILL.md` already has Mode A/B/C/D for distinct
triggers (passive detection, explicit capture, resolution,
promotion). The persona setup skills layer additional persona
behavior on top (e.g. trader: read positions.md first). This
layering happens in mission goals, not skill bodies, which is fine
— but the trader/creator/CEO logic is scattered across mission
goal strings instead of being a single document.

**Recommendation:** Each persona setup skill writes a
`projects/commitments/persona.md` (or extends `calibration.md`)
declaring the persona-specific rules. The shared
`commitment-triage` skill body reads that file as the authoritative
persona rule set. Removes duplication; lets a persona be re-tuned
by editing one file instead of mission goal strings.

#### GitHub API patterns

Both `github` and `code-review` embed the same async/await pattern
for sandbox compatibility ("Monty sandbox does NOT reliably capture
`import asyncio` into the function closure"). Both warn against
`get("body", body)` safety nets. Both describe the `application/vnd.github.raw`
media type quirk.

**Recommendation:** Move this guidance to a shared
`skills/github/references/sandbox-quirks.md` and have `code-review`
reference it. Currently the docstring is duplicated, which means a
correction (e.g. when the sandbox is fixed) has to land in two
places.

#### Listing-then-grouping pattern

`commitment-digest`, `tech-debt-tracker` (Mode D), `idea-parking`
(listing), `delegation-tracker` (informally), and `portfolio` (step
5 ranking) all walk the same pattern: `memory_tree` a directory,
`memory_read` each file, group by some field, render. This is a
common shape with five distinct implementations.

**Recommendation:** Consider a single `list_and_group(dir, sort_by, group_by)`
helper exposed to skills (either as a tool or a script template). Each
skill becomes responsible only for choosing axes, not for walking
the file system.

## 4. Pattern findings (top-level themes)

1. **The five `*-setup` skills are 60% duplicated.** They all
   declare `projects/commitments/`, write the same six placeholder
   READMEs, install the same two missions (`commitment-triage`,
   `commitment-digest`) with persona-tuned goal strings, and write
   a `calibration.md`. The unique 40% is persona-specific
   calibration text and persona-specific mission cadences.
   Consolidation opportunity: shared scaffolding script + per-persona
   calibration assets.

2. **Schema definitions are duplicated as escaped strings inside
   memory_write calls.** Both the commitments schema (Step 3 of
   `commitment-setup`) and the per-persona calibration files
   embed multi-page markdown as escaped strings. Editing these is
   error-prone. Moving to skill-asset files would eliminate the
   escaping and let normal Markdown tools (linters, spellcheckers,
   diff readers) work on them.

3. **The `commit` skill keyword overlaps with `github` skill keyword.**
   Both list `"commit"` as a keyword. Hard to predict which one wins
   on a bare "commit" mention. Recommend removing `"commit"` from
   `github`'s keyword list (it already has `pull request`,
   `repository`, `branch`, which are unambiguous) and letting
   `commit` own that word.

4. **Several skills define a "this skill is only successful if
   memory_write succeeded" assertion.** `commitment-triage`,
   `decision-capture`, `delegation-tracker`, `idea-parking`,
   `content-creator-setup`. This is the right discipline — agents
   that confirm without persisting are a known failure mode. But
   it's also a duplicated norm. Consider making it a global skill
   convention documented once (e.g. in
   `skills/_shared/persistence-discipline.md`) and referenced rather
   than re-stated.

5. **Two-tier `references/` pattern (already in use for
   `github-workflow`) is the right shape.** `github-workflow` keeps
   `workflow-routines.md` separate from `SKILL.md`. This is the model
   to extend for setup skills' schemas, calibration text, widget
   assets, and shared helpers.

6. **No skills currently use scripts/ except `portfolio`.** Portfolio
   has four `scripts/*.py` files and explicitly documents the pattern
   ("Four starter scripts ship with this skill"). This is the model
   to extend for `commit`, `local-test`, `project-setup`, and the
   setup skills' scaffolding work.

## 5. Methodology — checklist for writing new IronClaw skills

When authoring a new SKILL.md, apply the three-lens framework
*before* the first commit.

### Visibility

- [ ] **Is the skill name a verb that describes a side effect?**
  (commit, deploy, send, post, merge, install, delete) — if yes,
  add `disable-model-invocation: true` and require explicit invocation.
- [ ] **Is the skill methodology-only (no procedural steps to run)?**
  — if yes, add `user-invocable: false` to mark it as context-only.
- [ ] **Are activation keywords specific enough?** Single bare tokens
  ("commit", "branch", "test") match too broadly. Prefer multi-word
  phrases or domain-specific verbs.
- [ ] **Do keywords overlap with other skills?** Run a grep across
  `skills/*/SKILL.md` for each keyword before adding it. Resolve
  overlaps by removing the weaker claim or adding `exclude_keywords`.
- [ ] **For one-shot setup skills, set `setup_marker`** to a file the
  skill itself writes during setup. This is engine-enforced exclusion.

### Determinism vs judgment

- [ ] **Identify each step in the skill body.** For each, classify:
  deterministic (file ops, template substitution, slugify, regex
  scan, shell command with fixed flags) vs judgment (drafting,
  weighing tradeoffs, choosing names).
- [ ] **Move every deterministic step into `scripts/`** (or a host
  helper). The SKILL.md becomes a thin orchestrator that
  collects inputs, fires scripts, and applies judgment to outputs.
- [ ] **Don't embed multi-page content as escaped strings inside
  tool-call examples.** Use `references/` or skill assets and have
  the procedure read from them.
- [ ] **Don't slugify, hash, or template-substitute in prompt space.**
  These are pure functions; let them live in code.

### Composability

- [ ] **Search for existing skills that do the same thing.** Five
  setup skills share 60% of their bodies — don't add a sixth that
  retypes the same patterns.
- [ ] **If you're writing a passive-detection skill (silently extract
  signal into memory), include the "only successful if memory_write
  succeeded" assertion** — match the existing discipline in
  `commitment-triage`, `decision-capture`, etc.
- [ ] **Reference shared assets, don't duplicate them.** Schema docs,
  calibration templates, mission goal preambles, GitHub sandbox
  quirks — keep one canonical copy.
- [ ] **Workspace assets go in `projects/<id>/` only.** Never write
  outside that root. (Already a rule in `portfolio`; should be
  documented as a global convention.)

### Confirmation gates (always)

- [ ] **For irreversible side effects** (git commit, send message,
  POST to external API, delete, merge, transfer): always require
  explicit confirmation in the skill body, even if frontmatter
  has `disable-model-invocation: true`. Defense in depth.
- [ ] **For agent autonomy paths** (`resolution_path: agent_can_handle`,
  auto-fix, auto-resolve): state explicitly what triggers them and
  what requires approval. Match the `mechanical | taste | challenge`
  vocabulary already used in `commitment-setup`.

### Forward compatibility

- [ ] **Mission goals are skill prompts, just deferred.** When
  embedding a goal string for `mission_create`, apply the same
  three-lens discipline. Don't bury deterministic logic in a goal
  string where it can't be tested.
- [ ] **Don't depend on the engine ignoring unknown frontmatter.**
  If you add a field that the loader doesn't parse today, mark it
  with a comment so a future engine patch can find and honor it.

---

## Appendix — quick reference table

| Skill | Edited? | Recommended change | Note |
|-------|---------|-------------------|------|
| `commit` | YES | applied `disable-model-invocation: true` | Engine doesn't enforce yet; signal-only |
| `github-workflow` | no | recommend `disable-model-invocation: true` | Installs cron without confirmation gate |
| `project-setup` | no | recommend `disable-model-invocation: true` | Wraps github-workflow |
| `local-test` | no | recommend `disable-model-invocation: true` + extract `scripts/` | Docker invocations are deterministic |
| `web-ui-test` | no | recommend `disable-model-invocation: true` | Has `rm -rf` step |
| `code-review` | no | recommend `disable-model-invocation: true` OR split into review-local + review-pr | PR comment posting is the side effect |
| `security-review` | no | recommend `disable-model-invocation: true` | Auto-fix path + auto-creates critical commitments |
| `qa-review` | no | recommend `disable-model-invocation: true` | Generates test code |
| `coding` | no | recommend `user-invocable: false` | Pure methodology |
| `review-checklist` | no | recommend `user-invocable: false` | Pure checklist |
| `routine-advisor` | no | recommend `user-invocable: false` | Behavioral guidance, not a procedure |
| `github` | no | recommend removing `"commit"` from keywords | Overlap with `commit` skill |
| `commitment-setup` | no | recommend extracting scaffolding to script | Six near-identical memory_write calls |
| `ceo-setup` | no | recommend moving widget files to `widget/` directory | Mirrors `portfolio` pattern |
| `developer-setup` / `content-creator-setup` / `trader-setup` | no | recommend shared `commitments-schema.md` asset | Schema drift risk |
| `delegation-tracker` / `tech-debt-tracker` | no | recommend shared `slugify` helper | Pure function in prompt space |
| All five `*-setup` | no | recommend hoisting common preamble out of mission goal strings | Composability |
| Others | no | no changes | Defaults are correct |
