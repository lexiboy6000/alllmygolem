//! "Auto-fill submission" — on a feather task's Task-execution stage, sync the
//! Vagon assets (snapshots folder + session recordings), inspect the saved ZIP
//! structures, and fill the deliverables form (final / other / working file
//! names + total execution time). It runs every check live to the log and shows
//! a summary popup at the end. It NEVER submits the task.
//!
//! It halts early with a clear warning if a critical check fails (no task page,
//! no snapshot start date, no `_final.*` file, or working-file count below the
//! runtime/5 floor).
//!
//! NOTE: this drives a complex MUI/Radix SPA with several popups; selectors and
//! timings are best-effort and may need tuning against the live DOM. Every step
//! logs what it did so failures pinpoint the exact spot.

use std::time::Duration;

use crate::prelude::*;

use super::util;

pub struct SubmitFill;

#[async_trait]
impl Workflow for SubmitFill {
    fn name(&self) -> &'static str {
        "Auto-fill submission"
    }
    fn description(&self) -> &'static str {
        "Sync Vagon assets, inspect ZIP structures, and fill the deliverables form (no submit)."
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::required(
            "task_url",
            "Task URL (.../tasks/<id>/stage/execution)",
        )]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let wait = Duration::from_millis(ctx.settings.default_wait_timeout_ms);
        // Remote sync ("Check assets") and per-file saves can take a while.
        let long = wait.max(Duration::from_secs(90));

        let task_url = ctx.require_input("task_url")?;
        let task_id = task_id_from_url(&task_url).ok_or_else(|| {
            GolemError::Input(format!("could not find /tasks/<id>/ in URL: {task_url}"))
        })?;
        let snapshots_folder = format!("{task_id}-snapshots");

        let mut checks: Vec<String> = Vec::new();

        // --- open the task's execution stage ---
        ctx.step("open task execution").await?;
        ctx.navigate(&task_url).await?;
        ctx.wait_for_default("body").await?;
        if !util::wait_for_text(ctx, "[role=\"tab\"]", "Task execution", wait).await? {
            return Err(ctx
                .stop_and_warn(
                    "Not on a loaded task page (no 'Task execution' tab). Make sure you're logged \
                     in and the URL is a task's stage page.",
                )
                .await);
        }
        if !util::click_contains(ctx, "[role=\"tab\"]", "Task execution").await? {
            return Err(ctx
                .stop_and_warn("Could not click the 'Task execution' tab.")
                .await);
        }
        ctx.human_pause(600, 1200).await?;
        record(ctx, &mut checks, true, "opened Task execution tab");

        // Wait for the execution view to actually load — its always-present "Sync
        // from Vagon" button is the signal — before deciding about the OPTIONAL
        // "Clear all": on a still-loading page we'd wrongly conclude there's nothing
        // to clear (and skip it).
        let _ = wait_button_present(ctx, "Sync from Vagon", wait).await?;

        // --- clear existing outputs (only if the button is present & enabled) ---
        ctx.step("clear existing outputs").await?;
        match click_button_if_enabled(ctx, "Clear all").await? {
            ClickResult::Clicked => {
                record(ctx, &mut checks, true, "clicked 'Clear all'");
                ctx.human_pause(700, 1400).await?;
                // If there were saved assets, feather pops a "Clear saved assets?"
                // confirmation — confirm it (scoped to the dialog so we hit the
                // error-coloured confirm, never the Cancel button or the page's
                // own Clear all). No dialog => nothing to clear; just proceed.
                if click_confirm_clear_if_present(ctx, Duration::from_millis(3000)).await? {
                    record(ctx, &mut checks, true, "confirmed 'Clear saved assets?' dialog");
                    ctx.human_pause(700, 1400).await?;
                }
            }
            ClickResult::Disabled => {
                record(ctx, &mut checks, true, "'Clear all' present but disabled — skipped");
            }
            ClickResult::Missing => {
                record(ctx, &mut checks, true, "no 'Clear all' button — skipped");
            }
        }

        // --- sync from Vagon -> Check assets ---
        ctx.step("sync from Vagon").await?;
        if !sync_and_check_assets(ctx, wait).await? {
            return Err(ctx
                .stop_and_warn(
                    "Could not open the Sync Assets dialog (Sync from Vagon -> Check assets). \
                     The Sync dialog didn't appear — the page may still be loading.",
                )
                .await);
        }
        record(ctx, &mut checks, true, "requested asset sync (Check assets)");

        // Remote assets take a few seconds (sometimes more) to enumerate.
        ctx.step("wait for assets").await?;
        ctx.note_status("waiting for Vagon assets to load…");
        if !wait_for_row(ctx, &snapshots_folder, long).await? {
            return Err(ctx
                .stop_and_warn(format!(
                    "Timed out waiting for the '{snapshots_folder}' folder to appear after Check assets."
                ))
                .await);
        }
        record(ctx, &mut checks, true, format!("assets loaded; found {snapshots_folder}"));

        // --- save the snapshots folder ---
        ctx.step("save snapshots folder").await?;
        match save_row(ctx, &snapshots_folder, long).await? {
            SaveOutcome::Saved => record(ctx, &mut checks, true, "saved the snapshots folder"),
            SaveOutcome::NoButton => {
                record(ctx, &mut checks, true, "snapshots folder had no Save button (already saved?)")
            }
            SaveOutcome::TimedOut => {
                return Err(ctx
                    .stop_and_warn(
                        "The snapshots folder's Save button stayed disabled past the timeout — \
                         could not save it.",
                    )
                    .await);
            }
        }
        // Wait for the save to COMMIT (the button turns into a disabled "Saved")
        // before reading anything — escaping while it's still uploading is why the
        // assets didn't show up in the saved-assets view.
        if !wait_row_saved(ctx, &snapshots_folder, long).await? {
            record(ctx, &mut checks, false, "snapshots folder didn't reach the 'Saved' state in time");
        }
        ctx.human_pause(500, 1000).await?;

        // --- capture the session rows (name + date) inside the sync modal ---
        // The session row DATES only show in this modal; record them now, then
        // Escape to read the snapshots ZIP, filter, and re-open the modal to save.
        ctx.step("read session list").await?;
        if !wait_for_any_session(ctx, long).await? {
            return Err(ctx
                .stop_and_warn(
                    "No Session_<n> rows appeared in the sync dialog after Check assets — the \
                     asset list may still be loading.",
                )
                .await);
        }
        ctx.human_pause(700, 1300).await?; // let the full list settle
        let session_rows: Vec<(String, String)> = list_rows(ctx)
            .await?
            .into_iter()
            .filter(|r| is_session_name(&r.name))
            .map(|r| (r.name, r.date))
            .collect();
        record(ctx, &mut checks, true, format!("found {} session row(s)", session_rows.len()));

        // --- Escape -> main saved-assets view, read the snapshots ZIP for files ---
        // The "Show zip structure" gives ALL the deliverable files in one read. The
        // modal isn't always a [role=dialog], so we dismiss it with a plain Escape
        // and wait (generously) for that control to appear.
        ctx.step("open snapshots ZIP structure").await?;
        if !escape_to_saved_assets(ctx, &snapshots_folder, long).await? {
            return Err(ctx
                .stop_and_warn(
                    "After saving, the snapshots folder's 'Show zip structure' control never \
                     appeared in the saved-assets view (couldn't get past the modal Escape).",
                )
                .await);
        }

        // --- read the snapshots ZIP (one read lists every subfolder + deliverable) ---
        let snap_yaml = match open_zip_and_copy(ctx, &snapshots_folder, long).await? {
            Some(y) => y,
            None => {
                return Err(ctx
                    .stop_and_warn(
                        "Could not read the snapshots ZIP structure (its 'Show zip structure' \
                         popup produced no text).",
                    )
                    .await);
            }
        };

        // --- start date: the earliest snapshot subfolder, taken from the ZIP ---
        // We deliberately do NOT expand the saved-assets list to read subfolder
        // dates: it holds EVERY task's -snapshots folder (100+ rows), which is too
        // heavy/virtualized to expand reliably and can crash the renderer. The ZIP
        // already lists the YYYYMMDD_HHMMSS subfolders; their names are UTC, so we
        // convert the earliest to the browser's LOCAL zone — which is what feather
        // displays session dates in — for an apples-to-apples comparison.
        ctx.step("read snapshots start date").await?;
        let (start_dt, start_label) = match earliest_start_local(ctx, &snap_yaml).await? {
            Some(x) => x,
            None => {
                return Err(ctx
                    .stop_and_warn(
                        "Could not find any YYYYMMDD_HHMMSS subfolders in the snapshots ZIP, so the \
                         start date can't be determined.",
                    )
                    .await);
            }
        };
        record(ctx, &mut checks, true, format!("snapshots start date: {start_label}"));

        let files = extract_filenames(&snap_yaml);
        let mut finals: Vec<String> = files.iter().filter(|f| is_final(f)).cloned().collect();
        finals.sort();
        finals.dedup();
        if finals.len() > 1 {
            return Err(ctx
                .stop_and_warn(format!(
                    "Ambiguous final deliverable: multiple '*_final.*' files in the snapshots ZIP \
                     ({}). Resolve manually.",
                    finals.join(", ")
                ))
                .await);
        }
        let Some(final_file) = finals.into_iter().next() else {
            return Err(ctx
                .stop_and_warn(
                    "No final deliverable (a '*_final.*' file) found in the snapshots ZIP.",
                )
                .await);
        };
        let working_file = working_from_final(&final_file);
        let working_count = files.iter().filter(|f| **f == working_file).count();
        let mut others: Vec<String> = files
            .iter()
            .filter(|f| **f != final_file && **f != working_file && f.as_str() != "Files.txt")
            .cloned()
            .collect();
        others.sort();
        others.dedup();
        record(ctx, &mut checks, true, format!("final deliverable: {final_file}"));
        record(ctx, &mut checks, true, format!("working file: {working_file}"));
        record(
            ctx,
            &mut checks,
            true,
            format!("working-file instances in snapshots ZIP: {working_count}"),
        );
        record(
            ctx,
            &mut checks,
            true,
            format!("other deliverables: {}", if others.is_empty() { "(none)".into() } else { others.join(", ") }),
        );

        // --- select the sessions to save (dated at/after the start) ---
        ctx.step("select sessions to save").await?;
        for (name, date) in &session_rows {
            if parse_feather_date(date).is_none() {
                record(
                    ctx,
                    &mut checks,
                    false,
                    format!("session {name} has an unparseable date '{date}' — NOT considered"),
                );
            }
        }
        let mut to_save: Vec<(String, chrono::NaiveDateTime)> = session_rows
            .iter()
            .filter_map(|(name, date)| parse_feather_date(date).map(|d| (name.clone(), d)))
            .filter(|(_, d)| *d >= start_dt)
            .collect();
        // Dedup by name (a session may render more than once), then order by date.
        to_save.sort_by(|a, b| a.0.cmp(&b.0));
        to_save.dedup_by(|a, b| a.0 == b.0);
        to_save.sort_by_key(|a| a.1);
        if to_save.is_empty() {
            record(ctx, &mut checks, false, "no session files at/after the start date — nothing to save");
        }

        // --- re-open the modal and save the qualifying sessions ---
        let mut saved_sessions: Vec<String> = Vec::new();
        if !to_save.is_empty() {
            ctx.step("save session files").await?;
            if !sync_and_check_assets(ctx, wait).await? {
                return Err(ctx
                    .stop_and_warn("Could not re-open the Sync Assets dialog to save sessions.")
                    .await);
            }
            if let Some((first, _)) = to_save.first()
                && !wait_for_row(ctx, first, long).await?
            {
                return Err(ctx
                    .stop_and_warn("Sessions did not re-appear in the sync dialog after re-checking assets.")
                    .await);
            }
            for (name, _) in &to_save {
                // Respects the 5-at-a-time limit: save_row waits for the Save button.
                match save_row(ctx, name, long).await {
                    Ok(SaveOutcome::Saved) => {
                        saved_sessions.push(name.clone());
                        record(ctx, &mut checks, true, format!("saved session {name}"));
                    }
                    Ok(SaveOutcome::NoButton) => {
                        record(ctx, &mut checks, true, format!("session {name} had no Save (already saved?)"));
                        saved_sessions.push(name.clone());
                    }
                    Ok(SaveOutcome::TimedOut) => {
                        return Err(ctx
                            .stop_and_warn(format!(
                                "Session {name}'s Save button stayed disabled past the timeout — could \
                                 not save it, so the submission would be incomplete."
                            ))
                            .await);
                    }
                    Err(e) => {
                        record(ctx, &mut checks, false, format!("failed to save session {name}: {e}"));
                    }
                }
                ctx.human_pause(300, 700).await?;
            }
        }
        record(
            ctx,
            &mut checks,
            true,
            format!("saved {} session file(s)", saved_sessions.len()),
        );

        // Wait for every session save to COMMIT (button -> disabled "Saved") before
        // Escaping — escaping mid-upload is why the sessions weren't in the
        // saved-assets view when runtime tried to read them.
        if !saved_sessions.is_empty() {
            ctx.note_status("waiting for session saves to finish committing…");
            for name in &saved_sessions {
                // HALT (don't just warn): escaping before a save commits would lose
                // that session, leaving the submission incomplete.
                if !wait_row_saved(ctx, name, long).await? {
                    return Err(ctx
                        .stop_and_warn(format!(
                            "Session {name}'s save didn't reach the committed 'Saved' state before \
                             the timeout — stopping rather than escaping mid-upload and losing it."
                        ))
                        .await);
                }
            }
        }

        // --- runtime: each saved session's ZIP structure -> (mp4 - 2) * 5 ---
        // Escape back to the saved-assets view, then read each session's "Show zip
        // structure" and count its .mp4 recordings (drop the start + end recording).
        ctx.step("calculate runtime").await?;
        let mut total_runtime = 0i64;
        // We opened the modal to save sessions iff to_save was non-empty; escape back
        // regardless of how many saved (else the still-open modal blocks the form
        // fill). Marker = a saved session, or the always-saved snapshots folder.
        if !to_save.is_empty() {
            let marker = saved_sessions
                .first()
                .cloned()
                .unwrap_or_else(|| snapshots_folder.clone());
            if !escape_to_saved_assets(ctx, &marker, long).await? {
                return Err(ctx
                    .stop_and_warn(
                        "Couldn't return to the saved-assets view after saving sessions (the \
                         marker's 'Show zip structure' never appeared).",
                    )
                    .await);
            }
        }
        for name in &saved_sessions {
            match open_zip_and_copy(ctx, name, long).await? {
                Some(yaml) => {
                    let mp4 = yaml.matches(".mp4").count() as i64;
                    let mins = session_runtime_min(mp4);
                    total_runtime = total_runtime.saturating_add(mins);
                    record(
                        ctx,
                        &mut checks,
                        true,
                        format!("{name}: {mp4} recordings -> {} counted -> {mins} min", (mp4 - 2).max(0)),
                    );
                }
                None => {
                    return Err(ctx
                        .stop_and_warn(format!(
                            "Could not read the ZIP structure for session {name} — cannot verify its \
                             runtime, so the total would be wrong. Stopping."
                        ))
                        .await);
                }
            }
        }
        record(ctx, &mut checks, true, format!("total execution time: {total_runtime} min"));

        // --- fill the deliverables form (we're back in the main view) ---
        ctx.step("fill deliverables form").await?;
        if !fill_field(ctx, "#root_final_file", &final_file).await? {
            return Err(ctx.stop_and_warn("Could not fill the 'Final deliverable file name(s)' box.").await);
        }
        if !fill_field(ctx, "#root_final_other_file", &others.join("\n")).await? {
            record(ctx, &mut checks, false, "could not fill the 'Other deliverable file name(s)' box");
        }
        if !fill_field(ctx, "#root_interim_application_file_name", &working_file).await? {
            record(ctx, &mut checks, false, "could not fill the 'Working file' box");
        }
        if !fill_field(ctx, "#root_final_length", &total_runtime.to_string()).await? {
            return Err(ctx.stop_and_warn("Could not fill the 'total execution time' field.").await);
        }
        record(ctx, &mut checks, true, "filled final / other / working / runtime fields");

        // --- critical check: file was saved at least every 5 minutes ---
        ctx.step("verify save cadence").await?;
        // ceil(runtime / 5) without the unstable int div_ceil (runtime >= 0).
        let floor = (total_runtime + 4) / 5;
        let cadence_ok = (working_count as i64) >= floor;
        record(
            ctx,
            &mut checks,
            cadence_ok,
            format!(
                "save cadence: working files {working_count} {} runtime/5 = {floor}",
                if cadence_ok { ">=" } else { "< (TOO FEW — not saved every 5 min)" }
            ),
        );

        let summary = build_summary(&checks);
        if !cadence_ok {
            return Err(ctx
                .stop_and_warn(format!(
                    "Critical check FAILED: only {working_count} working-file saves for {total_runtime} \
                     min of runtime (need >= {floor}). Form is filled but NOT submitted.\n\n{summary}"
                ))
                .await);
        }

        // --- done: show the checks, do NOT submit ---
        ctx.warn_user(format!(
            "Submission form auto-filled (NOT submitted). Review, then submit manually.\n\n{summary}"
        ))
        .await?;

        Ok(WorkflowOutcome::CompletedWith(json!({
            "task_id": task_id,
            "final_file": final_file,
            "working_file": working_file,
            "other_files": others,
            "sessions_saved": saved_sessions.len(),
            "working_file_count": working_count,
            "total_runtime_min": total_runtime,
            "save_cadence_ok": cadence_ok,
        })))
    }
}

// ---------------------------------------------------------------------------
// Check logging
// ---------------------------------------------------------------------------

fn record(ctx: &WorkflowCtx, checks: &mut Vec<String>, ok: bool, msg: impl Into<String>) {
    let line = format!("[{}] {}", if ok { "OK" } else { "FAIL" }, msg.into());
    if ok {
        ctx.output(&line);
    } else {
        ctx.warn(&line);
    }
    checks.push(line);
}

fn build_summary(checks: &[String]) -> String {
    let mut s = String::from("Checks:\n");
    for c in checks {
        s.push_str(c);
        s.push('\n');
    }
    s
}

// ---------------------------------------------------------------------------
// Row / button DOM helpers (robust, polling)
// ---------------------------------------------------------------------------

struct Row {
    name: String,
    date: String,
}

/// All file/folder rows currently visible: their leaf name + displayed date.
async fn list_rows(ctx: &WorkflowCtx) -> Result<Vec<Row>> {
    let v = ctx.eval(LIST_ROWS_JS).await?;
    let mut out = Vec::new();
    if let Some(arr) = v.as_array() {
        for it in arr {
            let name = it.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let date = it.get("date").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if !name.is_empty() {
                out.push(Row { name, date });
            }
        }
    }
    Ok(out)
}

/// Poll until a row with `name` exists, or `timeout`.
async fn wait_for_row(ctx: &WorkflowCtx, name: &str, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if list_rows(ctx).await?.iter().any(|r| r.name == name) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(250, 500).await?;
    }
}

/// Poll until at least one `Session_<n>` row is present, or `timeout`.
async fn wait_for_any_session(ctx: &WorkflowCtx, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if list_rows(ctx).await?.iter().any(|r| is_session_name(&r.name)) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(300, 600).await?;
    }
}

/// Earliest `YYYYMMDD_HHMMSS` subfolder name (UTC) listed anywhere in the
/// snapshots ZIP YAML. Names are zero-padded, so lexicographic min = chronological.
fn earliest_subfolder_utc(yaml: &str) -> Option<String> {
    let mut best: Option<String> = None;
    for line in yaml.lines() {
        for run in line.split(|c: char| !(c.is_ascii_digit() || c == '_')) {
            if is_snapshot_subfolder(run)
                && let Some(name) = run.get(..15)
                && best.as_deref().map(|b| name < b).unwrap_or(true)
            {
                best = Some(name.to_string());
            }
        }
    }
    best
}

/// Convert a `YYYYMMDD_HHMMSS` UTC subfolder name to a browser-LOCAL datetime
/// (feather shows session dates in the browser's local zone) + a feather-style
/// label. Eval errors propagate; `Ok(None)` if the name/result is unparseable.
async fn utc_subfolder_to_local(
    ctx: &WorkflowCtx,
    utc: &str,
) -> Result<Option<(chrono::NaiveDateTime, String)>> {
    let part = |a: usize, b: usize| utc.get(a..b).and_then(|s| s.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) =
        (part(0, 4), part(4, 6), part(6, 8), part(9, 11), part(11, 13), part(13, 15))
    else {
        return Ok(None);
    };
    let js = UTC_TO_LOCAL_JS
        .replace("__Y__", &y.to_string())
        .replace("__MO__", &mo.to_string())
        .replace("__D__", &d.to_string())
        .replace("__H__", &h.to_string())
        .replace("__MI__", &mi.to_string())
        .replace("__S__", &s.to_string());
    let v = ctx.eval(&js).await?;
    let g = |k: &str| v.get(k).and_then(Value::as_i64);
    let (Some(ly), Some(lmo), Some(ld), Some(lh), Some(lmi)) =
        (g("y"), g("mo"), g("d"), g("h"), g("mi"))
    else {
        return Ok(None);
    };
    let Some(nd) = chrono::NaiveDate::from_ymd_opt(ly as i32, lmo as u32, ld as u32)
        .and_then(|date| date.and_hms_opt(lh as u32, lmi as u32, 0))
    else {
        return Ok(None);
    };
    Ok(Some((nd, nd.format("%b %d, %Y, %I:%M %p").to_string())))
}

/// The snapshot start date in feather's display (browser-local) zone, derived
/// from the earliest UTC subfolder in the snapshots ZIP — no fragile expanding of
/// the heavy saved-assets list.
async fn earliest_start_local(
    ctx: &WorkflowCtx,
    yaml: &str,
) -> Result<Option<(chrono::NaiveDateTime, String)>> {
    let Some(utc) = earliest_subfolder_utc(yaml) else {
        return Ok(None);
    };
    ctx.output(format!("earliest snapshot subfolder (UTC): {utc}"));
    utc_subfolder_to_local(ctx, &utc).await
}

/// Poll until the row named `name` shows a disabled "Saved" button — i.e. its
/// save has COMMITTED (the upload finished), not just been clicked. Escaping the
/// modal before this is why saved assets didn't appear in the saved-assets view.
async fn wait_row_saved(ctx: &WorkflowCtx, name: &str, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    let js = ROW_SAVED_JS.replace("__NAME__", &util::js_str(name));
    loop {
        ctx.guard().await?;
        if ctx.eval(&js).await?.as_bool().unwrap_or(false) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(500, 1000).await?;
    }
}

/// Escape the sync modal and wait for the main saved-assets view to be ready,
/// detected by the `marker` row's "Show zip structure" (magnifying-glass) control
/// appearing. The sync modal isn't reliably a `[role=dialog]`, so we dismiss it
/// with a plain Escape (re-pressed a few times across the wait) rather than the
/// dialog-scoped close — that "initial escape" not landing was why the saved-assets
/// view never showed. Generous timeout: the save commits before the control shows.
async fn escape_to_saved_assets(ctx: &WorkflowCtx, marker: &str, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut escapes = 0u32;
    loop {
        ctx.guard().await?;
        if locate_row_action(ctx, marker, "zip").await?.is_some() {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        if escapes < 8 {
            ctx.press_key("Escape").await?;
            escapes += 1;
        }
        ctx.human_pause(900, 1700).await?;
    }
}

/// Locate the action button (`save` / `zip` / `expand`) of the row named `name`,
/// returning its viewport centre + disabled flag, or None if absent.
async fn locate_row_action(
    ctx: &WorkflowCtx,
    name: &str,
    action: &str,
) -> Result<Option<(f64, f64, bool)>> {
    let js = CLICK_ROW_ACTION_JS
        .replace("__NAME__", &util::js_str(name))
        .replace("__ACTION__", &util::js_str(action));
    let v = ctx.eval(&js).await?;
    if v.is_null() {
        return Ok(None);
    }
    match (
        v.get("x").and_then(Value::as_f64),
        v.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => Ok(Some((x, y, v.get("disabled").and_then(Value::as_bool).unwrap_or(false)))),
        _ => Ok(None),
    }
}

/// A short "spot it and aim the cursor" beat before a deliberate click.
async fn aim_pause(ctx: &WorkflowCtx) -> Result<()> {
    ctx.human_pause(280, 720).await
}

/// Evaluate a locate JS repeatedly until its returned `{x,y}` stops moving — i.e.
/// the page has finished loading/shifting — then return the settled value. The
/// whole flakiness class the user hit comes from clicking a coordinate that was
/// captured while the page was still rendering (the element has since moved); this
/// waits for the position to be stable before we commit. Returns the last value
/// seen at `timeout` (which may be null / still-moving if it never settled).
async fn stable_eval(ctx: &WorkflowCtx, js: &str, timeout: Duration) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut prev: Option<(f64, f64)> = None;
    let mut stable = 0u32;
    loop {
        ctx.guard().await?;
        let v = ctx.eval(js).await?;
        let x = v.get("x").and_then(Value::as_f64);
        let y = v.get("y").and_then(Value::as_f64);
        match (x, y) {
            (Some(x), Some(y)) => {
                let settled = matches!(prev, Some((px, py)) if (x - px).abs() <= 2.0 && (y - py).abs() <= 2.0);
                prev = Some((x, y));
                if settled {
                    stable += 1;
                    if stable >= 2 {
                        return Ok(v); // two consecutive steady reads = settled
                    }
                } else {
                    stable = 0;
                }
            }
            _ => {
                prev = None;
                stable = 0;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(v);
        }
        ctx.human_pause(150, 350).await?;
    }
}

/// Re-read `js` FRESH and click the element's centre iff it's present + enabled.
/// Reading the coordinate immediately before clicking (after the aim beat) guards
/// against a last-moment shift while the cursor was travelling.
async fn fresh_click(ctx: &mut WorkflowCtx, js: &str) -> Result<bool> {
    let v = ctx.eval(js).await?;
    if v.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(false);
    }
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

/// Find a row's action button and click it (if present & enabled), waiting for the
/// page to settle so the coordinate isn't stale.
async fn click_row(ctx: &mut WorkflowCtx, name: &str, action: &str) -> Result<bool> {
    let js = CLICK_ROW_ACTION_JS
        .replace("__NAME__", &util::js_str(name))
        .replace("__ACTION__", &util::js_str(action));
    let v = stable_eval(ctx, &js, Duration::from_secs(12)).await?;
    if v.get("x").and_then(Value::as_f64).is_none()
        || v.get("disabled").and_then(Value::as_bool).unwrap_or(false)
    {
        return Ok(false);
    }
    aim_pause(ctx).await?;
    fresh_click(ctx, &js).await
}

/// Outcome of trying to save a row.
enum SaveOutcome {
    /// Clicked the (enabled) Save button.
    Saved,
    /// No Save button on the row — presumably already saved.
    NoButton,
    /// The Save button stayed disabled past the timeout (never saved).
    TimedOut,
}

/// Click a row's Save button, waiting for it to become enabled (feather caps
/// concurrent saves at 5) AND for its position to settle so the click isn't stale.
async fn save_row(ctx: &mut WorkflowCtx, name: &str, timeout: Duration) -> Result<SaveOutcome> {
    let js = CLICK_ROW_ACTION_JS
        .replace("__NAME__", &util::js_str(name))
        .replace("__ACTION__", &util::js_str("save"));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        let v = ctx.eval(&js).await?;
        if v.get("x").and_then(Value::as_f64).is_none() {
            return Ok(SaveOutcome::NoButton); // no Save button -> already saved
        }
        if !v.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
            // Enabled: settle the position, then human-click the FRESH coordinate.
            let _ = stable_eval(ctx, &js, Duration::from_secs(8)).await?;
            aim_pause(ctx).await?;
            if fresh_click(ctx, &js).await? {
                return Ok(SaveOutcome::Saved);
            }
            // Raced back to disabled / vanished during the settle — retry below.
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(SaveOutcome::TimedOut);
        }
        ctx.note_status(format!("waiting to save {name} (5-at-a-time limit)…"));
        ctx.human_pause(600, 1200).await?;
    }
}

/// Click a button identified by exact text, only if present and enabled (waiting
/// for its position to settle first).
async fn click_button_if_enabled(ctx: &mut WorkflowCtx, text: &str) -> Result<ClickResult> {
    let js = CLICK_BUTTON_JS.replace("__TXT__", &util::js_str(text));
    let v = ctx.eval(&js).await?;
    if !v.get("found").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(ClickResult::Missing);
    }
    if v.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(ClickResult::Disabled);
    }
    // Found + enabled: settle the position, then human-click the FRESH coordinate.
    let _ = stable_eval(ctx, &js, Duration::from_secs(8)).await?;
    aim_pause(ctx).await?;
    if fresh_click(ctx, &js).await? {
        Ok(ClickResult::Clicked)
    } else {
        Ok(ClickResult::Missing)
    }
}

/// Poll until a button with exact text `text` is present (found), or `timeout`.
/// Used to wait for page/modal content to finish loading before an OPTIONAL click
/// (e.g. don't decide "no Clear all" until the execution view has actually loaded).
async fn wait_button_present(ctx: &WorkflowCtx, text: &str, timeout: Duration) -> Result<bool> {
    let js = CLICK_BUTTON_JS.replace("__TXT__", &util::js_str(text));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if ctx.eval(&js).await?.get("found").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(250, 500).await?;
    }
}

#[derive(PartialEq)]
enum ClickResult {
    Clicked,
    Disabled,
    Missing,
}

/// Poll for a text button to appear, then click it.
async fn wait_and_click_button(ctx: &mut WorkflowCtx, text: &str, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        if matches!(click_button_if_enabled(ctx, text).await?, ClickResult::Clicked) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(250, 500).await?;
    }
}

/// Open the Sync Assets dialog and click "Check assets". Feather sometimes
/// no-ops the first "Sync from Vagon" click while the page is still settling, so
/// this retries: it only re-clicks Sync when no dialog is already open, then
/// waits a bounded time for "Check assets" to appear. Returns false if it never
/// got the dialog open (caller halts).
async fn sync_and_check_assets(ctx: &mut WorkflowCtx, wait: Duration) -> Result<bool> {
    let short = wait.min(Duration::from_secs(10));
    for attempt in 0..3u32 {
        // Don't re-click Sync if a dialog is already up (would toggle it shut).
        let dialog_open = ctx.eval(ANY_DIALOG_OPEN_JS).await?.as_bool().unwrap_or(false);
        if !dialog_open {
            if !wait_and_click_button(ctx, "Sync from Vagon", wait).await? {
                return Ok(false);
            }
            ctx.human_pause(700, 1400).await?;
        }
        if wait_and_click_button(ctx, "Check assets", short).await? {
            return Ok(true);
        }
        ctx.note_status(format!(
            "Sync dialog didn't open; retrying ({}/3)…",
            attempt + 1
        ));
        ctx.human_pause(700, 1300).await?;
    }
    Ok(false)
}

/// If a "Clear saved assets?" confirmation dialog is visible, click its confirm
/// "Clear all" button (scoped to the `[role=dialog]`, identified by the MUI
/// error-button class so the Cancel button — and the page's own Clear all — are
/// never hit). Polls up to `timeout`; `Ok(false)` means no dialog appeared (no
/// assets to clear).
async fn click_confirm_clear_if_present(ctx: &mut WorkflowCtx, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        ctx.guard().await?;
        let v = ctx.eval(CONFIRM_CLEAR_DIALOG_JS).await?;
        if v.get("x").and_then(Value::as_f64).is_some() {
            // Dialog present: settle its position, then human-click the FRESH confirm.
            let _ = stable_eval(ctx, CONFIRM_CLEAR_DIALOG_JS, Duration::from_secs(6)).await?;
            aim_pause(ctx).await?;
            if fresh_click(ctx, CONFIRM_CLEAR_DIALOG_JS).await? {
                return Ok(true);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        ctx.human_pause(200, 400).await?;
    }
}

/// Close an open zip-structure popup (Escape until its copy control is gone), so
/// the next one isn't blocked behind a still-open one.
async fn close_zip_popup(ctx: &WorkflowCtx) -> Result<()> {
    for _ in 0..3 {
        if !ctx.eval(ZIP_OPEN_JS).await?.as_bool().unwrap_or(false) {
            break;
        }
        ctx.press_key("Escape").await?;
        ctx.human_pause(300, 650).await?;
    }
    Ok(())
}

/// Click a row's "Show zip structure" (magnifying-glass) control, copy the YAML it
/// shows via a REAL human click on its copy button, return it, and close the popup.
/// Robust: closes any stale popup first, the copy locator scans all visible dialogs
/// (so nothing shadows the zip popup), and it retries the open once. Uses the in-page
/// clipboard hook (no OS clipboard).
async fn open_zip_and_copy(ctx: &mut WorkflowCtx, name: &str, timeout: Duration) -> Result<Option<String>> {
    close_zip_popup(ctx).await?;
    let overall = tokio::time::Instant::now() + timeout.min(Duration::from_secs(40));
    let mut yaml: Option<String> = None;
    for attempt in 0..2u32 {
        if !click_row(ctx, name, "zip").await? {
            if attempt == 0 {
                ctx.warn(format!("no 'Show zip structure' control for {name}"));
            }
            break;
        }
        let sub = std::cmp::min(
            tokio::time::Instant::now() + Duration::from_secs(12),
            overall,
        );
        loop {
            ctx.guard().await?;
            let v = ctx.eval(COPY_HOOK_LOCATE_JS).await?;
            if let (Some(x), Some(y)) = (
                v.get("x").and_then(Value::as_f64),
                v.get("y").and_then(Value::as_f64),
            ) {
                aim_pause(ctx).await?;
                ctx.click_at(x, y).await?;
                ctx.human_pause(300, 650).await?;
                let clip = ctx.eval(READ_CLIP_JS).await?;
                if let Some(s) = clip.as_str()
                    && !s.trim().is_empty()
                {
                    yaml = Some(s.to_string());
                    break;
                }
            }
            if tokio::time::Instant::now() >= sub {
                break;
            }
            ctx.human_pause(300, 600).await?;
        }
        if yaml.is_some() || tokio::time::Instant::now() >= overall {
            break;
        }
        ctx.note_status(format!("retrying ZIP structure for {name}…"));
        close_zip_popup(ctx).await?;
    }
    if yaml.is_none() {
        ctx.warn(format!("the ZIP structure for {name} never produced a copyable YAML"));
    }
    close_zip_popup(ctx).await?;
    Ok(yaml)
}

/// Set a React-controlled `<textarea>`/`<input>` value (native setter + events).
async fn fill_field(ctx: &WorkflowCtx, selector: &str, value: &str) -> Result<bool> {
    let js = SET_VALUE_JS
        .replace("__SEL__", &util::js_str(selector))
        .replace("__VAL__", &util::js_str(value));
    Ok(ctx.eval(&js).await?.as_bool().unwrap_or(false))
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// A session's counted runtime in minutes: drop the start + end recording
/// (the fixed `-2`), 5 minutes each.
fn session_runtime_min(mp4_count: i64) -> i64 {
    (mp4_count - 2).max(0) * 5
}

fn task_id_from_url(url: &str) -> Option<String> {
    let after = url.split("/tasks/").nth(1)?;
    let id = after.split('/').next().unwrap_or("");
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn parse_feather_date(s: &str) -> Option<chrono::NaiveDateTime> {
    // e.g. "Jun 17, 2026, 09:48 AM"
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%b %d, %Y, %I:%M %p").ok()
}

/// Snapshot subfolders are timestamp-named: `YYYYMMDD_HHMMSS`. Match a 15-char
/// `\d{8}_\d{6}` PREFIX (tolerating any trailing text the UI may append, e.g. a
/// size or a slash) rather than requiring an exact length.
fn is_snapshot_subfolder(name: &str) -> bool {
    name.trim()
        .as_bytes()
        .get(..15)
        .is_some_and(|p| {
            p.iter()
                .enumerate()
                .all(|(i, c)| if i == 8 { *c == b'_' } else { c.is_ascii_digit() })
        })
}

/// Session folders are named `Session_<digits>`.
fn is_session_name(name: &str) -> bool {
    name.strip_prefix("Session_")
        .map(|rest| !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

/// A final deliverable is a `*_final.<ext>` file.
fn is_final(name: &str) -> bool {
    if let Some(dot) = name.rfind('.') {
        name.get(..dot).map(|stem| stem.ends_with("_final")).unwrap_or(false)
    } else {
        false
    }
}

/// `foo_final.cir` -> `foo.cir` (drop the `_final` from the stem).
fn working_from_final(final_name: &str) -> String {
    if let Some(dot) = final_name.rfind('.')
        && let Some(stem) = final_name.get(..dot)
        && let Some(ext) = final_name.get(dot..)
        && let Some(base) = stem.strip_suffix("_final")
    {
        return format!("{base}{ext}");
    }
    final_name.to_string()
}

/// Extract candidate filenames (basenames) from arbitrary text (the ZIP YAML).
fn extract_filenames(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut tok = String::new();
    let is_tok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    for c in text.chars() {
        if is_tok(c) {
            tok.push(c);
        } else {
            flush_token(&tok, &mut out);
            tok.clear();
        }
    }
    flush_token(&tok, &mut out);
    out
}

fn flush_token(tok: &str, out: &mut Vec<String>) {
    let Some(dot) = tok.rfind('.') else { return };
    let ext = tok.get(dot + 1..).unwrap_or("");
    if ext.is_empty() || ext.len() > 6 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return;
    }
    // basename after the last '/'
    let base = tok.rsplit('/').next().unwrap_or(tok);
    if !base.is_empty() && base != "." {
        out.push(base.to_string());
    }
}

// ---------------------------------------------------------------------------
// JS evaluators (best-effort against the live DOM; tuned by the row structure
// of name <span class="truncate"> + date <span class="text-xs"> + action buttons)
// ---------------------------------------------------------------------------

// Convert a UTC date/time (components injected) to the browser's LOCAL zone,
// returning the local calendar components. feather displays session dates in the
// browser's local zone, so this makes the snapshot start comparable to them.
const UTC_TO_LOCAL_JS: &str = r#"(function(){
  var d = new Date(Date.UTC(__Y__, __MO__ - 1, __D__, __H__, __MI__, __S__));
  return { y: d.getFullYear(), mo: d.getMonth()+1, d: d.getDate(), h: d.getHours(), mi: d.getMinutes() };
})()"#;

const LIST_ROWS_JS: &str = r#"(function(){
  var out = [];
  var spans = document.querySelectorAll('span.truncate');
  for (var i=0;i<spans.length;i++){
    var s = spans[i];
    if (s.querySelector && s.querySelector('span.truncate')) continue;
    if (s.offsetParent === null) continue; // skip hidden rows (behind a dialog)
    var name = (s.textContent||'').trim();
    if (!name) continue;
    var row = s.parentElement;
    while (row && !(row.querySelector && row.querySelector('span.text-xs'))) row = row.parentElement;
    var date = '';
    if (row){ var d = row.querySelector('span.text-xs'); if (d) date = (d.textContent||'').trim(); }
    out.push({ name: name, date: date });
  }
  return out;
})()"#;

const CLICK_ROW_ACTION_JS: &str = r#"(function(){
  var name = __NAME__, action = __ACTION__;
  // The leaf (innermost) name span of a row.
  function leafName(row){
    var sp = row.querySelectorAll('span.truncate');
    for (var i=0;i<sp.length;i++){
      if (sp[i].querySelector && sp[i].querySelector('span.truncate')) continue;
      return (sp[i].textContent||'').trim();
    }
    return '';
  }
  // Nearest ancestor that is a single row (has both a name and a date).
  function rowOf(el){
    var r = el;
    while (r && !(r.querySelector && r.querySelector('span.text-xs') && r.querySelector('span.truncate'))) r = r.parentElement;
    return r;
  }
  var target = null;
  if (action === 'expand') {
    var spans = document.querySelectorAll('span.truncate');
    var nameSpan = null;
    for (var i=0;i<spans.length;i++){
      var s = spans[i];
      if (s.querySelector && s.querySelector('span.truncate')) continue;
      if (s.offsetParent === null) continue;
      if ((s.textContent||'').trim() === name){ nameSpan = s; break; }
    }
    if (!nameSpan) return null;
    // Find the folder's expand toggle by climbing ancestors WITHOUT requiring a
    // date span — a narrow window can hide it, which broke the old rowOf() and
    // made us click the NAME (i.e. navigate) instead of expand. Prefer
    // aria-expanded, then a chevron/arrow/folder/caret icon button; stop before
    // climbing into sibling rows.
    var via = '';
    var node = nameSpan;
    for (var up=0; up<10 && node && !target; up++){
      if (up > 0 && node.querySelectorAll){
        var leaves = 0, ns = node.querySelectorAll('span.truncate');
        for (var q=0;q<ns.length;q++){ if (!(ns[q].querySelector && ns[q].querySelector('span.truncate'))) leaves++; }
        if (leaves > 2) break; // over-climbed into multiple rows
      }
      var ae = node.querySelector ? node.querySelector('[aria-expanded]') : null;
      if (ae){ target = ae; via = 'aria-expanded'; break; }
      var rb = node.querySelectorAll ? node.querySelectorAll('button,[role="button"]') : [];
      for (var k=0;k<rb.length;k++){
        var svg = rb[k].querySelector('svg');
        var tid = svg ? (svg.getAttribute('data-testid')||'') : '';
        var al = rb[k].getAttribute('aria-label') || '';
        if (/expand|collapse|chevron|arrow|folder|tree|caret/i.test(tid + ' ' + al)){ target = rb[k]; via = 'icon-button'; break; }
      }
      node = node.parentElement;
    }
    // Last resort: the name's row button (may navigate; flagged so the log shows it).
    if (!target){ target = nameSpan.closest('button,[role="button"]') || nameSpan; via = 'name-fallback'; }
    var dsvg = target.querySelector ? target.querySelector('svg') : null;
    var dtid = (dsvg && dsvg.getAttribute('data-testid')) || (target.getAttribute && target.getAttribute('data-testid')) || '';
    try { target.scrollIntoView({ block:'center', inline:'center' }); } catch(e){}
    var br = target.getBoundingClientRect();
    return { x: br.left + br.width/2, y: br.top + br.height/2, disabled: false, via: via, tag: target.tagName || '', testid: dtid };
  } else {
    // Match the action button BY ITS OWN ROW'S NAME, so a far-away same-action
    // button (another row) can never be picked.
    var btns = document.querySelectorAll('button');
    for (var j=0;j<btns.length;j++){
      var b = btns[j];
      if (b.offsetParent === null) continue;
      var ok = false;
      if (action === 'save') ok = ((b.textContent||'').trim() === 'Save');
      else if (action === 'zip') ok = !!b.querySelector('svg[data-testid="SearchIcon"]');
      if (!ok) continue;
      var row = rowOf(b);
      if (row && leafName(row) === name){ target = b; break; }
    }
  }
  if (!target) return null;
  try { target.scrollIntoView({ block:'center', inline:'center' }); } catch(e){}
  var r = target.getBoundingClientRect();
  var disabled = !!(target.disabled || target.getAttribute('aria-disabled') === 'true' ||
                    (target.className && String(target.className).indexOf('Mui-disabled') !== -1));
  return { x: r.left + r.width/2, y: r.top + r.height/2, disabled: disabled };
})()"#;

// True iff the row named __NAME__ shows a (committed) "Saved" button — used to
// wait for a save to finish uploading before Escaping the modal.
const ROW_SAVED_JS: &str = r#"(function(){
  var name = __NAME__;
  function leafName(row){
    var sp = row.querySelectorAll('span.truncate');
    for (var i=0;i<sp.length;i++){
      if (sp[i].querySelector && sp[i].querySelector('span.truncate')) continue;
      return (sp[i].textContent||'').trim();
    }
    return '';
  }
  function rowOf(el){
    var r = el;
    while (r && !(r.querySelector && r.querySelector('span.text-xs') && r.querySelector('span.truncate'))) r = r.parentElement;
    return r;
  }
  var btns = document.querySelectorAll('button');
  for (var j=0;j<btns.length;j++){
    var b = btns[j];
    if (b.offsetParent === null) continue;
    if ((b.textContent||'').trim() !== 'Saved') continue;
    var row = rowOf(b);
    if (row && leafName(row) === name) return true;
  }
  return false;
})()"#;

const CLICK_BUTTON_JS: &str = r#"(function(){
  var txt = __TXT__;
  var btns = document.querySelectorAll('button');
  for (var i=0;i<btns.length;i++){
    if (btns[i].offsetParent === null) continue; // skip hidden (e.g. behind a dialog)
    if ((btns[i].textContent||'').trim() === txt){
      var b = btns[i];
      var disabled = !!(b.disabled || b.getAttribute('aria-disabled') === 'true' ||
                        (b.className && String(b.className).indexOf('Mui-disabled') !== -1));
      if (disabled) return { found:true, disabled:true };
      try { b.scrollIntoView({ block:'center' }); } catch(e){}
      var r = b.getBoundingClientRect();
      return { found:true, disabled:false, x: r.left + r.width/2, y: r.top + r.height/2 };
    }
  }
  return { found:false };
})()"#;

// Any visible modal open? Used by sync-dialog detection.
const ANY_DIALOG_OPEN_JS: &str = r#"(function(){
  var dlgs = document.querySelectorAll('[role="dialog"]');
  for (var i=0;i<dlgs.length;i++){ if (dlgs[i].offsetParent !== null) return true; }
  return false;
})()"#;

// Hook the in-page clipboard, then return the "Show zip structure" copy button's
// viewport centre so the workflow can drive a REAL human click onto it (not an
// inhuman programmatic btn.click()). Scans ALL visible [role=dialog]s for the one
// holding a copy control, and NEVER touches page-level copy buttons (which would
// grab the task rubric).
const COPY_HOOK_LOCATE_JS: &str = r#"(function(){
  window.__golem_clip = null;
  try {
    var cb = navigator.clipboard;
    if (!cb){ cb = {}; try { Object.defineProperty(navigator,'clipboard',{value:cb,configurable:true}); } catch(e2){} }
    cb.writeText = function(t){ window.__golem_clip = String(t); return Promise.resolve(); };
  } catch(e){}
  var dlgs = document.querySelectorAll('[role="dialog"]');
  for (var i=dlgs.length-1;i>=0;i--){
    var d = dlgs[i];
    if (d.offsetParent === null) continue;
    var btn = d.querySelector('button[aria-label="copy"]');
    if (!btn){ var ic = d.querySelector('svg[data-testid="ContentCopyIcon"]'); btn = ic ? ic.closest('button') : null; }
    if (btn && btn.offsetParent !== null){
      try { btn.scrollIntoView({ block:'center' }); } catch(e3){}
      var r = btn.getBoundingClientRect();
      return { x: r.left + r.width/2, y: r.top + r.height/2 };
    }
  }
  return null;
})()"#;

// Read whatever the hooked clipboard captured (the copied ZIP YAML), or null.
const READ_CLIP_JS: &str = r#"(function(){ return window.__golem_clip; })()"#;

// The zip-structure popup is open iff a visible [role=dialog] holds a copy control.
const ZIP_OPEN_JS: &str = r#"(function(){
  var dlgs = document.querySelectorAll('[role="dialog"]');
  for (var i=dlgs.length-1;i>=0;i--){
    var d = dlgs[i];
    if (d.offsetParent === null) continue;
    if (d.querySelector('button[aria-label="copy"]') || d.querySelector('svg[data-testid="ContentCopyIcon"]')) return true;
  }
  return false;
})()"#;

// Locate the confirm button inside the "Clear saved assets?" dialog. Scopes to
// the topmost visible [role=dialog], verifies the title, and returns the centre
// of the error-coloured "Clear all" button (NOT the Cancel button, which uses
// hdk-* classes, and NOT the page's own Clear all outside the dialog).
const CONFIRM_CLEAR_DIALOG_JS: &str = r#"(function(){
  var dlgs = document.querySelectorAll('[role="dialog"]');
  var dlg = null;
  for (var i=dlgs.length-1;i>=0;i--){ if (dlgs[i].offsetParent !== null){ dlg = dlgs[i]; break; } }
  if (!dlg) return null;
  if ((dlg.textContent||'').indexOf('Clear saved assets?') === -1) return null;
  var btns = dlg.querySelectorAll('button');
  var target = null;
  for (var j=0;j<btns.length;j++){
    var b = btns[j];
    if (b.offsetParent === null) continue;
    var cls = b.className ? String(b.className) : '';
    var isErr = cls.indexOf('MuiButton-colorError') !== -1 || cls.indexOf('MuiButton-containedError') !== -1;
    if ((b.textContent||'').trim() === 'Clear all' && isErr){ target = b; break; }
  }
  // Fallback: any MUI error button in the dialog (text could vary while loading).
  if (!target){
    for (var k=0;k<btns.length;k++){
      var bb = btns[k];
      if (bb.offsetParent === null) continue;
      var c2 = bb.className ? String(bb.className) : '';
      if (c2.indexOf('MuiButton-colorError') !== -1 || c2.indexOf('MuiButton-containedError') !== -1){ target = bb; break; }
    }
  }
  if (!target) return null;
  try { target.scrollIntoView({ block:'center' }); } catch(e){}
  var r = target.getBoundingClientRect();
  return { x: r.left + r.width/2, y: r.top + r.height/2 };
})()"#;

const SET_VALUE_JS: &str = r#"(function(){
  var el = document.querySelector(__SEL__);
  if (!el) return false;
  var proto = el.tagName === 'TEXTAREA' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
  var desc = Object.getOwnPropertyDescriptor(proto, 'value');
  if (desc && desc.set) { desc.set.call(el, __VAL__); } else { el.value = __VAL__; }
  el.dispatchEvent(new Event('input', { bubbles:true }));
  el.dispatchEvent(new Event('change', { bubbles:true }));
  return true;
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_parsing() {
        assert_eq!(
            task_id_from_url(
                "https://feather.openai.com/tasks/d3bb5691-2ad7-44f4-8aa9-1298ead0c2fc/stage/execution"
            )
            .as_deref(),
            Some("d3bb5691-2ad7-44f4-8aa9-1298ead0c2fc")
        );
        assert_eq!(task_id_from_url("https://feather.openai.com/campaigns"), None);
    }

    #[test]
    fn date_parsing_and_ordering() {
        // Option<NaiveDateTime> orders None < Some(a) < Some(b) iff a < b, so
        // comparing the parsed Options directly checks both parse + ordering.
        let before = parse_feather_date("Jun 17, 2026, 09:33 AM");
        let early = parse_feather_date("Jun 17, 2026, 09:48 AM");
        let later = parse_feather_date("Jun 17, 2026, 11:55 AM");
        let next_day = parse_feather_date("Jun 18, 2026, 05:03 PM");
        assert!(before.is_some() && early.is_some() && later.is_some() && next_day.is_some());
        assert!(before < early);
        assert!(early < later);
        assert!(later < next_day);
        assert!(parse_feather_date("not a date").is_none());
    }

    #[test]
    fn name_classifiers() {
        assert!(is_snapshot_subfolder("20260617_164811"));
        assert!(is_snapshot_subfolder("20260617_164811/")); // trailing text tolerated
        assert!(is_snapshot_subfolder("20260617_164811 (3 files)"));
        assert!(!is_snapshot_subfolder("2026061_164811")); // too short for the prefix
        assert!(!is_snapshot_subfolder("Session_151622"));
        assert!(is_session_name("Session_151622"));
        assert!(!is_session_name("Session_"));
        assert!(!is_session_name("session_1")); // case-sensitive
        assert!(!is_session_name("20260617_164811"));
    }

    #[test]
    fn earliest_subfolder_from_zip_yaml() {
        // Typical ZIP-structure YAML: nested subfolder paths, one per snapshot.
        let yaml = "\
- 20260611_164811/ROCMonitor.cir
- 20260611_164811/graphs1.png
- 20260611_121700/ROCMonitor.cir
- 20260612_090000/ROCMonitor_final.cir
- Files.txt
";
        assert_eq!(earliest_subfolder_utc(yaml).as_deref(), Some("20260611_121700"));
        // No subfolders -> None.
        assert_eq!(earliest_subfolder_utc("- Files.txt\n- readme.md").as_deref(), None);
        // Tolerates quotes / indentation around the path component.
        let y2 = "    \"20260601_000000/a.cir\"\n  20260531_235959/b.cir";
        assert_eq!(earliest_subfolder_utc(y2).as_deref(), Some("20260531_235959"));
    }

    #[test]
    fn final_and_working() {
        assert!(is_final("roc_final.cir"));
        assert!(!is_final("roc.cir"));
        assert!(!is_final("final.cir")); // stem is "final", not "*_final"
        assert_eq!(working_from_final("roc_final.cir"), "roc.cir");
        assert_eq!(working_from_final("a_b_final.blend"), "a_b.blend");
    }

    #[test]
    fn filename_extraction_and_classification() {
        // The snapshots ZIP YAML: the working file recurs once per subfolder; the
        // final + extras live in only some subfolders (paths are stripped to names).
        let yaml = "
        20260617_164811/roc.cir
        20260617_165312/roc.cir
        20260617_170102/roc.cir
        20260617_170102/roc_final.cir
        20260617_165312/graphs.png
        Files.txt
        ";
        let files = extract_filenames(yaml);
        let final_file = files.iter().find(|f| is_final(f)).cloned().unwrap_or_default();
        assert_eq!(final_file, "roc_final.cir");
        let working = working_from_final(&final_file);
        assert_eq!(working, "roc.cir");
        assert_eq!(files.iter().filter(|f| **f == working).count(), 3); // one per subfolder
        let mut others: Vec<String> = files
            .iter()
            .filter(|f| **f != final_file && **f != working && f.as_str() != "Files.txt")
            .cloned()
            .collect();
        others.sort();
        others.dedup();
        assert_eq!(others, vec!["graphs.png".to_string()]);
    }

    #[test]
    fn local_start_filters_sessions() {
        // Sessions are filtered on their DISPLAYED dates; the start comes from the
        // earliest snapshot subfolder's DISPLAYED date (read off the expanded row,
        // same basis), so the comparison is apples-to-apples. Converting the UTC
        // subfolder NAME via the browser mismatched feather's display zone.
        let start_local = parse_feather_date("Jun 17, 2026, 09:48 AM"); // earliest subfolder
        let s_151622 = parse_feather_date("Jun 17, 2026, 09:33 AM"); // before -> excluded
        let s_151976 = parse_feather_date("Jun 17, 2026, 11:55 AM"); // after  -> included
        let s_152681 = parse_feather_date("Jun 17, 2026, 02:31 PM"); // after  -> included
        assert!(start_local.is_some());
        assert!(s_151622 < start_local);
        assert!(s_151976 >= start_local);
        assert!(s_152681 >= start_local);
    }

    #[test]
    fn mp4_runtime_rule() {
        let yaml = "rec1.mp4 rec2.mp4 rec3.mp4 thumb.png clip.mp4";
        let n = yaml.matches(".mp4").count() as i64;
        assert_eq!(n, 4);
        assert_eq!(session_runtime_min(n), 10); // (4-2)*5
        assert_eq!(session_runtime_min(27), 125); // the spec example: (27-2)*5
        assert_eq!(session_runtime_min(1), 0); // never negative
        assert_eq!(session_runtime_min(0), 0);
    }
}
