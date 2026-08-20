//! Optional variation pass over claude's written answers, run by workflow 7
//! between the judging and the typing: a LOCAL model (Qwen3 via a llama.cpp
//! server) rewrites each sentence with slight-but-noticeable variation, so
//! the typed text doesn't carry one fixed authorial voice across every
//! submission. Covers the `open_feedback` paragraphs AND the
//! `same_verdict_reason` note -- both get typed into the page for real (the
//! feedback boxes always, the reason whenever the "All ratings are the same"
//! confirmation pops on submit), so both need the same treatment.
//!
//! Fidelity is the whole design problem: feedback must keep every number,
//! filename, quoted string, attribution and verdict exactly (the platform's
//! own rubric demands concrete, checkable citations). Three nested defenses,
//! each validated against real feedback paragraphs on 2026-08-15:
//!
//! 1. a MECHANICAL GUARD on every rewrite: length bounds, every number /
//!    quoted span / file.ext token from the original present verbatim,
//!    Response-A/B mention counts unchanged (catches dropped attributions
//!    and person shifts), and no em/en dashes introduced (an LLM tell the
//!    judging prompt deliberately bans, which Qwen likes to add back);
//! 2. a CHECKER pass by the same local model with a step-by-step checklist
//!    (judgment, attribution, numbers/quotes, fluency) per rewritten
//!    sentence -- prompted to reason first and output a JSON verdict last,
//!    which is what makes a 4B model catch inverted verdicts ("B takes it"
//!    -> "B rejects it") it misses when asked for bare JSON (14/14 on the
//!    validation battery, zero false positives);
//! 3. a bounded FIX loop: a flagged sentence is rewritten once more WITH the
//!    checker's complaint, re-guarded and re-checked; anything still bad
//!    reverts to claude's original sentence, and a paragraph that ends up
//!    too short reverts wholesale.
//!
//! The pass is opt-in by infrastructure: if no llama.cpp server answers at
//! `GOLEM_REWRITER_URL` (default `http://127.0.0.1:8091`), the step reports
//! that and leaves claude's wording untouched. It NEVER fails the round --
//! every error path degrades to "no variation".
//!
//! The rewritten text is persisted back into `claude_answers`, so the apply
//! pass, the verify passes and step 8's re-reads all see one consistent
//! text.

use rand::RngExt;

use crate::prelude::*;

use super::util;

/// Where the llama.cpp server is expected, unless `GOLEM_REWRITER_URL` says
/// otherwise. Start it with e.g.:
/// `llama-server -m qwen3-4b-instruct-q4km.gguf --port 8091 --no-webui`
const DEFAULT_REWRITER_URL: &str = "http://127.0.0.1:8091";

fn rewriter_url() -> String {
    std::env::var("GOLEM_REWRITER_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REWRITER_URL.to_string())
}

const REWRITE_SYS: &str = "You rewrite sentences with noticeable but faithful variation. Rules: \
preserve every fact, number, measurement, name, file reference, quoted string and claim EXACTLY \
as written - never add, drop, or alter any detail; never change which response (A or B) a \
statement is about, and never change the verdict or outcome stated; DO vary the wording clearly: \
different sentence opening, synonyms, reordered clauses where meaning allows; keep roughly the \
same length and the same plain, direct, first-person tone; never use em dashes; output ONLY the \
rewritten sentence, nothing else.";

const CHECK_SYS: &str = "You check whether a rewritten sentence faithfully preserves an original \
sentence from an annotator's feedback comparing Response A and Response B. Work through this \
checklist, each in one short line:\n\
1. JUDGMENT: state the original's verdict or outcome, then the rewrite's. Same?\n\
2. ATTRIBUTION: who or which response each claim belongs to, original vs rewrite. Same?\n\
3. NUMBERS & QUOTES: identical in both?\n\
4. FLUENCY: does the rewrite read as natural English?\n\
Style and wording differences are fine; identical sentences are fine. Then on the FINAL line \
output only a JSON object: {\"ok\": true} if all checks pass, else {\"ok\": false, \"issue\": \
\"<one short sentence>\"}";

/// The `same_verdict_reason` floor a varied rewrite must stay above. The
/// dialog it answers states its real minimum only when it pops at submit
/// time (see `SameRatingsDialog::min`), so it can't be read here; the
/// judging prompt asks claude for 60-300 characters for exactly that
/// reason, and the same 60 serves as the revert threshold -- a rewrite
/// below it risks `same_verdict_justification` skipping the reason for a
/// fallback that was never varied.
const SAME_VERDICT_FLOOR: usize = 60;

/// Rewrite the written answers inside `task_dir/claude_answers` -- the
/// `open_feedback` paragraphs and the `same_verdict_reason` note -- with
/// slight variation, checked as described in the module docs, and write the
/// file back. `questions` supplies each feedback paragraph's minimum length
/// (the page's own gate) so a shrunken rewrite can never disable the submit;
/// the reason uses [`SAME_VERDICT_FLOOR`].
pub async fn vary_open_feedback(
    ctx: &mut WorkflowCtx,
    task_dir: &std::path::Path,
    questions: &[util::FeedbackQuestion],
) -> Result<()> {
    let path = task_dir.join("claude_answers");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
        return Ok(());
    };
    let paragraphs: Vec<String> = match value.get("open_feedback") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let reason: Option<String> = value
        .get("same_verdict_reason")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    if paragraphs.is_empty() && reason.is_none() {
        return Ok(());
    }

    let url = rewriter_url();
    // A dead server is the OFF switch, not an error.
    let health = ctx
        .run(
            "curl",
            &["-fsS", "-m", "3", &format!("{url}/health")],
            None,
            Some(Duration::from_secs(10)),
        )
        .await;
    if !health.map(|o| o.success()).unwrap_or(false) {
        ctx.output(format!(
            "local rewriter not reachable at {url} -- keeping claude's wording as-is \
             (start llama-server there to enable feedback variation)"
        ));
        return Ok(());
    }

    let mut out_paragraphs = Vec::with_capacity(paragraphs.len());
    let mut any_changed = false;
    for (pi, par) in paragraphs.iter().enumerate() {
        let min_chars = questions.get(pi).map(|q| q.min).unwrap_or(0).max(1) as usize;
        match vary_paragraph(ctx, &url, "feedback", par).await? {
            Some(varied) => {
                let final_len = varied.trim().chars().count();
                // The page gates the submit on this length; claude was asked
                // for 1.3-2x the minimum, so shrinking below it (or far below
                // the original) means the rewrite lost real content.
                if final_len < min_chars || final_len * 10 < par.trim().chars().count() * 7 {
                    ctx.warn(format!(
                        "feedback #{}: the varied text came out too short ({final_len} chars) \
                         -- keeping claude's wording",
                        pi + 1
                    ));
                    out_paragraphs.push(par.clone());
                } else {
                    any_changed |= varied != *par;
                    out_paragraphs.push(varied);
                }
            }
            None => out_paragraphs.push(par.clone()),
        }
    }

    // The same-verdict reason goes through the identical pipeline. Its
    // revert floor is fixed (see SAME_VERDICT_FLOOR): the dialog that
    // consumes it states its minimum only when it pops at submit time.
    let mut out_reason: Option<String> = None;
    if let Some(orig) = &reason {
        match vary_paragraph(ctx, &url, "same-verdict reason", orig).await? {
            Some(varied) => {
                let final_len = varied.trim().chars().count();
                if final_len < SAME_VERDICT_FLOOR
                    || final_len * 10 < orig.trim().chars().count() * 7
                {
                    ctx.warn(format!(
                        "same-verdict reason: the varied text came out too short \
                         ({final_len} chars) -- keeping claude's wording"
                    ));
                } else if varied != *orig {
                    any_changed = true;
                    out_reason = Some(varied);
                }
            }
            None => {}
        }
    }

    if !any_changed {
        ctx.output("wording unchanged -- nothing usable came back from the rewriter");
        return Ok(());
    }
    if let Some(obj) = value.as_object_mut() {
        if !paragraphs.is_empty() {
            obj.insert(
                "open_feedback".to_string(),
                Value::Array(out_paragraphs.into_iter().map(Value::String).collect()),
            );
        }
        if let Some(r) = out_reason {
            obj.insert("same_verdict_reason".to_string(), Value::String(r));
        }
    }
    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|e| GolemError::Other(format!("re-serialize claude_answers: {e}")))?;
    std::fs::write(&path, pretty)
        .map_err(|e| GolemError::Io(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// One paragraph through the split -> rewrite -> guard -> check -> fix
/// pipeline. `None` means "leave the original alone" (rewriter failure).
/// `label` names the paragraph in the summary log line.
async fn vary_paragraph(
    ctx: &mut WorkflowCtx,
    url: &str,
    label: &str,
    par: &str,
) -> Result<Option<String>> {
    let sents = split_sentences(par);
    let mut finals: Vec<String> = Vec::with_capacity(sents.len());
    let (mut varied, mut guarded, mut fixed, mut reverted) = (0usize, 0usize, 0usize, 0usize);
    for (i, s) in sents.iter().enumerate() {
        ctx.guard().await?;
        let prev = if i > 0 { Some(sents[i - 1].as_str()) } else { None };
        // first attempt with real variation, one calmer retry if the guard
        // objects, then give up on this sentence
        let mut candidate = None;
        for temp in [0.85, 0.6] {
            let Some(v) = rewrite(ctx, url, s, prev, temp, None).await? else {
                continue;
            };
            if guard_ok(s, &v).0 {
                candidate = Some(v);
                break;
            }
        }
        let Some(v) = candidate else {
            guarded += 1;
            finals.push(s.clone());
            continue;
        };
        if v == *s {
            finals.push(v);
            continue;
        }
        match check(ctx, url, s, &v).await? {
            CheckVerdict::Ok => {
                varied += 1;
                finals.push(v);
            }
            CheckVerdict::Flagged(issue) => {
                // one repair attempt carrying the checker's complaint
                let repaired = match rewrite(ctx, url, s, prev, 0.6, Some(&issue)).await? {
                    Some(r) if guard_ok(s, &r).0 => match check(ctx, url, s, &r).await? {
                        CheckVerdict::Ok => Some(r),
                        CheckVerdict::Flagged(_) => None,
                    },
                    _ => None,
                };
                match repaired {
                    Some(r) => {
                        fixed += 1;
                        varied += 1;
                        finals.push(r);
                    }
                    None => {
                        reverted += 1;
                        finals.push(s.clone());
                    }
                }
            }
        }
    }
    let kept = guarded + reverted;
    ctx.output(format!(
        "{label}: varied {varied} of {} sentence(s){}{}",
        sents.len(),
        if fixed > 0 {
            format!(" ({fixed} repaired after the checker flagged it)")
        } else {
            String::new()
        },
        if kept > 0 {
            format!(", kept claude's wording for {kept}")
        } else {
            String::new()
        },
    ));
    Ok(Some(util::normalize_feedback(&finals.join(" "))))
}

/// A checker verdict: acceptable, or flagged with its one-line complaint.
enum CheckVerdict {
    Ok,
    Flagged(String),
}

/// Ask the local model to rewrite `text` (optionally telling it what its
/// previous attempt got wrong). `None` on any transport/parse failure -- the
/// caller treats that as "keep the original".
async fn rewrite(
    ctx: &WorkflowCtx,
    url: &str,
    text: &str,
    prev: Option<&str>,
    temp: f64,
    problem: Option<&str>,
) -> Result<Option<String>> {
    let mut user = String::new();
    if let Some(p) = prev {
        user.push_str(&format!("Context (previous sentence, do NOT rewrite it): {p}\n"));
    }
    user.push_str(&format!("Sentence to rewrite: {text}"));
    if let Some(issue) = problem {
        user.push_str(&format!(
            "\n\nYour previous rewrite had a problem: {issue}\nProduce a corrected variation \
             that fixes exactly that."
        ));
    }
    Ok(llama_chat(ctx, url, REWRITE_SYS, &user, temp, 240)
        .await?
        .map(|s| straighten_quotes(s.trim()))
        .filter(|s| !s.is_empty()))
}

/// Replace typographic quotes with the straight ones a person types into a
/// plain textarea. Qwen writes "don\u{2019}t" with a curly apostrophe, and
/// typing U+2019 into the feedback box is a word-processor tell no quick
/// human answer would carry (and a guard hazard: a straight-quoted span from
/// the original wouldn't match its curly rewrite verbatim).
fn straighten_quotes(s: &str) -> String {
    s.replace(['\u{2018}', '\u{2019}'], "'")
        .replace(['\u{201C}', '\u{201D}'], "\"")
}

/// Run the checklist check on one original/rewrite pair. Transport or parse
/// failures count as FLAGGED with a generic issue: unverified variation must
/// never pass through.
async fn check(ctx: &WorkflowCtx, url: &str, orig: &str, rewrite: &str) -> Result<CheckVerdict> {
    let user = format!("ORIGINAL: {orig}\nREWRITE: {rewrite}");
    let Some(reply) = llama_chat(ctx, url, CHECK_SYS, &user, 0.0, 400).await? else {
        return Ok(CheckVerdict::Flagged("checker unavailable".into()));
    };
    // The verdict is the FINAL JSON object; everything before it is the
    // model's checklist reasoning (deliberate -- see module docs).
    let verdict = reply
        .rfind('{')
        .and_then(|start| reply[start..].rfind('}').map(|end| &reply[start..start + end + 1]))
        .and_then(|json| serde_json::from_str::<Value>(json).ok());
    Ok(match verdict {
        Some(v) if v.get("ok").and_then(Value::as_bool).unwrap_or(false) => CheckVerdict::Ok,
        Some(v) => CheckVerdict::Flagged(
            v.get("issue")
                .and_then(Value::as_str)
                .unwrap_or("checker flagged it without detail")
                .to_string(),
        ),
        None => CheckVerdict::Flagged("checker verdict was unparseable".into()),
    })
}

/// One chat call to the llama.cpp server via curl (the repo's HTTP tool of
/// choice). `None` on any failure; the callers all degrade gracefully.
async fn llama_chat(
    ctx: &WorkflowCtx,
    url: &str,
    system: &str,
    user: &str,
    temp: f64,
    max_tokens: u32,
) -> Result<Option<String>> {
    let seed: u32 = {
        let mut rng = rand::rng();
        rng.random()
    };
    let body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "temperature": temp,
        "top_p": 0.95,
        "max_tokens": max_tokens,
        "seed": seed,
    })
    .to_string();
    let endpoint = format!("{url}/v1/chat/completions");
    let out = ctx
        .run(
            "curl",
            &[
                "-fsS",
                "-m",
                "300",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "--data-binary",
                &body,
                &endpoint,
            ],
            None,
            Some(Duration::from_secs(320)),
        )
        .await;
    let Ok(out) = out else { return Ok(None) };
    if !out.success() {
        return Ok(None);
    }
    Ok(serde_json::from_str::<Value>(&out.stdout)
        .ok()
        .and_then(|v| {
            v.get("choices")?
                .get(0)?
                .get("message")?
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string)
        }))
}

// ---------------------------------------------------------------------------
// sentence splitting + the mechanical guard
// ---------------------------------------------------------------------------

/// Sentence-ending abbreviations that must NOT split ("e.g. the file").
/// Case-sensitive, matched against the word before a ". ".
const ABBREVS: [&str; 10] = ["e.g", "i.e", "vs", "etc", "approx", "cf", "Mr", "Ms", "Dr", "No"];

/// Split a feedback paragraph into sentences on `. `, `! `, `? `, keeping
/// common abbreviations glued. Imperfect splits (a quote containing `! `)
/// are harmless: the fragment is rewritten lightly like any sentence.
fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut parts = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        if chars.get(i + 1).is_some_and(|&n| n != ' ') {
            continue; // "0.7 mm", "task.pdf" -- not a boundary
        }
        if c == '.' {
            let before_dot: String = cur
                .trim_end_matches('.')
                .chars()
                .rev()
                .take_while(|ch| ch.is_alphanumeric() || *ch == '.')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if ABBREVS.contains(&before_dot.trim_end_matches('.')) {
                continue;
            }
        }
        let s = cur.trim().to_string();
        if !s.is_empty() {
            parts.push(s);
        }
        cur.clear();
    }
    let tail = cur.trim().to_string();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

/// The tokens a rewrite must carry verbatim: number runs, quoted spans
/// ('...' or "..." with word-boundary quotes, so possessives like "A's"
/// can't fake a span), and filename-shaped tokens (`stem.ext`).
fn preserve_tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    // number runs: digits plus the joiners that appear inside dimensions
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_digit() || matches!(chars[i], '.' | ',' | 'x' | '×' | '%'))
            {
                i += 1;
            }
            let mut run: String = chars[start..i].iter().collect();
            while run.chars().last().is_some_and(|c| !c.is_ascii_digit()) {
                run.pop();
            }
            if !run.is_empty() && !out.contains(&run) {
                out.push(run);
            }
        } else {
            i += 1;
        }
    }
    // quoted spans
    for quote in ['\'', '"'] {
        let mut i = 0;
        while i < chars.len() {
            let opens = chars[i] == quote
                && (i == 0 || !chars[i - 1].is_alphanumeric());
            if !opens {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            let closing = loop {
                if j >= chars.len() {
                    break None;
                }
                if chars[j] == quote
                    && chars.get(j + 1).is_none_or(|&n| !n.is_alphanumeric())
                {
                    break Some(j);
                }
                j += 1;
            };
            match closing {
                Some(j) if (2..=60).contains(&(j - i - 1)) => {
                    let span: String = chars[i + 1..j].iter().collect();
                    if !out.contains(&span) {
                        out.push(span);
                    }
                    i = j + 1;
                }
                _ => i += 1,
            }
        }
    }
    // filename-shaped tokens
    for word in s.split_whitespace() {
        let w = word.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | ')' | '(' | '\'' | '"'));
        let w = w.trim_end_matches('.');
        if let Some(dot) = w.rfind('.') {
            let ext = &w[dot + 1..];
            if dot >= 1
                && (2..=4).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphabetic())
                && !out.contains(&w.to_string())
            {
                out.push(w.to_string());
            }
        }
    }
    out
}

/// How many times a sentence mentions Response A resp. B -- the bare tokens
/// "A"/"B" and possessives "A's"/"B's". A rewrite that changes these counts
/// dropped or invented an attribution (the most dangerous class of rewrite
/// error observed), whatever else it got right.
fn ab_counts(s: &str) -> (usize, usize) {
    let (mut a, mut b) = (0, 0);
    for word in s.split(|c: char| !(c.is_alphanumeric() || c == '\'')) {
        match word {
            "A" | "A's" => a += 1,
            "B" | "B's" => b += 1,
            _ => {}
        }
    }
    (a, b)
}

/// The mechanical guard. Returns `(ok, why-not)`.
fn guard_ok(orig: &str, new: &str) -> (bool, String) {
    if new.trim().is_empty() {
        return (false, "empty".into());
    }
    let (ol, nl) = (orig.chars().count().max(1), new.chars().count());
    if nl * 2 < ol || nl * 10 > ol * 18 {
        return (false, "length out of bounds".into());
    }
    if ab_counts(orig) != ab_counts(new) {
        return (false, "Response A/B mentions changed".into());
    }
    if (new.contains('\u{2014}') && !orig.contains('\u{2014}'))
        || (new.contains('\u{2013}') && !orig.contains('\u{2013}'))
    {
        return (false, "introduced an em/en dash".into());
    }
    // Multiplicity matters: "170 x 170 x 170" corrupted to "175 x 170 x 170"
    // still CONTAINS "170", so tokens of 2+ chars must appear at least as
    // often as in the original. Single characters keep plain containment --
    // counting "1"s would also count the one inside "170".
    let missing: Vec<String> = preserve_tokens(orig)
        .into_iter()
        .filter(|t| {
            if t.chars().count() >= 2 {
                new.matches(t.as_str()).count() < orig.matches(t.as_str()).count()
            } else {
                !new.contains(t.as_str())
            }
        })
        .collect();
    if !missing.is_empty() {
        return (false, format!("missing verbatim: {missing:?}"));
    }
    (true, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_split_and_abbreviations_glue() {
        let s = "I checked it. All five screens work, e.g. the checkout. B fails! Why? Fine.";
        assert_eq!(
            split_sentences(s),
            vec![
                "I checked it.",
                "All five screens work, e.g. the checkout.",
                "B fails!",
                "Why?",
                "Fine.",
            ]
        );
        // decimals and filenames never split
        assert_eq!(
            split_sentences("The recess is 0.7 mm in task.pdf today. Done."),
            vec!["The recess is 0.7 mm in task.pdf today.", "Done."]
        );
    }

    #[test]
    fn preserved_tokens_cover_numbers_quotes_and_files() {
        let s = "A's page 1 reads 'BONUS 3 FLASHCARD' and ships task_data/00_3545f6b4.pdf \
                 at 170 x 170 mm, 0.7 mm deep, 393x852 px.";
        let toks = preserve_tokens(s);
        for want in ["1", "BONUS 3 FLASHCARD", "task_data/00_3545f6b4.pdf", "170", "0.7", "393x852"] {
            assert!(toks.iter().any(|t| t == want), "missing {want:?} in {toks:?}");
        }
        // the possessive apostrophes in "A's ... B's" must not fake a span
        let toks = preserve_tokens("A's cover beats B's cover.");
        assert!(
            toks.iter().all(|t| !t.contains("cover beats")),
            "possessives faked a quote: {toks:?}"
        );
    }

    #[test]
    fn the_guard_catches_the_observed_failure_classes() {
        let orig = "A's inner pot measures 170 x 170 x 170 mm, so B takes it.";
        // dropped attribution (A's -> The)
        assert!(!guard_ok(orig, "The inner pot measures 170 x 170 x 170 mm, so B takes it.").0);
        // corrupted number
        assert!(!guard_ok(orig, "A's inner pot measures 175 x 170 x 170 mm, so B takes it.").0);
        // introduced em dash
        assert!(!guard_ok(orig, "A's inner pot measures 170 x 170 x 170 mm \u{2014} so B takes it.").0);
        // a faithful light rewrite passes
        let (ok, why) =
            guard_ok(orig, "A's inner pot comes to 170 x 170 x 170 mm, so B takes it.");
        assert!(ok, "{why}");
        // person shift changes the A-count
        assert!(!guard_ok("A does win on the filename.", "I win on the filename.").0);
    }
}
