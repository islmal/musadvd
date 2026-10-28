# Project-name prefix for the authoring work directory

## Goal
`<output>/.convertx-work` → `<output>/<project name>.convertx-work` (e.g. `Urlaub 2025.convertx-work`).
Side effect: two projects can build into the same output directory in parallel without deleting
each other's work directory.

## Approach
- The marker file *inside* the folder stays `.convertx-work` (`WORK_DIR`); recognition of our own
  folders (only marked ones are deleted) is unchanged.
- New `pub fn work_dir(out: &Path, opts: &BuildOptions) -> PathBuf` in `src/dvd.rs`:
  name = `opts.project_name` (non-blank) → `opts.menu_title` (non-blank) → first title's
  `StreamSelection.label` → first input's file stem; passed through `safe_folder_name`, then
  `format!("{name}{WORK_DIR}")`.
- The work dir is created *before* probing, so the name must come from `opts`, not from the probed
  `titles` (same precedence as `default_project_name`, src/dvd.rs ~1162 / src/bd.rs ~596).

## Files
1. `src/dvd.rs`: add `work_dir`; use it where `out.join(WORK_DIR)` is built: creation (~1082) and
   cancel cleanup in `build_dvd` (~1040); update module doc (line 6).
2. `src/bd.rs`: same at creation (~525), import `work_dir`; module doc (line 7).
3. `src-tauri/src/burn.rs`: `delete_dvd_output` (~251) looks for
   `<project folder name>.convertx-work` next to the project folder; adjust its test (~476) to
   create `Film.convertx-work`.
4. `tests/build_dvd.rs:145`, `tests/cancel.rs:83`, `tests/playback_options.rs:71`: assert no entry
   in the output dir ends with `.convertx-work` (instead of checking the fixed name).
5. `STATUS.md` line ~116 (marker note): document the new folder name.

## Order (micro-commit after each green step)
1. `work_dir` + unit test in `dvd.rs` → `cargo check --workspace` → commit.
2. Switch DVD, BD and cancel paths → `cargo check --workspace` → commit.
3. `burn.rs` + integration tests → commit.

## Verification
- New unit test for `work_dir`: `project_name = "Urlaub 2025"` → `Urlaub 2025.convertx-work`;
  blank → `menu_title`; both blank → first input's stem; `a/b:c` gets sanitised.
- Adjusted `burn.rs` test: deletion finds and removes `Film.convertx-work`.
- `cargo test --workspace` (~3 min): cancel / build_dvd / playback_options prove no work dir is
  left after success and after cancel.
- `cargo check --lib --tests --target x86_64-unknown-linux-gnu`.

## Known limits (accepted)
- Old `.convertx-work` leftovers from earlier aborted runs are no longer auto-cleaned.
- Project folder `Film (2)` (name collision) still gets work dir `Film.convertx-work`; harmless,
  the work dir is removed after a successful build.
