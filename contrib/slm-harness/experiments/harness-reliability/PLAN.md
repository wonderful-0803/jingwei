# E005: bounded generic harness reliability

Approved scope: improve progress, submission contract, and pre-tool correction. Existing dev baseline d432583. No changes to H001 or frozen verified50 artifacts.

- Core: opt-in WorkflowPolicy limits inspection tools and reserves final steps by narrowing the existing authorized selector; never grants a tool. One optional no-progress recovery and optional completion rejection recovery share the existing correction/step/task ledger. Defaults retain existing stop behavior.
- Correction: IncompleteResponse is safe to propose again only before any committed execution. It includes length truncation but also other incomplete finish reasons; the action layer currently erases the exact finish reason. Feedback asks for shorter complete actions and never replays side effects.
- Host: trusted profile names inspection/submission tools. Only a designated successful submitting tool may establish a receipt, mutation invalidates it, completion verifies current artifact fingerprints. Receipt means delivery integrity, not hidden-answer correctness.
- Domain: v2 Excel inspection is read-only; standalone submit alone registers executable solution and artifact. Keep v1 profile/worker behavior available for baseline reproducibility. Non-Excel text profile uses the same host and generic policy.
- Verification: deterministic real harness tests (fake model boundary), receipt invalidation tests, sandbox regression, small paired failure-task reruns with separate paths and provenance. No 50/400 rerun or claim of benchmark-wide improvement.
