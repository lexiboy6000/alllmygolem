//! "Get task data" — extract the prompt, reference files, starting-state files
//! and description, and the rubric JSON from the current task page; download all
//! files; persist a JSON bundle. Returns the bundle as the workflow result.

use crate::prelude::*;

use super::util;

pub struct GetTaskData;

#[async_trait]
impl Workflow for GetTaskData {
    fn name(&self) -> &'static str {
        "Get task data"
    }
    fn description(&self) -> &'static str {
        "Extract prompt, reference files, starting state, and rubric JSON for the current task."
    }
    fn dependencies(&self) -> Vec<&'static str> {
        vec!["Navigate to task"]
    }
    fn inputs(&self) -> Vec<InputSpec> {
        // Forwarded to the "Navigate to task" dependency; also a fallback if the
        // current page somehow isn't a task.
        vec![InputSpec::optional(
            "task_url",
            "Task URL (blank = use the page already open)",
            "",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        // The "Navigate to task" dependency has normally put us on the task page
        // already. As a safety net, confirm (waiting for the SPA to render) and
        // navigate ourselves if a URL was supplied and we're not there yet.
        ctx.step("ensure on a task page").await?;
        let wait = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        let mut on_task = util::wait_for_text(ctx, "[role=\"tab\"]", "Prompt definition", wait)
            .await
            .unwrap_or(false);
        if !on_task
            && let Some(url) = ctx
                .input("task_url")
                .map(str::to_string)
                .filter(|u| u.contains("feather.openai.com/tasks/"))
            {
                ctx.output(format!("not on a task page; navigating to {url}"));
                ctx.navigate(&url).await?;
                ctx.wait_for_default("body").await?;
                on_task = util::wait_for_text(ctx, "[role=\"tab\"]", "Prompt definition", wait)
                    .await
                    .unwrap_or(false);
            }
        if !on_task {
            return Err(ctx
                .stop_and_warn(
                    "Not on a loaded task page (no 'Prompt definition' tab found). Open the \
                     task in the controlled Chrome (and make sure you're logged in), or provide \
                     its URL in the task_url field, then retry. Tip: run `golem extract <url>` \
                     to inspect what the page exposes.",
                )
                .await);
        }

        // The task definition (prompt, reference files, rubric) lives under the
        // "Prompt definition" tab. These are Radix tabs that UNMOUNT inactive
        // content, and the /stage/execution URL opens with "Task execution"
        // active — so we must activate "Prompt definition" first and wait for the
        // prompt field to mount, or extraction sees an empty DOM.
        ctx.step("open the Prompt definition tab").await?;
        let _ = util::click_contains(ctx, "[role=\"tab\"]", "Prompt definition").await;
        // Wait for the prompt textarea (or its name fallback) to appear.
        let _ = ctx
            .wait_for("#root_task_prompt, textarea[name=\"task_prompt\"]", wait)
            .await;
        ctx.human_pause(300, 700).await?;

        ctx.step("read prompt and file lists").await?;
        let data = util::eval_fn(ctx, EXTRACT_JS).await?;

        let prompt = data
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if prompt.trim().is_empty() {
            ctx.warn("task prompt appears to be empty");
        }

        let reference_files = collect_files(data.get("referenceFiles"));
        let input_files = collect_files(data.get("inputFiles"));
        let anticipated_hours = data.get("anticipatedHours").and_then(Value::as_f64);
        if let Some(h) = anticipated_hours {
            ctx.output(format!("anticipated duration: {h} hour(s)"));
        }
        let starting_state_text = data
            .get("startingStateText")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let has_starting_state = data
            .get("hasStartingState")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || !input_files.is_empty()
            || !starting_state_text.trim().is_empty();

        ctx.output(format!(
            "found {} reference file(s), {} starting-state file(s)",
            reference_files.len(),
            input_files.len()
        ));
        if prompt.trim().is_empty() && reference_files.is_empty() && input_files.is_empty() {
            ctx.warn(
                "extraction found no prompt and no files - the page DOM may differ from the \
                 expected structure; run `golem extract <task_url>` to inspect it.",
            );
        }

        // --- download everything ---
        ctx.step("download files").await?;
        let mut downloaded: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for (label, files) in [("reference", &reference_files), ("starting-state", &input_files)] {
            for f in files {
                if f.url.is_empty() {
                    continue;
                }
                match ctx.download(&f.url, &nonempty(&f.name, "download.bin")).await {
                    Ok(path) => downloaded.push(path.display().to_string()),
                    Err(e) => {
                        ctx.warn(format!("failed to download {label} file '{}': {e}", f.name));
                        failures.push(f.name.clone());
                    }
                }
            }
        }

        if !has_starting_state {
            ctx.output("no starting state present (this is fine); proceeding.");
        }

        // --- rubric (page copies JSON to the clipboard) ---
        ctx.step("copy rubric").await?;
        let rubric = self.copy_rubric(ctx).await;

        // --- assemble + persist ---
        let task_url = util::href(ctx).await.unwrap_or_default();
        let bundle = json!({
            "task_url": task_url,
            "prompt": prompt,
            "has_starting_state": has_starting_state,
            "starting_state": starting_state_text,
            "rubric": rubric,
            "reference_files": files_json(&reference_files),
            "starting_state_files": files_json(&input_files),
            "anticipated_hours": anticipated_hours,
            "downloaded": downloaded,
            "download_failures": failures,
        });

        if let Err(e) = self.persist(ctx, &task_url, &bundle) {
            ctx.warn(format!("could not save task bundle to disk: {e}"));
        }

        if !failures.is_empty() {
            ctx.warn_user(format!(
                "{} file(s) failed to download: {}. The rest of the task data was captured.",
                failures.len(),
                failures.join(", ")
            ))
            .await?;
        }

        Ok(WorkflowOutcome::CompletedWith(bundle))
    }
}

impl GetTaskData {
    /// Click the rubric copy button and capture the copied JSON. The page copies
    /// to the clipboard; rather than depend on the OS clipboard (unreliable on
    /// some Linux/Wayland setups) we hook `clipboard.writeText` / `execCommand`
    /// to capture the value in-page, falling back to the OS clipboard.
    async fn copy_rubric(&self, ctx: &mut WorkflowCtx) -> Value {
        if !ctx
            .exists("[data-testid=\"ContentCopyIcon\"]")
            .await
            .unwrap_or(false)
        {
            ctx.warn("no rubric copy button found; skipping rubric");
            return Value::Null;
        }
        // Install in-page hooks before clicking.
        let _ = ctx.eval(CLIPBOARD_HOOK_JS).await;
        if let Err(e) = ctx.click("[data-testid=\"ContentCopyIcon\"]").await {
            ctx.warn(format!("failed to click rubric copy button: {e}"));
            return Value::Null;
        }
        let _ = ctx.human_pause(400, 900).await;

        // Prefer the value captured in-page; fall back to the OS clipboard.
        let captured = ctx
            .eval("window.__golem_clip")
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|s| !s.trim().is_empty());
        let text = match captured {
            Some(t) => Some(t),
            None => ctx.clipboard_read().ok().filter(|s| !s.trim().is_empty()),
        };
        match text {
            Some(t) => serde_json::from_str::<Value>(&t).unwrap_or(Value::String(t)),
            None => {
                ctx.warn("could not capture rubric (clipboard hook and OS clipboard both empty)");
                Value::Null
            }
        }
    }

    /// Save the bundle under the configured data dir as `task-<id>.json`.
    fn persist(&self, ctx: &WorkflowCtx, task_url: &str, bundle: &Value) -> Result<()> {
        let dir = ctx.settings.data_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dir.display())))?;
        let id = task_id_from_url(task_url).unwrap_or_else(|| ctx.run_id.clone());
        let path = dir.join(format!("task-{id}.json"));
        let text = serde_json::to_string_pretty(bundle)?;
        std::fs::write(&path, text)
            .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
        ctx.output(format!("saved task data -> {}", path.display()));
        Ok(())
    }
}

/// A reference/starting-state file discovered on the page.
#[derive(Clone)]
struct FileRef {
    name: String,
    url: String,
}

fn collect_files(value: Option<&Value>) -> Vec<FileRef> {
    let mut out = Vec::new();
    if let Some(Value::Array(items)) = value {
        for item in items {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let url = item
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !name.is_empty() || !url.is_empty() {
                out.push(FileRef { name, url });
            }
        }
    }
    out
}

fn files_json(files: &[FileRef]) -> Value {
    Value::Array(
        files
            .iter()
            .map(|f| json!({ "name": f.name, "url": f.url }))
            .collect(),
    )
}

fn nonempty<'a>(s: &'a str, fallback: &'a str) -> String {
    if s.trim().is_empty() {
        fallback.to_string()
    } else {
        s.to_string()
    }
}

/// Pull the task id out of a `.../tasks/<id>/...` URL.
fn task_id_from_url(url: &str) -> Option<String> {
    let after = url.split("/tasks/").nth(1)?;
    let id = after.split(['/', '?', '#']).next().unwrap_or("");
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Hooks the page's clipboard-write paths so a subsequent "copy" captures the
/// copied text into `window.__golem_clip` (avoids depending on the OS clipboard).
const CLIPBOARD_HOOK_JS: &str = r#"(function(){
  window.__golem_clip = null;
  try {
    var cb = navigator.clipboard;
    if (!cb) { cb = {}; try { Object.defineProperty(navigator, 'clipboard', { value: cb, configurable: true }); } catch (e2) {} }
    cb.writeText = function(t){ window.__golem_clip = String(t); return Promise.resolve(); };
  } catch (e) {}
  try {
    var orig = document.execCommand ? document.execCommand.bind(document) : null;
    if (orig) {
      document.execCommand = function(cmd){
        if (cmd === 'copy') {
          var s = window.getSelection ? window.getSelection().toString() : '';
          if (s) window.__golem_clip = s;
        }
        return orig.apply(document, arguments);
      };
    }
  } catch (e) {}
  return true;
})()"#;

/// Extracts the prompt, starting-state text, and reference/input file lists from
/// a task page. Best-effort: walks `ul.file-info` lists within each titled
/// section, reading the filename (`strong`) and a URL (download anchor or image).
/// `pub(crate)` so the `golem extract <url>` diagnostic can exercise it directly.
pub(crate) const EXTRACT_JS: &str = r#"
function txt(el) { return el ? (el.textContent || '').trim() : ''; }
function sectionFiles(titleId) {
    var t = document.getElementById(titleId);
    if (!t) return [];
    var box = t.parentElement;
    for (var k = 0; k < 6 && box; k++) {
        if (box.querySelector && box.querySelector('ul.file-info')) break;
        box = box.parentElement;
    }
    if (!box) return [];
    var lis = box.querySelectorAll('ul.file-info > li');
    var out = [];
    for (var i = 0; i < lis.length; i++) {
        var li = lis[i];
        var name = txt(li.querySelector('strong'));
        var a = li.querySelector('a[download]');
        var img = li.querySelector('img');
        var url = a ? a.href : (img ? img.src : '');
        if (name || url) out.push({ name: name, url: url });
    }
    return out;
}
function fieldText(el) { return el ? (el.value || el.textContent || '') : ''; }
// Best-effort: the "How long would you anticipate this task taking an 'Employee'
// to complete? (in hours)" field. Try a direct field, then a label-text search,
// then a number embedded in the question text. Returns a Number or null.
function anticipatedHours() {
    function num(s){ var n = parseFloat(String(s).replace(/[^0-9.]/g, '')); return isNaN(n) ? null : n; }
    var direct = document.querySelector('#root_time_approximation, [name="root_time_approximation"], #root_anticipated_hours, [name="anticipated_hours"], input[aria-label*="anticipat" i]');
    if (direct) { var d = num(fieldText(direct)); if (d !== null) return d; }
    var nodes = document.querySelectorAll('label, span, div, p, h3, h4, legend');
    for (var i = 0; i < nodes.length; i++) {
        var t = (nodes[i].textContent || '').toLowerCase();
        if (t.indexOf('anticipat') === -1 || t.indexOf('hour') === -1) continue;
        var box = nodes[i];
        for (var k = 0; k < 4 && box; k++) {
            var inp = box.querySelector && box.querySelector('input, textarea, [role="spinbutton"]');
            if (inp) { var v = num(inp.value || inp.textContent); if (v !== null) return v; }
            box = box.parentElement;
        }
        var m = t.match(/([0-9]+(\.[0-9]+)?)\s*hour/);
        if (m) { var hm = num(m[1]); if (hm !== null) return hm; }
    }
    return null;
}
var promptEl = document.getElementById('root_task_prompt')
    || document.querySelector('textarea[name="task_prompt"]')
    || document.querySelector('textarea[aria-label*="rompt"]');
var ssEl = document.getElementById('root_input_description')
    || document.querySelector('textarea[name="input_description"]');
var refs = sectionFiles('root_reference_files__title');
var ins = sectionFiles('root_input_files__title');
var ssText = fieldText(ssEl);
return {
    prompt: fieldText(promptEl),
    startingStateText: ssText,
    hasStartingState: !!ssText.trim() || ins.length > 0,
    referenceFiles: refs,
    inputFiles: ins,
    anticipatedHours: anticipatedHours()
};
"#;
