# Verified 400 grading compatibility

The official 2025-12 Verified archive contains **one** initial/golden pair per task, not the original 912 set's three-case layout. There are 395 `*_init.xlsx` / `*_golden.xlsx` pairs and five `initial.xlsx` / `golden.xlsx` pairs. All 400 IDs are retained.

We call the unmodified upstream `cell_level_compare` function (commit 49b73a9) after adapting metadata to its `(sheet_name, bounded_cell_range)` interface. This preserves official value normalization (numeric rounding to two decimals, date normalization, empty string/None equivalence). The upstream command-line wrapper cannot directly read this archive's filenames/metadata.

Metadata normalization, fixed before model inference:
- Explicit sheet-qualified answer_position takes precedence.
- Otherwise use answer_sheet when present; if absent use the first sheet, as upstream does.
- Task 60-7 lists all participating sheets in answer_sheet; its instruction explicitly names Consolidated Tracker as output. The first listed sheet is used.
- Strip misplaced surrounding quotes in published sheet/range notation.
- Preserve commas inside sheet names, including task 130-9's literal `b2b, sez, de` sheet.
- For whole-column/whole-row ranges, bound the unspecified dimension by max(gold used extent, predicted used extent). This retains checks for extra populated output rows.
- Task 73-45's published `'Sheet1'!BD2:308` is normalized to `'Sheet1'!BD2:BD308`, consistent with its instruction (column BD) and data_position. Source JSON and golden workbook remain untouched.

Gold is never mounted in the code sandbox or included in model feedback. Gold-self comparison and unchanged-input baseline are recorded separately. Only predictions are recalculated with LibreOffice 26.8; official golden caches remain authoritative and unmodified.

Limitations: upstream comparison checks cell values, with fill/font checks commented out. It cannot measure rich-text highlighting, general formatting, charts, hidden-gridline settings, or compliance with a request to use a formula rather than constants. Some formatting-only tasks may already pass unchanged. We report the unchanged-input baseline and avoid interpreting value accuracy as full product compliance. Formula results depending on today's date may differ from historical golden caches; these are not silently corrected.
