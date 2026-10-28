# DVD titles: closed GOPs + continuous audio PTS

Date: 2026-10-28. Goal: a DVD build with chapters and re-encoded audio leaves no
`GOP may not be closed`, `Discontinuity` or `moves backwards` warning in `authoring.log`.

## Findings (reproduced with the CLI, 2 sources, chapters every 30 s, `--loudnorm`)

- `WARN: GOP may not be closed on cell 2/3 …` in **every** multi-chapter build:
  `EncodeJob::video_codec_args` (`src/encoder.rs`) only sets `-g 12 -bf 2`; no `-flags +cgop`,
  no `-sc_threshold`, so B-frames straddle GOP boundaries and `dvd_mux::mark_chapters` sees
  `firstifield != 0`. Only the DVD menu intro path (`-force_key_frames … -flags +cgop
  -sc_threshold 1000000000`) has the flags.
- `WARN: Discontinuity of 7894 in audio channel 0`: MP4 source, AAC 44.1 kHz, audio offset 12.3 ms,
  `--loudnorm`. ffprobe shows a real 88 ms PTS gap at 87.6 s in `title_02.mpg`. Cause: the chain
  `loudnorm…,aresample=48000` (and the implicit `aresample` of unfiltered re-encodes via `-ar`) has
  no `async`, so irregular timestamps reach the AC-3 encoder unchanged. Small values ("2") are the
  same mechanism.
  Checked directly with ffmpeg: `aresample=48000:async=1:min_hard_comp=0.01` removes the gap
  (fills silence, keeps A/V sync). `asetpts=N/SR/TB` also removes it but shifts the remaining audio
  → rejected.
- LPCM → AC-3 on DVD is already correct (`plan_audio`: `Lpcm`+`Dvd` → AC-3; PCM sources never hit
  `copy_plan`); covered by unit test `lpcm_plans`.
- Stream-copied AC-3/DTS is left alone: MKV/MP4/TS give frame-exact 2880-tick stamps; jitter there
  is a source defect, and regenerating PTS (`setts`) would hide real gaps → A/V drift. `dvd_mux`
  re-anchors the PTS anyway.

## Files

- `src/encoder.rs` (fix + unit tests)
- `tests/dvd_audio.rs` (new integration test)

## Steps

1. **Integration test first (must fail on the current code)** in `tests/dvd_audio.rs`:
   `authoring_log_is_clean_with_chapters_and_loudnorm`.
   - Source via ffmpeg: 90 s, `testsrc2=size=720x576:rate=25`, audio
     `sine=f=440:sample_rate=44100` with `-itsoffset 0.0123` before the audio input, `-c:v libx264
     -preset ultrafast -c:a aac`, `.mp4`.
   - Build with loudnorm on, chapters every 30 s (`ChapterSettings { interval_secs: 30.0, .. }`),
     `fast_encode`, `keep_work: true`.
   - Read `authoring.log` from `report.work_dir` (like `tests/menu_intro.rs` ~line 789) and assert
     it contains none of `GOP may not be closed`, `Discontinuity`, `moves backwards` (print the log
     on failure). Skip gracefully when tools/templates are missing (existing `setup()`).
   - Run it once before the fix and confirm it fails (both warnings expected).
2. `EncodeJob::video_codec_args`: append `-flags +cgop -sc_threshold 1000000000` (ffmpeg's
   mpeg2video refuses `+cgop` with scene-change detection). Shared by pass 1 and pass 2, so both
   stay identical.
3. `EncodeJob::encode_args`: constant
   `DVD_AUDIO_SYNC = "aresample=48000:async=1:min_hard_comp=0.01"`; every re-encoded track gets
   `"{loudnorm},{DVD_AUDIO_SYNC}"` or `DVD_AUDIO_SYNC` alone as its filter. `audio_track_args`
   already skips the filter for copies. BD stays unchanged (`bd_mux` regenerates audio PTS from
   sample counts). One-line comment explaining why (dvd_mux checks AC-3 PTS on the 2880 grid).
4. Unit tests in `src/encoder.rs`:
   - update expected strings (DVD encode args test asserting
     `-map 0:1 -map 0:2 -c:a:0 ac3 …`, and the loudnorm test asserting
     `-filter:a:0 loudnorm=I=-23,aresample=48000`);
   - new assertions: `-flags +cgop -sc_threshold 1000000000` in both `first_pass_args` and
     `encode_args`; `async=1` only on re-encoded tracks, never next to `-c:a:N copy`.

## Verification

- New integration test red before, green after the fix.
- `cargo test --workspace` fully green, esp. `dvdauthor_native` (byte identity; fixtures are
  recorded, independent of the encoder), `dvd_audio`, `menu_intro`, `motion_menu`, `two_pass`,
  `build_dvd`, `ntsc`.
- `cargo check -p convertx-gui`, `cargo check --lib --tests --target x86_64-unknown-linux-gnu`.

## Out of scope / risks

- Exact "Discontinuity of 2" with the user's original source not reproduced (same mechanism shown
  with 88 ms).
- Closed GOPs cost a little bitrate efficiency; chapters still snap to the nearest GOP (≤ 0.48 s);
  frame-exact chapter keyframes (`-force_key_frames`) deliberately skipped.
