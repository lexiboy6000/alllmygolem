//! Small helpers shared by the task1/task_data/responseA/responseB workflows
//! (create_task1.rs, task_data.rs, create_response_a_dir.rs,
//! download_response_a.rs, download_response_b.rs).
//!
//! IMPORTANT Golem quirk this file works around: when workflows are chained
//! via `dependencies()`, the engine builds a BRAND NEW `WorkflowCtx` for each
//! one (see `src/engine/chain.rs`) -- so `ctx.set(...)` in one workflow is
//! NOT visible via `ctx.get(...)` in the next, and the shared `inputs` map is
//! fixed before the chain even starts (it can't hold a value one workflow
//! decides partway through, like an auto-incremented folder name). So the
//! task folder name is decided ONCE by "1. Create task1 directory"
//! (`resolve_or_create_task_dir`: task1, task2, task3, ... auto-incrementing
//! by checking what already exists in the output dir) and written to a
//! marker file on disk; every other workflow reads it back
//! (`current_task_dir`). A file is the one thing that reliably crosses the
//! fresh-ctx-per-workflow boundary.

use serde::Deserialize;

use crate::prelude::*;

/// Quote a Rust string as a safe JS string literal (e.g. `a` -> `"a"`).
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

// ----- step 7: ask Claude, then click its answers in ----------------------

/// One criterion's judgment, as Claude is instructed to write it to the
/// `claude_answers` file (see `ANSWER_CRITERIA_PROMPT`).
#[derive(Deserialize)]
pub struct CriterionAnswer {
    pub number: u32,
    pub response_a: String,
    pub response_b: String,
    #[serde(default)]
    pub notes: String,
}

/// The page's separate "Overall Quality" pick ("Which generation better
/// fulfills the task requirements?" -> Response A / Response B / Tie).
#[derive(Deserialize)]
pub struct OverallAnswer {
    /// Exactly "Response A", "Response B", or "Tie" (matches the page's button text).
    pub winner: String,
    #[serde(default)]
    pub notes: String,
}

/// The full `claude_answers` file: per-criterion judgments plus the overall pick.
#[derive(Deserialize)]
pub struct ClaudeAnswers {
    pub criteria: Vec<CriterionAnswer>,
    pub overall: OverallAnswer,
}

/// Run `claude` (the Claude Code CLI) as a subprocess, cwd'd into `task_dir`,
/// and have it read everything under task1 and write its judgments to
/// `task_dir/claude_answers`. Reuses the same launcher settings as the Solve
/// pipeline (Settings > Claude path / model / effort / timeout) -- this isn't
/// a "solve" workflow, but those settings are general-purpose, not solve-only.
pub async fn ask_claude_for_answers(ctx: &WorkflowCtx, task_dir: &std::path::Path) -> Result<()> {
    let claude = if ctx.settings.claude_path.trim().is_empty() {
        "claude".to_string()
    } else {
        ctx.settings.claude_path.clone()
    };
    let model = ctx.settings.solve_model.clone();
    let effort = ctx.settings.solve_effort.clone();
    let mut args: Vec<&str> = vec![
        "-p",
        ANSWER_CRITERIA_PROMPT,
        "--dangerously-skip-permissions",
        "--output-format",
        "stream-json",
        "--verbose",
    ];
    if !model.trim().is_empty() {
        args.push("--model");
        args.push(model.as_str());
    }
    if !effort.trim().is_empty() {
        args.push("--effort");
        args.push(effort.as_str());
    }
    let timeout = Duration::from_secs(ctx.settings.claude_timeout_secs.max(60));
    let out = ctx.run_claude(&claude, &args, Some(task_dir), Some(timeout)).await?;
    if !out.success() {
        return Err(GolemError::Other(format!(
            "claude exited with an error: {}",
            out.combined().trim()
        )));
    }
    Ok(())
}

/// Read + parse `claude_answers` written by `ask_claude_for_answers`.
pub fn read_claude_answers(path: &std::path::Path) -> Result<ClaudeAnswers> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| GolemError::Io(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&text).map_err(|e| {
        GolemError::Other(format!(
            "parse {}: {e} (claude may not have written the exact JSON shape asked for)",
            path.display()
        ))
    })
}

/// Find and click the `want` ("Good"/"Bad") button for `response_label`
/// ("Response A"/"Response B") in the row numbered `number` under the
/// Evaluation Criteria list. Returns `Ok(false)` if that exact button
/// couldn't be found (page structure differs, or the row/label doesn't
/// exist) rather than clicking something wrong.
pub async fn click_criterion_button(
    ctx: &mut WorkflowCtx,
    number: u32,
    response_label: &str,
    want: &str,
) -> Result<bool> {
    let js = CLICK_CRITERION_BUTTON_JS
        .replace("__NUM__", &js_str(&format!("{number}.")))
        .replace("__RESP__", &js_str(response_label))
        .replace("__WANT__", &js_str(want));
    let v = ctx.eval(&js).await?;
    match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => {
            ctx.click_at(x, y).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Find and click the Overall Quality button matching `want` ("Response A",
/// "Response B", or "Tie"), in the card headed by an `<h3>Overall Quality</h3>`.
pub async fn click_overall_button(ctx: &mut WorkflowCtx, want: &str) -> Result<bool> {
    let js = CLICK_OVERALL_BUTTON_JS.replace("__WANT__", &js_str(want));
    let v = ctx.eval(&js).await?;
    match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => {
            ctx.click_at(x, y).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Find the Submit button (identified by being next to the "Skip" button)
/// and click it IF it's currently enabled. Returns `Ok(false)` if it's
/// missing or still disabled (e.g. not every criterion got rated).
pub async fn click_submit_if_enabled(ctx: &mut WorkflowCtx) -> Result<bool> {
    let v = ctx.eval(FIND_SUBMIT_JS).await?;
    match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => {
            ctx.click_at(x, y).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

const ANSWER_CRITERIA_PROMPT: &str = "You are evaluating two AI-generated responses to a task, \
laid out in the current directory:\n\
- task_data/information -- the task description (what was asked for)\n\
- task_data/ -- reference files for the task, if any\n\
- task_data/evaluation_criteria/questions -- the numbered evaluation criteria, one per line \
(\"1. ...\", \"2. ...\", etc.)\n\
- responseA/ -- Response A's submitted files (read every file, especially the HTML deliverable)\n\
- responseB/ -- Response B's submitted files (read every file, especially the HTML deliverable)\n\n\
For EVERY criterion listed in task_data/evaluation_criteria/questions, judge Response A and \
Response B independently: does that response satisfy the criterion? Use \"Good\" if it does, \
\"Bad\" if it does not or only partially does.\n\n\
Separately, also give an OVERALL pick: which response, taken as a whole, better fulfills the \
task requirements -- \"Response A\", \"Response B\", or \"Tie\" if they're genuinely equal.\n\n\
Write your answer as a JSON OBJECT to a file named exactly `claude_answers` in the CURRENT \
directory (no file extension), in EXACTLY this shape:\n\
{\"criteria\": [{\"number\": 1, \"response_a\": \"Good\", \"response_b\": \"Bad\", \"notes\": \
\"one short sentence why\"}, ...], \"overall\": {\"winner\": \"Response A\", \"notes\": \"one \
short sentence why\"}}\n\
The \"criteria\" array must have one object per criterion, in the SAME order and using the SAME \
number as in the questions file. \"overall.winner\" must be EXACTLY one of \"Response A\", \
\"Response B\", or \"Tie\".\n\n\
Output ONLY that file -- do not print the JSON to stdout, do not add commentary elsewhere. Be \
strict and specific in your judgment.";

const CLICK_CRITERION_BUTTON_JS: &str = r#"(function(){
  var NUM = __NUM__;
  var RESP = __RESP__;
  var WANT = __WANT__;

  var spans = document.querySelectorAll('span');
  var header = null;
  for (var i = 0; i < spans.length; i++) {
    if ((spans[i].textContent || '').trim() === 'Evaluation Criteria') { header = spans[i]; break; }
  }
  if (!header) return null;
  var card = header.closest('[class*="rounded-lg"]') || header.parentElement;
  var list = card ? card.querySelector('.divide-y') : null;
  if (!list) return null;

  var rows = list.children;
  for (var j = 0; j < rows.length; j++) {
    var row = rows[j];
    var head = row.querySelector('.flex.gap-2') || row.firstElementChild;
    var kids = head ? head.children : [];
    var num = kids[0] ? (kids[0].textContent || '').trim() : '';
    if (num !== NUM) continue;

    var allDivs = row.querySelectorAll('div');
    for (var g = 0; g < allDivs.length; g++) {
      var grp = allDivs[g];
      var kids2 = grp.children;
      if (kids2.length < 3) continue;
      if (kids2[0].tagName !== 'SPAN') continue;
      if ((kids2[0].textContent || '').trim() !== RESP) continue;
      for (var b = 1; b < kids2.length; b++) {
        if (kids2[b].tagName === 'BUTTON' && (kids2[b].textContent || '').trim() === WANT) {
          var e = kids2[b];
          try { e.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
          var r = e.getBoundingClientRect();
          if (r.width < 1 || r.height < 1) return null;
          return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
        }
      }
    }
    return null;
  }
  return null;
})()"#;

const CLICK_OVERALL_BUTTON_JS: &str = r#"(function(){
  var WANT = __WANT__;
  var h3s = document.querySelectorAll('h3');
  var header = null;
  for (var i = 0; i < h3s.length; i++) {
    if ((h3s[i].textContent || '').trim() === 'Overall Quality') { header = h3s[i]; break; }
  }
  if (!header) return null;
  var card = header.closest('[class*="rounded-lg"]') || header.parentElement;
  if (!card) return null;
  var btns = card.querySelectorAll('button');
  for (var j = 0; j < btns.length; j++) {
    if ((btns[j].textContent || '').trim() === WANT) {
      var e = btns[j];
      try { e.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
      var r = e.getBoundingClientRect();
      if (r.width < 1 || r.height < 1) return null;
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    }
  }
  return null;
})()"#;

const FIND_SUBMIT_JS: &str = r#"(function(){
  var btns = document.querySelectorAll('button');
  var skip = null;
  for (var i = 0; i < btns.length; i++) {
    if ((btns[i].textContent || '').trim().indexOf('Skip') !== -1) { skip = btns[i]; break; }
  }
  if (!skip || !skip.parentElement) return null;
  var siblings = skip.parentElement.querySelectorAll('button');
  for (var j = 0; j < siblings.length; j++) {
    var b = siblings[j];
    if (b === skip) continue;
    if (b.disabled || b.getAttribute('aria-disabled') === 'true') continue;
    try { b.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
    var r = b.getBoundingClientRect();
    if (r.width < 1 || r.height < 1) continue;
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
})()"#;

/// Where we record which task folder this run is using -- see
/// `resolve_or_create_task_dir` / `current_task_dir` below.
fn marker_path(ctx: &WorkflowCtx) -> std::path::PathBuf {
    ctx.settings.output_dir.join(".golem_current_task")
}

/// Scan `output_dir` for existing `task<N>` directories and return the next
/// unused number (1 if none exist yet).
fn next_task_number(output_dir: &std::path::Path) -> u32 {
    let mut max = 0u32;
    if let Ok(read) = std::fs::read_dir(output_dir) {
        for entry in read.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let raw_name = entry.file_name();
            let Some(name) = raw_name.to_str() else { continue };
            if let Some(num_str) = name.strip_prefix("task")
                && let Ok(num) = num_str.parse::<u32>()
            {
                max = max.max(num);
            }
        }
    }
    max + 1
}

/// Used ONLY by "1. Create task1 directory": decide the task folder name
/// (an explicit override typed into the `task_dir` field, else auto-
/// increment task1, task2, task3, ... by checking what already exists),
/// create it, and record the choice in a marker file.
///
/// Why a marker file and not `ctx.set`? Chained workflows each get a BRAND
/// NEW `WorkflowCtx` (see the module doc comment above) -- `ctx.set` in this
/// workflow would not be visible to the next one. A file on disk is the one
/// thing that reliably crosses that boundary.
pub fn resolve_or_create_task_dir(ctx: &WorkflowCtx) -> Result<std::path::PathBuf> {
    let output_dir = ctx.settings.output_dir.clone();
    let explicit = ctx.input("task_dir").map(str::trim).filter(|s| !s.is_empty());
    let name = match explicit {
        Some(n) => n.to_string(),
        None => format!("task{}", next_task_number(&output_dir)),
    };
    let dir = output_dir.join(&name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dir.display())))?;
    let marker = marker_path(ctx);
    std::fs::write(&marker, &name)
        .map_err(|e| GolemError::Io(format!("write {}: {e}", marker.display())))?;
    Ok(dir)
}

/// Used by every OTHER workflow in the family (steps 2-7): resolve the same
/// task folder step 1 picked for this run. An explicit `task_dir` field
/// still wins if you type one directly into THIS workflow's own field
/// (e.g. running it standalone); otherwise it reads the marker file step 1
/// wrote.
pub fn current_task_dir(ctx: &WorkflowCtx) -> Result<std::path::PathBuf> {
    let output_dir = &ctx.settings.output_dir;
    let explicit = ctx.input("task_dir").map(str::trim).filter(|s| !s.is_empty());
    if let Some(n) = explicit {
        return Ok(output_dir.join(n));
    }
    let marker = marker_path(ctx);
    let name = std::fs::read_to_string(&marker).map_err(|e| {
        GolemError::Io(format!(
            "read {}: {e} -- run \"1. Create task1 directory\" first (or type an explicit \
             folder name into this workflow's task_dir field) so this step knows which task \
             folder to use",
            marker.display()
        ))
    })?;
    Ok(output_dir.join(name.trim()))
}

/// The OS's real Downloads folder (e.g. `~/Downloads` on macOS) -- where
/// Chrome saves files when we DON'T redirect it, which is exactly what a
/// manual click does.
fn system_downloads_dir() -> Result<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(|p| p.to_path_buf()))
        .ok_or_else(|| GolemError::Other("couldn't determine the system Downloads folder".into()))
}

/// Click a link (found via `find_js`, an IIFE returning `{x, y}` or `null`)
/// and wait for Chrome to actually save a new file into the OS's real
/// Downloads folder, then move it into `dest_dir/filename`.
///
/// We deliberately do NOT try to redirect Chrome's download folder via CDP
/// first (an earlier attempt at that didn't reliably work) -- instead this
/// lets Chrome do exactly what it already does for a normal manual click
/// (save into `~/Downloads`, auto-numbering `all_files (1).zip` etc. if a
/// name collides), and picks up whatever NEW file appears there as a result.
///
/// Detects "done downloading" by watching for a new filename to appear that
/// ISN'T a Chrome in-progress file (`.crdownload`/`.tmp`), then confirming
/// its size stops changing across a short pause.
pub async fn click_and_wait_for_download(
    ctx: &mut WorkflowCtx,
    find_js: &str,
    dest_dir: &std::path::Path,
    filename: &str,
    timeout: Duration,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dest_dir.display())))?;
    let downloads = system_downloads_dir()?;
    let before = list_names(&downloads);

    // The page can take a moment to render (SPA route change, data still
    // loading, etc.), so poll for the click target the same way the other
    // wait_for_* lookups do instead of giving up after a single eval.
    let find_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let (x, y) = loop {
        ctx.guard().await?;
        let target = ctx.eval(find_js).await?;
        match (
            target.get("x").and_then(Value::as_f64),
            target.get("y").and_then(Value::as_f64),
        ) {
            (Some(x), Some(y)) => break (x, y),
            _ => {
                if tokio::time::Instant::now() >= find_deadline {
                    return Err(GolemError::Other(
                        "download link not found on the page".to_string(),
                    ));
                }
                ctx.human_pause(250, 400).await?;
            }
        }
    };
    ctx.click_at(x, y).await?;

    let deadline = tokio::time::Instant::now() + timeout;
    let downloaded = loop {
        ctx.guard().await?;
        let now = list_names(&downloads);
        let mut candidate: Option<std::path::PathBuf> = None;
        for name in now.difference(&before) {
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".crdownload") || name_str.ends_with(".tmp") {
                continue;
            }
            candidate = Some(downloads.join(name));
        }
        if let Some(path) = candidate {
            // Confirm it's actually finished (stopped growing), not still mid-write.
            let size1 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            ctx.human_pause(400, 700).await?;
            let size2 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size1 == size2 && size1 > 0 {
                break path;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(GolemError::Other(format!(
                "clicked the link but no new file appeared in {} within {}s",
                downloads.display(),
                timeout.as_secs()
            )));
        }
        ctx.human_pause(300, 500).await?;
    };

    let dest = dest_dir.join(filename);
    std::fs::rename(&downloaded, &dest).map_err(|e| {
        GolemError::Io(format!(
            "move {} -> {}: {e}",
            downloaded.display(),
            dest.display()
        ))
    })?;
    Ok(dest)
}

fn list_names(dir: &std::path::Path) -> std::collections::HashSet<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .map(|read| read.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default()
}

/// Finds the Task Data block's download link (same lookup as
/// `TASK_DATA_ZIP_URL_JS`) and returns its clickable centre `{x, y}` instead
/// of its href, for use with `click_and_wait_for_download`.
///
/// Selects the block by its own class list, not by an `<h1>Task Data</h1>`
/// inside it -- some tasks' Task Data block has no heading and no download
/// link at all (just the description text), so anchoring on the heading text
/// made this (and the two lookups below) fail to find anything at all on
/// those tasks. The class combination `prose prose-sm max-w-none
/// text-sm text-foreground` is unique to this one block on the page in both
/// variants, so match on that directly.
pub const TASK_DATA_ZIP_CLICK_JS: &str = r#"(function(){
  var box = document.querySelector('div.prose.prose-sm.max-w-none.text-sm.text-foreground');
  if (!box) return null;
  var a = box.querySelector('a[href]');
  if (!a) return null;
  try { a.scrollIntoView({ block: 'center', inline: 'center' }); } catch (x) {}
  var r = a.getBoundingClientRect();
  if (r.width < 1 || r.height < 1) return null;
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
})()"#;

/// Download `url` straight into `dest_dir/filename` via `curl` -- NOT
/// `ctx.download`, which attaches the current page's cookies + user-agent.
/// These zip files live on a completely different domain
/// (`*.multimodal-agentic-generation-preview.mangovibe.net`) than the task
/// page (`multimango.com`); real browsers never send cookies cross-domain,
/// but `ctx.download` does it manually, and this specific host 404s when it
/// sees a foreign cookie it doesn't recognize (confirmed: fetching the exact
/// same URL with zero cookies attached returns a clean 200). `curl` here
/// sends a bare, unauthenticated request, matching that.
pub async fn download_into(
    ctx: &WorkflowCtx,
    url: &str,
    dest_dir: &std::path::Path,
    filename: &str,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dest_dir.display())))?;
    let dest = dest_dir.join(filename);
    let dest_str = dest
        .to_str()
        .ok_or_else(|| GolemError::Io(format!("non-UTF8 path: {}", dest.display())))?;
    let out = ctx
        .run(
            "curl",
            &["-fsSL", "-o", dest_str, url],
            None,
            Some(Duration::from_secs(120)),
        )
        .await?;
    if !out.success() {
        return Err(GolemError::Io(format!(
            "curl {url} -> {}: {}",
            dest.display(),
            out.combined().trim()
        )));
    }
    Ok(dest)
}

/// Unzip `zip_path` into `dest_dir` (creating it first) and delete the zip
/// afterward. The escape-hatch `ctx.run` shells out to the system `unzip`.
pub async fn unzip_and_cleanup(
    ctx: &WorkflowCtx,
    zip_path: &std::path::Path,
    dest_dir: &std::path::Path,
) -> Result<()> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| GolemError::Io(format!("mkdir {}: {e}", dest_dir.display())))?;
    let out = ctx
        .run(
            "unzip",
            &[
                "-o",
                zip_path.to_str().unwrap_or_default(),
                "-d",
                dest_dir.to_str().unwrap_or_default(),
            ],
            None,
            Some(Duration::from_secs(60)),
        )
        .await?;
    if !out.success() {
        return Err(GolemError::Io(format!("unzip failed: {}", out.combined())));
    }
    std::fs::remove_file(zip_path)
        .map_err(|e| GolemError::Io(format!("delete {}: {e}", zip_path.display())))?;
    Ok(())
}

/// The Task Data block's direct "Download all task data (ZIP)" link href:
/// find `<h1>Task Data</h1>`, read the first `<a href>` inside its wrapper div.
/// This is a same-page, top-level anchor (unlike the response zips below), so
/// it's directly readable -- no cross-origin issue here.
pub async fn task_data_zip_url(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(TASK_DATA_ZIP_URL_JS).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// The Task Data block's full visible text (innerText of its wrapper div).
pub async fn task_data_text(ctx: &WorkflowCtx) -> Result<String> {
    let v = ctx.eval(TASK_DATA_TEXT_JS).await?;
    Ok(v.as_str().unwrap_or_default().to_string())
}

/// Derives `<origin-of-the-named-response's-iframe>/all_files.zip` for
/// `label` ("Response A" or "Response B").
///
/// Why not click the "Copy link" button directly? It's rendered *inside* that
/// response's own `<iframe>` document, which is a different origin than the
/// multimango.com top page (confirmed: the copied URL lives on
/// `...multimodal-agentic-generation-preview.mangovibe.net`). A parent page's
/// JS can't read into a cross-origin iframe's DOM -- that's the browser's
/// same-origin policy, not a Golem limitation. But an `<iframe>` element's
/// `src` attribute is always readable from the parent (only its *contents*
/// are protected), and it's served from the exact same host as the zip. So we
/// read `iframe.src`, take its origin, and append `/all_files.zip` ourselves.
///
/// This is a pattern-based guess. If `ctx.download` on the result 404s, the
/// zip isn't at that exact path for that response and this needs adjusting.
pub async fn response_zip_url(ctx: &WorkflowCtx, label: &str) -> Result<Option<String>> {
    let js = FIND_RESPONSE_ZIP_URL_JS.replace("__LABEL__", &js_str(label));
    let v = ctx.eval(&js).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// Poll `task_data_zip_url` every ~250-400ms until it finds something, or
/// `timeout` elapses. The page is a client-rendered SPA, so a one-shot check
/// right after navigation can race the render -- this rides that out instead
/// of failing immediately. Cancellable via `ctx.guard` (Stop button works).
pub async fn wait_for_task_data_zip_url(
    ctx: &WorkflowCtx,
    timeout: Duration,
) -> Result<Option<String>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if let Some(url) = task_data_zip_url(ctx).await? {
            return Ok(Some(url));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(250, 400).await?;
    }
}

/// Same idea as [`wait_for_task_data_zip_url`], for a response's iframe.
pub async fn wait_for_response_zip_url(
    ctx: &WorkflowCtx,
    label: &str,
    timeout: Duration,
) -> Result<Option<String>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if let Some(url) = response_zip_url(ctx, label).await? {
            return Ok(Some(url));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(250, 400).await?;
    }
}

/// The Evaluation Criteria list, one line per criterion ("1. <text>",
/// "2. <text>", ...). Finds the "Evaluation Criteria" header span, walks up
/// to its card, then reads each row of the `div.divide-y.divide-border`
/// list beneath it -- the first child span in each row is the number, the
/// second is the criterion text.
pub async fn evaluation_criteria_text(ctx: &WorkflowCtx) -> Result<Option<String>> {
    let v = ctx.eval(EVALUATION_CRITERIA_JS).await?;
    Ok(v.as_str().map(str::to_string).filter(|s| !s.is_empty()))
}

/// Poll `evaluation_criteria_text` until it finds rows, or `timeout` elapses.
pub async fn wait_for_evaluation_criteria_text(
    ctx: &WorkflowCtx,
    timeout: Duration,
) -> Result<Option<String>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if let Some(text) = evaluation_criteria_text(ctx).await? {
            return Ok(Some(text));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        ctx.human_pause(250, 400).await?;
    }
}

const EVALUATION_CRITERIA_JS: &str = r#"(function(){
  var spans = document.querySelectorAll('span');
  var header = null;
  for (var i = 0; i < spans.length; i++) {
    if ((spans[i].textContent || '').trim() === 'Evaluation Criteria') { header = spans[i]; break; }
  }
  if (!header) return null;
  var card = header.closest('[class*="rounded-lg"]') || header.parentElement;
  var list = card ? card.querySelector('.divide-y') : null;
  if (!list) return null;
  var rows = list.children;
  var lines = [];
  for (var j = 0; j < rows.length; j++) {
    var row = rows[j];
    var head = row.querySelector('.flex.gap-2') || row.firstElementChild;
    var kids = head ? head.children : [];
    var num = kids[0] ? (kids[0].textContent || '').trim() : (j + 1) + '.';
    var text = kids[1] ? (kids[1].textContent || '').trim() : (head ? (head.textContent || '').trim() : '');
    if (text) lines.push(num + ' ' + text);
  }
  return lines.length ? lines.join('\n') : null;
})()"#;

// Both lookups below select the Task Data block by its own class list (see
// the comment on `TASK_DATA_ZIP_CLICK_JS`) rather than requiring an
// `<h1>Task Data</h1>` -- some tasks' block has no heading/download link, only
// the description text, and this still finds it.

const TASK_DATA_ZIP_URL_JS: &str = r#"(function(){
  var box = document.querySelector('div.prose.prose-sm.max-w-none.text-sm.text-foreground');
  if (!box) return null;
  var a = box.querySelector('a[href]');
  return a ? a.href : null;
})()"#;

const TASK_DATA_TEXT_JS: &str = r#"(function(){
  var box = document.querySelector('div.prose.prose-sm.max-w-none.text-sm.text-foreground');
  return box ? (box.innerText || box.textContent || '') : '';
})()"#;

const FIND_RESPONSE_ZIP_URL_JS: &str = r#"(function(){
  var LABEL = __LABEL__;
  function findIframe() {
    var iframes = document.querySelectorAll('iframe');
    for (var k = 0; k < iframes.length; k++) {
      if ((iframes[k].getAttribute('title') || '') === LABEL) return iframes[k];
    }
    var spans = document.querySelectorAll('span');
    for (var i = 0; i < spans.length; i++) {
      if ((spans[i].textContent || '').trim() === LABEL) {
        var card = spans[i].closest('[class*="rounded-lg"]') || spans[i].parentElement;
        if (card) {
          var inner = card.querySelector('iframe');
          if (inner) return inner;
        }
      }
    }
    return null;
  }
  var f = findIframe();
  if (!f) return null;
  var src = f.getAttribute('src') || '';
  try {
    var u = new URL(src, location.href);
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
    return u.origin + '/all_files.zip';
  } catch (e) {
    return null;
  }
})()"#;
