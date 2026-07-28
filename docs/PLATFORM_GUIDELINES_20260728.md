# Multimodal Agent Arena — official annotator guidelines (captured 2026-07-28)

Pasted from the platform's "Task Details" page. `ANSWER_CRITERIA_PROMPT` in
`src/workflows/first_test/util.rs` encodes the judging rules below — keep the
two in sync when the platform revises these.

## What you'll see

- **A task prompt** — what the AI was asked to do (text, may include images,
  PDFs, videos, 3D files, code, or other input materials).
- **Two artifacts (A and B)** — the AI's outputs: websites, games, images,
  data visualizations, slide decks, 3D models, code outputs, reports, etc.
- **A checklist of rubrics (when present)** — success criteria written
  specifically for this task. Some are objective (did it produce the file?),
  some subjective (does it look convincing?). Both are valid. Not all prompts
  include rubrics.

## How to evaluate

1. **Read the task prompt** and all input materials before looking at outputs.
2. **Review both artifacts.** Open and interact with both. Websites/games:
   click around, try things. Slide decks: browse ALL slides. Images/reports:
   examine carefully.
3. **Rate each rubric (when present):** for each criterion, rate both
   responses independently (A Pass/Fail, B Pass/Fail). You're not picking a
   winner per rubric — you're judging each response on its own merits.
   When no rubrics are shown, skip this step.
4. **Pick overall quality:** whichever better fulfills the task overall.
   Tie only when both are genuinely equal — if one is even slightly better,
   pick it. When no rubrics are present, evaluate on: instruction following,
   visual quality, content completeness, usability.

## Rules / tips

- Keep the prompt, rubrics, and input materials visible while annotating.
- Actually interact with interactive artifacts; don't just look.
- Treat each rubric as roughly equal weight unless one is clearly more
  central to the task.
- If one output completely fails (doesn't load, produces nothing, clearly
  broken): mark ALL its rubrics as Fail and pick the other. A broken output
  should always lose.
- Use Tie sparingly — there is almost always a slight edge one way.
- Flag problems in the comment field (confusing prompt, broken materials,
  unclear rubrics). Skipping is allowed for unreadable languages or clearly
  broken tasks.

## Worked examples (from the platform)

- **Website task:** rubric-by-rubric Good/Bad per response, then overall from
  the tallies ("A 4/4 vs B 2/4 → Response A"). "A looks nicer" with no rubric
  grounding is called out as a bad submission.
- **One output fails to load:** "Design B completely fails to render — no
  output visible. All B rubrics marked Bad. Overall: Response A." Calling a
  broken output "okay" is called out as wrong.

## QA rubric (how submissions are graded)

- **5 Exceptional** — thorough rubric-by-rubric analysis, interacted with
  both artifacts, rating clearly justified by specific rubric outcomes.
- **4 Strong** — good rubric coverage, reasonable rating, minor gaps.
- **3 Acceptable** — rating seems correct but rubric analysis is shallow.
- **2 Weak** — rating not well justified, rubrics mostly ignored, or didn't
  interact with artifacts.
- **1 Unacceptable** — wrong rating (e.g. preferred a broken output),
  ignored rubrics, or no evidence of reviewing the artifacts.

## Related assessment takeaways (2026-07-28)

The qualification assessment tested the same principles: function beats
polish; every criterion judged independently (a line graph's presence says
nothing about the other criteria); the overall pick must align with the
rubric outcomes (preferring a 0/4 response over a 3/4 one is a critical
error); judge from interaction, not a static glance.
