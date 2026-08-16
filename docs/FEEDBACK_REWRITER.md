# Feedback rewriter (local Qwen3 via llama.cpp)

Workflow 7 has an optional "vary the open feedback wording" step
(`src/workflows/first_test/vary_feedback.rs`): after Claude's judging
validates and before anything is typed, a local model rewrites each
open-feedback sentence with slight-but-noticeable variation, mechanically
guarded (numbers, quoted strings, filenames, Response-A/B attributions all
preserved verbatim; no em dashes; typographic quotes straightened) and
checked by the same local model through a judgment/attribution/numbers/
fluency checklist. Anything the guard or checker can't clear reverts to
Claude's original sentence, and a paragraph that shrinks below the page's
stated minimum reverts wholesale. The result is persisted back into
`claude_answers`, so apply, verify and step 8 all see one consistent text.

## The switch is the server

If nothing answers at the rewriter URL, the step logs
"local rewriter not reachable ... keeping claude's wording" and the round
proceeds untouched. Starting the server IS enabling the feature; stopping
it disables it. It can never fail a round.

- URL: `GOLEM_REWRITER_URL` env var, default `http://127.0.0.1:8091`
- Stop: `pkill -x llama-server`

## Starting it

The binary and model live outside the repo (they are ~2.5 GB):

- `~/models/llama-b10448/llama-server` — llama.cpp prebuilt (b10448,
  from ggml-org/llama.cpp GitHub releases, `bin-ubuntu-x64`)
- `~/models/qwen3-4b-instruct-q4km.gguf` — Qwen3-4B-Instruct-2507 Q4_K_M
  (from `unsloth/Qwen3-4B-Instruct-2507-GGUF` on Hugging Face; the official
  Qwen GGUF repo is gated)

```sh
setsid nohup ~/models/llama-b10448/llama-server \
  -m ~/models/qwen3-4b-instruct-q4km.gguf \
  --port 8091 -t 10 -c 4096 --no-webui \
  > ~/models/llama-server.log 2>&1 < /dev/null &
```

`-t 10` fits the i7-1255U (12 threads); tune to your core count. On that
CPU the pass costs roughly 60–90 s per ~400-character feedback paragraph,
comfortably inside a round's pacing budget.

## Why these exact prompts

The rewrite prompt pins verdicts and A/B attributions because those were
the two error classes observed at higher sampling temperatures. The checker
prompt walks a four-line checklist BEFORE its JSON verdict because a 4B
model needs the reasoning room: bare-JSON prompting scored 12/14 on the
2026-08-15 ground-truth battery (missing an inverted verdict and a person
shift), checklist-first scored 14/14 with zero false positives. Details and
the battery cases are in the commit messages for `f2b936a` and `33ddfa8`.
