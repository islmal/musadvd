# MusaDVD handbook

All handbooks and reference documentation live in this folder. Project rules for contributors and
agents are in [`../CLAUDE.md`](../CLAUDE.md); the architecture summary and open issues are in
[`../STATUS.md`](../STATUS.md); the generated file tree is `../SYSTEM_STATE.txt`.

## Developer documentation (English, Markdown)

| Document | Contents |
|---|---|
| [architecture.md](architecture.md) | Pipelines (DVD, Blu-ray/AVCHD, burn), data flow, design principles, licensing of derived code |
| [modules-engine.md](modules-engine.md) | Every module of the engine library `src/` (authoring, muxing, menus, subtitles) |
| [modules-burner.md](modules-burner.md) | Every module of `src/burner/` (image writers, burn engines, Burn Studio logic) |
| [modules-gui.md](modules-gui.md) | Tauri backend (`src-tauri/`) and TypeScript frontend (`frontend/`), windows, commands, events |
| [cli.md](cli.md) | The `musadvd` command-line interface |
| [settings-i18n.md](settings-i18n.md) | Settings file, locale files and how to add a language |
| [testing.md](testing.md) | Test suites, fixtures, oracles, scripts and CI |
| [menu-editor-technical.md](menu-editor-technical.md) | Menu editor, timeline, layout JSON model and menu rendering pipeline (technical reference) |

## User handbooks (German)

These are end-user guides for the menu editor and are written in German (the GUI default language).

| Document | Format |
|---|---|
| [Menu editor handbook](menu-editor-handbook.html) / [PDF](Menue-Editor-Handbuch.pdf) | HTML, PDF |
| [Timeline guide](timeline-guide.html) / [PDF](Menue-Editor-Timeline-Anleitung.pdf) | HTML, PDF |
| [Technical reference, PDF edition](Menue-Editor-Technical-Reference.pdf) | PDF (print of an earlier `menu-editor-technical.md`) |

## Plans

Design plans of finished and planned work are kept in [`../docs/plans/`](../docs/plans/) (finished
ones in `archive/`). They are history, not reference: when a plan and the code disagree, the code wins.

## Writing rules

- All Markdown files (handbook, STATUS.md, plans, READMEs) are written in **English**. The German
  user handbooks above are the only deliberate exception; they are end-user material.
- Refer to code by function or file name, never by line number.
- Facts a script can produce (file tree, line counts, dependencies) do not belong in prose.
