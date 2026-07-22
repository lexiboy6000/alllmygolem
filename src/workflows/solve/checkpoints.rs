//! "Solve: checkpoints" — after solving, ask Claude for N short, chronological
//! activity-log notes describing how the netlist was built. They become the
//! Vagon "User Activity Log" entries the execute-on-VM workflow posts at evenly
//! spaced points while typing. `n = ceil(anticipated_hours) + 2` (so ≥1/hour),
//! reviewed/edited by the operator at the pipeline's review gate before use.

use std::time::Duration;

use crate::prelude::*;

use super::util;

pub struct SolveCheckpoints;

#[async_trait]
impl Workflow for SolveCheckpoints {
    fn name(&self) -> &'static str {
        "Solve: checkpoints"
    }
    fn description(&self) -> &'static str {
        "Generate short, reviewable Vagon-log checkpoint notes for a solved netlist (Claude)."
    }
    fn requires_browser(&self) -> bool {
        false
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional("task_id", "Task id (blank = newest bundle)", "")]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let task_id = ctx.input("task_id").map(str::to_string);
        let (id, bundle) = util::find_bundle(&ctx.settings, task_id.as_deref())?;
        let ws = ctx.settings.output_dir.join("solve").join(&id);

        let netlist_path = ws.join("final").join("solution.cir");
        let netlist = std::fs::read_to_string(&netlist_path).map_err(|e| {
            GolemError::Io(format!("read netlist {}: {e}", netlist_path.display()))
        })?;
        if netlist.trim().is_empty() {
            return Err(ctx
                .stop_and_warn(format!(
                    "netlist {} is empty — run the solve step first.",
                    netlist_path.display()
                ))
                .await);
        }

        let hours = bundle.anticipated_hours.unwrap_or(3.0);
        let n = checkpoint_count(hours);
        ctx.output(format!(
            "generating {n} checkpoint note(s) for task {id} ({hours} h estimate)"
        ));

        ctx.step("ask Claude for checkpoints").await?;
        let prompt = checkpoint_prompt(&bundle.prompt, &netlist, n);
        let timeout = Duration::from_secs(ctx.settings.claude_timeout_secs.max(60));
        let claude = util::claude_bin(&ctx.settings);
        let model = ctx.settings.solve_model.clone();
        let effort = ctx.settings.solve_effort.clone();
        let mut args: Vec<&str> = vec![
            "-p",
            prompt.as_str(),
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
        let _ = std::fs::create_dir_all(&ws);
        let out = ctx.run_claude(&claude, &args, Some(&ws), Some(timeout)).await?;

        let checkpoints = parse_checkpoints(&out.stdout, n);
        if checkpoints.is_empty() {
            return Err(ctx
                .stop_and_warn(
                    "Claude returned no usable checkpoint notes; add them manually at the review step.",
                )
                .await);
        }
        ctx.output(format!("got {} checkpoint note(s)", checkpoints.len()));

        let path = ws.join("checkpoints.json");
        let doc = json!({ "checkpoints": checkpoints });
        std::fs::write(&path, serde_json::to_string_pretty(&doc)?)
            .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
        ctx.output(format!("saved checkpoints -> {}", path.display()));

        Ok(WorkflowOutcome::CompletedWith(json!({
            "task_id": id,
            "count": checkpoints.len(),
        })))
    }
}

/// `n = ceil(hours) + 2` (guarantees >1 per hour), minimum 3.
fn checkpoint_count(hours: f64) -> usize {
    let h = if hours.is_finite() && hours > 0.0 { hours } else { 3.0 };
    (h.ceil() as usize + 2).max(3)
}

fn checkpoint_prompt(task_prompt: &str, netlist: &str, n: usize) -> String {
    format!(
        "You are documenting the work of building a SPICE netlist for this task, as short \
         activity-log notes a contractor would post at evenly-spaced points WHILE working.\n\n\
         TASK:\n{task}\n\nFINAL NETLIST:\n```\n{netlist}\n```\n\n\
         Write exactly {n} checkpoint notes, in chronological build order (early scaffolding → \
         refinements → final verification). Each is a terse 3-8 word phrase describing what was \
         just done (e.g. \"added input source and load\", \"tuned filter cutoff\", \"verified \
         transient response\"). No numbering, no preamble, no trailing punctuation. Output ONLY \
         JSON: {{\"checkpoints\": [\"...\", \"...\"]}}",
        task = truncate(task_prompt, 4000),
        netlist = truncate(netlist, 6000),
        n = n,
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// Parse `{"checkpoints":[...]}` or a bare JSON array from Claude's result text;
/// fall back to numbered/bulleted lines. Clamps to `n`.
fn parse_checkpoints(text: &str, n: usize) -> Vec<String> {
    if let Some(list) = extract_json_checkpoints(text) {
        return clamp(list, n);
    }
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', '•', '·']).trim())
        .map(strip_leading_number)
        .map(|l| l.trim_matches(['"', '\'', ',']).trim().to_string())
        .filter(|l| !l.is_empty() && l.chars().count() < 120 && !l.contains(['{', '}']))
        .collect();
    clamp(lines, n)
}

fn clamp(mut v: Vec<String>, n: usize) -> Vec<String> {
    v.truncate(n);
    v
}

fn strip_leading_number(l: &str) -> String {
    let t = l.trim_start();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    let rest = &t[digits..];
    if digits > 0 && (rest.starts_with('.') || rest.starts_with(')')) {
        rest[1..].trim_start().to_string()
    } else {
        t.to_string()
    }
}

fn extract_json_checkpoints(text: &str) -> Option<Vec<String>> {
    let try_parse =
        |s: &str| serde_json::from_str::<Value>(s).ok().and_then(|v| checkpoints_from_value(&v));
    if let Some(list) = try_parse(text.trim()) {
        return Some(list);
    }
    if let (Some(a), Some(b)) = (text.find('{'), text.rfind('}'))
        && a < b
        && let Some(list) = try_parse(&text[a..=b])
    {
        return Some(list);
    }
    if let (Some(a), Some(b)) = (text.find('['), text.rfind(']'))
        && a < b
        && let Some(list) = try_parse(&text[a..=b])
    {
        return Some(list);
    }
    None
}

fn checkpoints_from_value(v: &Value) -> Option<Vec<String>> {
    let arr = if v.is_array() {
        v.as_array()
    } else {
        v.get("checkpoints").and_then(Value::as_array)
    };
    let out: Vec<String> = arr?
        .iter()
        .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_is_ceil_hours_plus_two() {
        assert_eq!(checkpoint_count(5.0), 7);
        assert_eq!(checkpoint_count(3.0), 5);
        assert_eq!(checkpoint_count(4.2), 7); // ceil(4.2)=5 -> 5+2=7
        assert_eq!(checkpoint_count(0.5), 3); // ceil=1 -> 1+2=3
        assert_eq!(checkpoint_count(0.0), 5); // unknown -> 3h -> 5
        assert_eq!(checkpoint_count(f64::NAN), 5);
    }

    #[test]
    fn parse_object_array_and_lines() {
        let obj = r#"Here you go: {"checkpoints": ["a", "b", "c"]} done"#;
        assert_eq!(parse_checkpoints(obj, 5), vec!["a", "b", "c"]);
        let arr = r#"["x","y"]"#;
        assert_eq!(parse_checkpoints(arr, 5), vec!["x", "y"]);
        let lines = "1. first thing\n2) second thing\n- third thing";
        assert_eq!(
            parse_checkpoints(lines, 5),
            vec!["first thing", "second thing", "third thing"]
        );
        // clamps to n
        assert_eq!(parse_checkpoints(r#"["a","b","c","d"]"#, 2), vec!["a", "b"]);
    }
}
