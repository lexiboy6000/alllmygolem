# Golem

A stability-first workflow automation tool. Golem attaches to a **real, running
Chrome session** over the DevTools Protocol (CDP) and drives it with
**human-like mouse and keyboard motion** (arcs, easing, jitter, variable timing —
not the instantaneous "taps" you get from raw DevTools input). It can also inject
**native OS input** for things outside the browser (OS file dialogs, etc.).

It is built for multi-hour unattended runs, so the #1 design goal is that the
process **cannot panic**: every fallible operation returns a `Result`, the engine
isolates panics, and progress is checkpointed to disk after every step.

- **GUI** (egui): workflow list, live output/log, progress, settings, and a
  modal for prompts.
- **Movable always-on-top overlay**: shown while a workflow runs, so you can
  Stop / Pause / Resume or answer a prompt without the main window.
- **Extensible workflows**: a workflow is one small Rust file implementing the
  `Workflow` trait against a rich `WorkflowCtx` API (or the raw CDP / native
  escape hatches for *anything*).

## Architecture

```
main thread  ── egui GUI ───────────┐  UiCommand   ┌── engine (tokio runtime, bg thread)
             ◄── EngineEvent ────────┘ (channels)   └── CdpBrowser (attach + auto-reconnect + relaunch)
                                                        NativeInput (enigo on a dedicated thread)
                                                        WorkflowCtx → your Workflow::run()
```

The GUI and engine never touch each other's state — they only exchange messages.
A panic in any worker task is caught and logged; the process keeps running.
See `docs/STABILITY.md`.

## Prerequisites

- Rust (stable). Built/tested with 1.95.
- Google Chrome or Chromium.
- Linux build note: native input uses enigo's pure-Rust `x11rb` backend, so no
  `libxdo`/`xdotool` system package is required.

## 1. Launch Chrome with remote debugging

Golem attaches to your existing logged-in session; you start Chrome with a debug
port (use a normal profile so you stay logged in):

- **Linux/macOS:**
  ```bash
  google-chrome --remote-debugging-port=9222
  # or: chromium --remote-debugging-port=9222
  ```
- **Windows:**
  ```bat
  "C:\Program Files\Google\Chrome\Application\chrome.exe" --remote-debugging-port=9222
  ```

Verify it's up: open `http://localhost:9222/json/version`.

If you enable **Auto-relaunch Chrome** in Settings and set the Chrome binary
path, Golem will restart Chrome with the debug flag if it dies mid-run.

## 2. Build & run

```bash
cargo build --release
./target/release/golem
```

### Verify connectivity (no GUI)

```bash
# With a debug-enabled Chrome running on :9222
./target/release/golem selftest
```

This connects, opens a throwaway page, evaluates JS, finds an element, and
clicks it with human-like motion — printing each step. Useful for confirming
the CDP path before a real run.

To see exactly what "Get task data" extracts from a task page (run it against
your live, logged-in Chrome on the debug port):

```bash
./target/release/golem extract "https://feather.openai.com/tasks/<id>/stage/execution"
```

It prints the detected tabs, whether the rubric copy button is present, and the
extracted prompt / reference files / starting-state — handy for diagnosing a
page whose DOM differs from what the workflow expects.

## 3. Using the GUI

1. **Connect** (top bar) — attaches to Chrome on the configured host/port.
   The connection indicator shows attach/reconnect/relaunch state.
2. Pick a workflow in the left panel, fill any **input fields** it declares,
   and press **Run**. If the workflow has prerequisite workflows (dependencies,
   resolved recursively), Golem first asks you to confirm running them; they
   then run in order before the one you picked. A workflow with no dependencies
   runs with no prompt.
3. Watch the **output log** and **progress** in the centre. Use **Stop /
   Pause / Resume** any time (also on the overlay).
4. When a workflow needs you, a **prompt** appears (in the main window and the
   overlay): free text, yes/no, a choice, or an acknowledgement.
5. **Settings** (gear): Chrome host/port, auto-relaunch + Chrome path, output
   directory, input strategy, human-motion tuning, timeouts, and reconnect
   behaviour. **Apply** saves them.

### Input strategy

- **Cdp** (default): clicks/typing are dispatched via CDP along a human path.
  Robust for unattended runs, needs no window focus, and works by coordinate
  into an iframe/remote-desktop canvas where there is no queryable DOM.
- **Native** / **Hybrid**: route pointer (and, for Native, keyboard) through the
  real OS cursor via enigo. Best on Windows/X11; second-tier on Wayland.

## Included workflows (feather.openai.com)

Per `docs/WORKFLOWS_20260618.md`:

| Workflow | Depends on | Inputs | What it does |
|---|---|---|---|
| **Navigate and verify integrity** | – | `expected_sha` (default `783e85f8fe5`) | Opens feather, checks `data-git-sha`; on mismatch asks whether to continue. |
| **Navigate back to homepage** | – | – | Clicks "To do" (or the home button); verifies the homepage URL. |
| **Claim task** | Navigate and verify integrity | – | Campaigns → Visual Demos v2 → Ngspice batch, verifying the URL at each hop. *(Runs "Navigate back to homepage" afterward. The actual claim steps are TODO in the spec, so it stops with a notice after selecting the batch.)* |
| **Navigate to task** | Navigate and verify integrity | `task_url` (optional; blank = verify the open page) | Navigates to a task URL (or verifies the current page) and confirms the "Prompt definition" + "Task execution" tabs. |
| **Get task data** | none (self-contained) | `task_url` (optional; blank = use the open page) | Works on whatever task page is open (or navigates to `task_url` if given and the current page isn't a task). Extracts the prompt, reference files, starting-state files + description, and the rubric JSON (copy button → clipboard); downloads all files; saves a JSON bundle to `golem-output/data/`. Halts with a clear message if the page isn't a loaded task. |

"STOP and warn/prompt user" steps surface a message and halt the chain.

## Adding a new workflow

Create `src/workflows/<area>/my_workflow.rs`:

```rust
use crate::prelude::*;

pub struct MyWorkflow;

#[async_trait]
impl Workflow for MyWorkflow {
    fn name(&self) -> &'static str { "My workflow" }
    fn description(&self) -> &'static str { "Does the thing." }
    fn dependencies(&self) -> Vec<&'static str> { vec![] }        // optional
    fn inputs(&self) -> Vec<InputSpec> {                          // optional
        vec![InputSpec::required("url", "Target URL")]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        ctx.step("open page").await?;                 // marks + checkpoints a step
        ctx.navigate(&ctx.require_input("url")?).await?;
        ctx.wait_for_default("#submit").await?;

        ctx.step("interact").await?;
        ctx.type_into("#name", "Ada").await?;         // human typing
        ctx.click("#submit").await?;                  // human-path click on an element
        ctx.click_at(640.0, 480.0).await?;            // click by coordinate (e.g. into an iframe)

        if !ctx.confirm("Looks right?").await? {
            return Err(ctx.stop_and_warn("user rejected").await);
        }
        let title = ctx.eval("document.title").await?;  // arbitrary JS
        ctx.set("title", title)?;                       // store (persisted in checkpoints)
        Ok(WorkflowOutcome::Completed)
    }
}
```

Then register it in `src/workflows/mod.rs`:

```rust
registry.register(Arc::new(my_area::MyWorkflow));
```

It appears in the GUI automatically. The full `WorkflowCtx` API (in
`src/context.rs`) includes `navigate`, `wait_for`, `exists`, `attr`, `text`,
`eval`, `click`, `click_at`, `double_click_at`, `type_into`, `type_human`,
`press_key`, `scroll`, `move_to`, `download`, `clipboard_read/write`,
`human_pause`, `prompt_text`, `confirm`, `choose`, `warn_user`, `halt`,
`stop_and_warn`, `set`/`get`/`input`, and the raw `ctx.browser` / `ctx.input`
escape hatches.

## Stability features

- No `unwrap`/`expect`/`panic!` in our code — enforced by `cargo clippy`
  (`[lints.clippy]` in `Cargo.toml`). Profiles keep `panic = "unwind"`.
- A non-aborting panic hook logs panics; the engine catches panics per workflow.
- **Checkpoints** are written atomically after every `ctx.step(...)` to
  `golem-output/checkpoints/`. On startup Golem offers to resume the latest.
- **CDP auto-reconnect** with exponential backoff, and optional Chrome relaunch.

## Output layout

```
golem-output/
  logs/         rolling daily logs (golem.log.<date>)
  checkpoints/  per-run resumable state (deleted on clean completion)
  downloads/    files fetched by workflows
  data/         saved task-data bundles (e.g. task-<id>.json)
```
Settings file: your platform config dir (`.../golem/Golem/settings.json`).

## Known limitations

- Wayland native input and macOS are second-tier (macOS needs Accessibility
  permission). CDP-strategy automation is unaffected.
- The "Claim task" workflow stops after selecting the Ngspice batch because the
  remaining steps are unspecified (TODO) in the source spec.
