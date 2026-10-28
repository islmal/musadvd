# Plan: make the GitHub repo public (2026-10-28)

Goal: licence-clean repo with a README, no personal e-mail and no tool binaries in the history.
Flipping the visibility stays the user's action.

## Decisions (user, 2026-10-28)
- Licence **GPL-3.0-or-later** (dvdauthor ports are GPL-2.0-or-later, tsMuxer-derived code is
  Apache-2.0; only GPL-3 combines both).
- Rewrite history: drop `bin/*.exe` blobs, map `malik <old e-mail>` →
  `islmal <136852714+islmal@users.noreply.github.com>`.
- Keep the name "ConvertX Rust", add a not-affiliated-with-VSO disclaimer.
- Keep `CLAUDE.md`, `STATUS.md`, `docs/plans/` public; neutralise the burner/drive line in `CLAUDE.md`.

## Audit findings (already checked)
- No secrets in history (updater pubkey is public; signing key lives in GitHub secrets). No releases yet.
  `.claude/` not tracked. No VSO template content in the repo (only the install path).
- History blobs: `bin/tsMuxeR.exe` 5.4 MB, `bin/spumux.exe` 5.3 MB, `bin/dvdauthor.exe` 2.5 MB.
- 42 commits carry the old e-mail; 6 carry `github.fracture622@passmail.com` (GitHub web
  commits by islmal) – leave those unless the user wants them mapped too.
- `main` is an ancestor of `native-authoring` (13 commits behind) → fast-forward.
- Branches `i18n-runtime-locales`, `live-player-hardening`, `native-text-subs` have 0 commits not in
  `native-authoring`.

## Files
New:
- `README.md` (English): what it is, features (DVD/BD/AVCHD, template + custom menus, Burn Studio,
  CLI + GUI in de/en/ru), status/limits (Windows first, only VLC-checked, short out-of-scope list from
  STATUS.md §1), build (copy ffmpeg/ffprobe into `bin/`, commands from CLAUDE.md), one CLI example
  taken from real `convertx-rust --help` output (do not invent flags), "Licence & credits" and the
  disclaimer: "Not affiliated with VSO Software. ConvertX templates are not included; they are only
  read from a user's own ConvertXToDVD installation."
- `LICENSE`: GPL-3.0 full text, verbatim from https://www.gnu.org/licenses/gpl-3.0.txt.
- `THIRD_PARTY_NOTICES.md`: dvdauthor (copyright holders from the headers of
  ldo/dvdauthor@fe8fe35 – verify they say "version 2 … or any later version"; which of our files
  derive from it), tsMuxer (Apache-2.0, copy its NOTICE if the repo has one; `bd_mux.rs`,
  `bd_clip.rs`), DejaVu fonts (→ `assets/fonts/DejaVu-LICENSE.txt`), FFmpeg (bundled by the
  installer, GPL-3 gyan.dev full build, source https://ffmpeg.org/download.html).

Change:
- `Cargo.toml`, `src-tauri/Cargo.toml`: `license = "GPL-3.0-or-later"`, accurate `description`;
  fix the `ab_glyph` comment (bundled DejaVu, not Windows fonts).
- `src/lib.rs:1`: drop "Clean-room" (part of the code is GPL-derived).
- `src/dvdvm.rs`, `src/dvd_mux.rs`, `src/ifo.rs`, `src/spumux.rs`, `src/bd_mux.rs`, `src/bd_clip.rs`:
  one doc line each with origin, original copyright and licence (see THIRD_PARTY_NOTICES.md).
- `src-tauri/tauri.conf.json`: resource `../bin/dvdauthor-COPYING.txt` → `../LICENSE` and
  `../THIRD_PARTY_NOTICES.md`.
- `.gitignore`: fix the stale "see STATUS.md §4.1" comment (→ README).
- `CLAUDE.md`: drive line without model/letter ("ask before any real burn").

Delete: `bin/dvdauthor-COPYING.txt`.

## Steps
1. File changes on `native-authoring`, run the checks below, one commit.
2. `git checkout main && git merge --ff-only native-authoring && git push`; delete the three merged
   branches locally and on `origin`.
3. Backup: `git clone --mirror https://github.com/islmal/convertx-rust.git C:\convertx-rust-backup.git`.
4. `pip install git-filter-repo`; in a fresh mirror clone (scratch dir):
   `git filter-repo --invert-paths --path-glob 'bin/*.exe' --mailmap <file>` with the mailmap line
   `islmal <136852714+islmal@users.noreply.github.com> malik <old e-mail>`. Tag
   `v0.1.0` is rewritten with it.
5. Run the history gate (below). **Ask the user for an explicit go**, then
   `git push --force --mirror` to origin (filter-repo removes the remote – re-add it).
6. Local checkout: back up `bin/ffmpeg.exe`/`ffprobe.exe`, then `git fetch origin` and
   `git reset --hard origin/<branch>` per branch (or re-clone and copy ffmpeg back). Set
   `git config user.email 136852714+islmal@users.noreply.github.com` in the repo.
7. User: Settings → Visibility → Public; description/topics; optionally GitHub e-mail settings
   "Keep my email private" + "Block command line pushes that expose my email".

## Verification
Before the commit (step 1):
- `cargo test --workspace` (regression), `cargo check -p convertx-gui` (tauri-build checks the
  resource paths), `npm run build`.

History gate in the rewritten mirror (step 5, must all pass before pushing):
- `git log --all --format='%ae%n%ce' | sort -u` contains no `outlook.com`.
- `git rev-list --objects --all | git cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' | grep -i '\.exe$'` is empty.
- `git log --all -p | grep -c <old-local-part>` is 0 (blob contents too).
- `git tag` still lists `v0.1.0`; `git log --oneline main | wc -l` equals the pre-rewrite count.

After the push:
- `gh api 'repos/islmal/convertx-rust/commits?per_page=100' --jq '.[].commit.author.email' | sort -u`
  shows only noreply/passmail addresses.
