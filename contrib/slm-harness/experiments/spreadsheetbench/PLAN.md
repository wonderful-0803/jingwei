# Verified 400：jingwei agent comparison

User confirmed full Verified 400 for both installed Ollama models on 2026-09-21.
This is an independent experiment; no jingwei core or H001 design changes.

## Frozen protocol

- Official source: https://github.com/RUCKBReasoning/SpreadsheetBench at 49b73a94775fb489063f60ca1865e3a650079a79.
- Data: https://huggingface.co/datasets/KAKA22/SpreadsheetBench/resolve/main/spreadsheetbench_verified_400.tar.gz. Archive hash and all task IDs recorded in manifest before inference.
- Models: hf.co/unsloth/Qwen3.5-9B-GGUF:Q8_0 and hf.co/unsloth/Qwen3.8-27B-GGUF:UD-IQ3_S.
- Same jingwei ReferenceAgent, JSON action protocol, dynamically selected tools/skill profile, temperature 0, seed 7, reasoning disabled, context 32768, output reserve 4096, maximum 8 agent steps and 2 corrections. Model digests recorded.
- Model sees instruction, answer position, sheet names and first five rows of case 1. It can inspect that input with Python and submit a standalone Python solution. All gold files remain outside its sandbox. Verified supplies one initial/golden pair per task. No correctness feedback from gold.
- Tools execute in bubblewrap without network/host home, with read-only runtime/input and task-scoped writable work area. Resource and time limits apply. The final submitted program is replayed from fresh input for the supplied Verified input.
- Final XLSX is recalculated using explicit LibreOffice 26.8.0.3 and its bundled Python, with separate profile, macros disabled, no external-link update. Source and gold unchanged.
- Use unmodified official cell_level_compare with the documented Verified metadata adapter (GRADING.md). Main score = fraction of all 400 tasks passing their supplied case; also publish category counts. Missing outputs fail. Infrastructure failures are separately labeled; never quietly removed from denominator.
- Official comparator checks values in answer_position, not chart/format quality; this benchmark cannot certify all product requirements. Our agent protocol is not the original paper's exact prompt/inference setup, and LibreOffice is not Microsoft Excel. Report as this harness configuration on the official dataset/grader, not an official leaderboard submission.
- Per-task traces, generated code, XLSX outputs, execution logs, scoring and progress are persisted; completed results are resumable only against an identical manifest.

## Validation

1. XLOOKUP native, XLSX reopen, dependency recalculation: completed 33/33 with exported formulas preserved.
2. Test sandbox cannot read host or gold, cannot connect network, starts each final solution from a clean working copy, and does not accept symlink outputs.
3. Validate an executor control against a synthetic workbook and official comparator; test missing output and wrong answer.
4. Run a small plumbing pilot with both models, freeze implementation, then process all 400 per model with checkpointing.

Outputs live under artifacts/spreadsheetbench; code and protocol under experiments/spreadsheetbench.
