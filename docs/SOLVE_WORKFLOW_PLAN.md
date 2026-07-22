# Plan: "Solve task" workflow(s)

Status: **IMPLEMENTED** (2026-06-18). Builds clean; clippy no-panic gate passes.
Not yet run end-to-end here (this sandbox has no Docker daemon and shouldn't spawn
a nested Claude Code) — run it on a machine with Docker up.

Locked choices: preflight + one orchestrator with internal auto-retry loops;
plots via **ngspice's built-in `gnuplot` command → PNG** (no Xvfb needed, just
gnuplot); review-failure → **auto-retry with feedback up to N (≈3), then STOP +
prompt**; Claude Code runs with **`--dangerously-skip-permissions`** in the
sandboxed workspace; Claude Code runs on the **host**, ngspice via `docker exec`.

## Goal & product

Take the task bundle produced by **Get task data** (prompt, rubric JSON,
reference images, starting-state files) and autonomously produce a **final,
solved ngspice netlist** that satisfies every rubric item — using Claude Code +
a Docker (Ubuntu + ngspice) sandbox, with independent review agents gating
completion. The product is `final/solution.cir` (+ plots + verdicts).

All tasks are SPICE, so the toolchain is fixed: ngspice for simulation, gnuplot
for plots, `wrdata` acceptable for plot data.

## High-level pipeline

```
[Get task data] -> bundle (prompt, rubric, ref images, starting state)
        |
   (A) PREFLIGHT  ── verify EVERYTHING up front, STOP on any gap
        |
   (B) SOLVE ORCHESTRATOR
        ├─ 1. Provision: workspace + copy files + docker run (bind mount)
        ├─ 2. Solve+Review loop (≤ N):
        │       claude-code solve  ->  review agent #1 (complete? unmet items?)
        │       if not complete -> feed feedback back, retry
        ├─ 3. Plots: clean SPICE + plotting deck -> ngspice -> plots/*.png
        ├─ 4. Plot-Review loop (≤ M):
        │       review agent #2 (vision: plots vs rubric) -> verdict
        │       if not complete -> feed feedback to solve (back to 2)
        └─ 5. Finalize: copy final netlist + plots + verdicts; cleanup
        |
   PRODUCT: final/solution.cir
```

## Decomposition into Golem workflows

Golem workflows run **once** and chain via dependencies (acyclic). The
review→retry **loops** don't fit a pure run-once chain, so:

**Recommended:** two workflows —
- **`Solve: preflight`** — all dependency/environment checks + image build.
- **`Solve task (Claude + Docker)`** — orchestrator that owns phases 1–5 and the
  internal retry loops. Depends on `Solve: preflight`.

**Alternative (simpler, no auto-retry):** five small workflows chained by
dependency — `Provision` → `Solve` → `Review (completeness)` → `Plots` →
`Review (plots)`. Each review **STOPs and prompts you** on failure instead of
auto-retrying; you re-run the chain manually after Claude tries again. Simpler
code, more babysitting. *(This is decision Q4.)*

Both options need `Get task data` to have run; the solve workflow reads the saved
bundle from disk (`golem-output/data/task-<id>.json`) rather than relying on
in-memory state.

## (A) Preflight — catch every error up front

STOP + prompt with a specific message on the first failure. Checks:

1. **Claude Code**: `claude --version` resolves on PATH.
2. **Claude Code auth/works**: `claude -p "reply OK"` returns within a timeout
   (catches not-logged-in / quota / network).
3. **Docker present**: `docker --version`.
4. **Docker daemon reachable**: `docker info` (also catches permission/group).
5. **ngspice image**: ensure `golem-ngspice:latest` exists; build from the
   shipped Dockerfile if missing (needs network for apt).
6. **Image contents**: `docker run --rm golem-ngspice:latest sh -c "ngspice -v && gnuplot --version"` — ngspice + gnuplot required (gnuplot drives the PNG plots). Xvfb is installed as a fallback but not hard-required.
7. **Task bundle**: the bundle JSON exists and parses; referenced reference/
   starting-state files exist in `golem-output/downloads/` (or re-download).
8. **Workspace writable**: can create `golem-output/solve/<id>/`.
9. **Disk space**: a few hundred MB free (best-effort warning).

Output: a validated context (bundle path, image tag) for the orchestrator.

## (B) Orchestrator phases

### Phase 1 — Provision
- Create workspace `golem-output/solve/<task-id>/` containing:
  `prompt.txt`, `rubric.json`, `reference_*.png` (copied from downloads),
  starting-state files, and `INSTRUCTIONS.md` (the solve brief).
- `docker run -d --name golem-ngspice-<id> -v <workspace-abs>:/work golem-ngspice:latest sleep infinity`.
- Claude Code runs on the **host** in the workspace dir and runs ngspice via
  `docker exec golem-ngspice-<id> ngspice -b /work/<file>` *(decision Q1: host vs
  in-container Claude)*.

### Phase 2 — Solve + completeness review (loop ≤ N)
- **Solve agent** (one `claude -p` invocation = one full agentic turn):
  - cwd = workspace; prompt instructs it to read `prompt.txt`, `rubric.json`,
    `reference_*.png`, starting state; write `solution.cir`; declare device
    models **inline**; iterate by running ngspice **in the container** until it
    runs with no errors/warnings and meets the rubric (DC op point; transient
    ≥ 500 ms showing slow-change non-trigger, short spike non-trigger, qualified
    rapid-change trigger, return-to-normal); use `wrdata` for outputs. Finish
    when confident.
  - It writes the netlist to a known path (`solution.cir`) so Golem finds it.
- **Review agent #1** (a *fresh* `claude -p`, independent session) reads
  `solution.cir` + the ngspice run log + `rubric.json` + `prompt.txt`, may re-run
  ngspice itself, and writes `review_solve.json`:
  `{ "complete": bool, "unmet_items": [...], "feedback": "..." }`.
- If `complete` → exit loop. Else append the feedback and re-run the solve agent
  (`claude -p "Continue in this workspace. A reviewer found: <feedback>. Fix the
  unmet rubric items and re-test."`). Repeat up to **N** (default 3).

### Phase 3 — Plots
- Produce a clean final SPICE netlist + a plotting control deck that runs the
  required analyses and emits each required signal (input, rate-of-change node,
  threshold ref, qualification node, decision output, RAPID CHANGE output).
- Render to `plots/*.png` using **ngspice's `gnuplot` command** (e.g.
  `gnuplot plots/roc v(roc) v(thresh)` inside a `.control` block), which writes
  PNGs via gnuplot with no display. `wrdata` may also be dumped alongside for the
  reviewer to inspect raw values.

### Phase 4 — Plot review (loop ≤ M)
- **Review agent #2** (vision): reads `plots/*.png` + `rubric.json` + `prompt.txt`
  and writes `review_plots.json` `{ "complete": bool, "issues": [...], "feedback": "..." }`.
- If `complete` → done. Else feed the feedback back to the solve agent (Phase 2)
  and regenerate plots. Bounded by a global iteration budget *(decision Q3)*.

### Phase 5 — Finalize
- Copy `solution.cir`, `plots/*`, `review_*.json` to
  `golem-output/solve/<id>/final/`.
- Cleanup container *(decision: remove on success, keep on failure for
  inspection — default)*.
- If iterations exhausted without success → STOP and prompt you with the latest
  verdict + artifact paths.

## Docker image (shipped Dockerfile)

```dockerfile
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
      ngspice gnuplot ca-certificates xvfb \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
```
(`gnuplot` is required for PNG plots; `xvfb` kept only as a fallback.)
Tagged `golem-ngspice:latest`. Built in preflight if absent. (Image is reused
across tasks; a fresh **container** is created per task.)

## Claude Code invocation details

- Solve and both reviews are separate `claude -p` processes (reviews are
  independent sessions, so they aren't biased by the solver's reasoning).
- Non-interactive autonomy needs permissions handled *(decision Q5)*:
  `--dangerously-skip-permissions` (full tool autonomy in the sandboxed
  workspace) vs a curated `--allowedTools "Bash Read Write Edit"`.
- Each agent writes a **known artifact** (`solution.cir`, `review_solve.json`,
  `review_plots.json`) that Golem reads — no fragile stdout parsing.
- Model configurable; default latest Opus for solve, Opus/Sonnet for review.
- Long runs stream output to the Golem log and honor Stop/Pause (kill the child).

## Golem engine/ctx additions needed

1. **Process execution in `WorkflowCtx`**: `ctx.run(program, args, cwd, timeout)`
   capturing stdout/stderr/exit, plus a streaming variant that logs lines live;
   kills the child on Stop. (Today `WorkflowCtx` has no shell capability.)
2. **Browser-optional workflows**: add `Workflow::requires_browser() -> bool`
   (default true). Solve workflows return `false` so the engine runs them without
   a Chrome connection (uses a no-op browser backend, like `NoopInput`).
3. Per-phase checkpointing (iteration count, current phase) so a crash resumes
   mid-pipeline; the on-disk workspace already persists artifacts.

## Artifact layout

```
golem-output/solve/<task-id>/
  prompt.txt  rubric.json  reference_*.png  <starting-state files>
  INSTRUCTIONS.md
  solution.cir            # produced/iterated by the solve agent
  ngspice.log
  review_solve.json
  plots/*.png
  review_plots.json
  final/                  # the product
    solution.cir  plots/*  review_*.json  summary.md
```

## Locked decisions

- **Claude Code location**: host, ngspice via `docker exec`.
- **Plot method**: ngspice's `gnuplot` command → PNG (no Xvfb).
- **Review-failure**: auto-retry with feedback up to N (≈3), then STOP + prompt.
- **Decomposition**: preflight workflow + one orchestrator (internal loops).
- **Claude Code autonomy**: `--dangerously-skip-permissions`.
- **Misc defaults**: ubuntu:24.04; remove container on success / keep on failure;
  global iteration cap (≈5) + per-`claude` timeout (≈15 min); models configurable
  via Settings (default latest Opus for solve, Sonnet/Opus for review).

## Out of scope (for now)
- Submitting the solution back to feather (this workflow stops at the netlist).
- Non-SPICE task types.
