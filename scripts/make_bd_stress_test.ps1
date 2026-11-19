<#
.SYNOPSIS
Generates a self-contained Blu-ray hardware stress-test kit for MusaDVD.

.DESCRIPTION
Creates three synthetic Matroska test clips and a validated 1920x1080 CustomLayout JSON
in the output folder (default: .\bd_stress_test):
  1. 01_reencode_pgs_chapters.mkv (~45s, 1080p23.976):
     - Long GOP (keyint=250) forces scan_video fallback and full re-encoding under --bd-video auto.
     - Two 48 kHz AC-3 tracks (5.1 English 440 Hz + 2.0 German 660 Hz) for Audio Settings tests.
     - Multi-cue embedded SRT subtitle stream (forces src/pgs.rs text-to-PGS rendering & muxing).
     - Three chapter markers (0s, 15s, 30s) via ffmetadata for Chapter submenu & chapter-skip tests.
  2. 02_copy_timing_repair.mkv (~45s, 1080p23.976):
     - H.264 High@4.1 yuv420p, -bluray-compat 1, keyint=24:min-keyint=24:nal-hrd=none, ~6 Mbps + AC-3.
     - Accepted by encoder::bd_video_copy and bd_mux::scan_video as a stream-copy candidate.
     - Missing HRD/SEI forces bd_mux::TimingRepair (src/h264.rs + src/bd_mux.rs) to inject SPS VUI/HRD
       and buffering-period / picture-timing SEI packets.
  3. 03_secondary_720p5994.mkv (~30s, 720p59.94):
     - H.264 High@4.1 yuv420p, -bluray-compat 1, keyint=60:min-keyint=60:nal-hrd=none, ~4 Mbps + AC-3.
     - Tests per-title CLPI/MPLS format switching (src/bd_clip.rs) on the same disc:
       * CLI (musadvd build-dvd --disc bd25 --tv ntsc --bd-video auto): build-dvd has one global
         --resolution flag (default hd1080), so Clips 1 & 2 author at 1080p23.976 while Clip 3
         re-encodes to 1080i29.97 (fake-interlaced), still verifying per-title rate/scan switching.
       * GUI (MusaDVD Video tab): set a per-title override on Clip 3 to 720p / 59.94 fps so Clip 3
         stays native 720p59.94 (stream-copied with TimingRepair under auto) alongside 1080p23.976 titles.
  4. stress_layout.json:
     - 1920x1080 CustomLayout (src/layout.rs, passes layout::validate_bd) with:
       * Main Title Menu (role: "title") with 2.0s motion intro keyframes (introSecs + intro),
         Play Title 1-3, Play All, and submenu links.
       * Chapter Submenu (role: "root", title: 1) for Clip 1's 3 chapters.
       * Audio/Subtitle Settings Submenu (role: "settings", title: 1) with exact ffprobe stream
         indices matching Clip 1 (audio: 1, 2; subtitle: 3 and Off).

.PARAMETER OutDir
Destination folder for the generated test kit. Defaults to .\bd_stress_test.
#>
[CmdletBinding()]
param(
    [string]$OutDir = ".\bd_stress_test"
)

$ErrorActionPreference = "Stop"

# Locate ffmpeg / ffprobe: prefer repo-local bin/ffmpeg.exe, otherwise fall back to PATH.
$repoRoot = Split-Path -Parent $PSScriptRoot
$localFfmpeg = Join-Path $repoRoot "bin\ffmpeg.exe"
$localFfprobe = Join-Path $repoRoot "bin\ffprobe.exe"

if (Test-Path -LiteralPath $localFfmpeg) {
    $ffmpeg = $localFfmpeg
} else {
    $cmd = Get-Command ffmpeg -ErrorAction SilentlyContinue
    if (-not $cmd) {
        throw "ffmpeg not found in '$localFfmpeg' or on PATH."
    }
    $ffmpeg = $cmd.Source
}

if (Test-Path -LiteralPath $localFfprobe) {
    $ffprobe = $localFfprobe
} else {
    $cmd = Get-Command ffprobe -ErrorAction SilentlyContinue
    $ffprobe = if ($cmd) { $cmd.Source } else { $null }
}

if (-not (Test-Path -LiteralPath $OutDir)) {
    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
}
$outPath = (Resolve-Path -LiteralPath $OutDir).Path

function Write-Utf8NoBom {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Text
    )
    [System.IO.File]::WriteAllText($Path, $Text, [System.Text.UTF8Encoding]::new($false))
}

function Invoke-Ffmpeg {
    param([Parameter(Mandatory)][string[]]$ArgsList)
    & $ffmpeg -hide_banner -loglevel error -y @ArgsList
    if ($LASTEXITCODE -ne 0) {
        throw "ffmpeg failed with exit code $LASTEXITCODE."
    }
}

Write-Host "==> Using ffmpeg: $ffmpeg"
Write-Host "==> Output folder: $outPath"

# ---------------------------------------------------------------------------
# Clip 1: 01_reencode_pgs_chapters.mkv (~45s, 1080p23.976, forced re-encode)
# ---------------------------------------------------------------------------
$clip1 = Join-Path $outPath "01_reencode_pgs_chapters.mkv"
$srtPath = Join-Path $outPath "_tmp_01_subs.srt"
$chapPath = Join-Path $outPath "_tmp_01_chapters.txt"

$srtContent = @"
1
00:00:02,000 --> 00:00:07,000
Chapter 1: Full Re-encode & PGS Test
Subtitle Cue 1 (00:02 - 00:07)

2
00:00:09,000 --> 00:00:14,000
Check 1080p PGS crispness & safe area
No flicker, clipping, or desync

3
00:00:17,000 --> 00:00:22,000
Chapter 2: Audio & Subtitle Settings
Audio 1 = 440 Hz 5.1 | Audio 2 = 660 Hz 2.0

4
00:00:24,000 --> 00:00:29,000
Toggle Subtitles OFF / ON via remote
or the Title 1 Settings submenu

5
00:00:32,000 --> 00:00:37,000
Chapter 3: Seek & Top Menu Return
Test FF/RW, Chapter Skip, and Top Menu

6
00:00:39,000 --> 00:00:43,500
End of Clip 1 (45s)
Next: Clip 2 (Stream Copy + TimingRepair)
"@

$chapContent = @"
;FFMETADATA1
[CHAPTER]
TIMEBASE=1/1000
START=0
END=15000
title=Chapter 1: Intro & Re-encode

[CHAPTER]
TIMEBASE=1/1000
START=15000
END=30000
title=Chapter 2: PGS & Audio Switch

[CHAPTER]
TIMEBASE=1/1000
START=30000
END=45000
title=Chapter 3: Finale & Seek
"@

Write-Utf8NoBom -Path $srtPath -Text $srtContent
Write-Utf8NoBom -Path $chapPath -Text $chapContent

Write-Host "==> Generating Clip 1: 01_reencode_pgs_chapters.mkv (45s, 1080p23.976, keyint=250, 2x AC-3, SRT, 3 chapters)..."
# Stream indices are deterministic via -map: 0=video, 1=audio(eng 5.1), 2=audio(deu 2.0), 3=subtitle(eng srt).
Invoke-Ffmpeg @(
    "-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=24000/1001:d=45",
    "-f", "lavfi", "-i", "sine=f=440:d=45:sample_rate=48000,aformat=channel_layouts=5.1",
    "-f", "lavfi", "-i", "sine=f=660:d=45:sample_rate=48000,aformat=channel_layouts=stereo",
    "-i", $srtPath,
    "-f", "ffmetadata", "-i", $chapPath,
    "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:s", "-map_chapters", "4",
    "-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-level:v", "4.1",
    "-pix_fmt", "yuv420p", "-x264-params", "keyint=250:min-keyint=250:scenecut=0", "-b:v", "4M",
    "-c:a", "ac3", "-b:a:0", "384k", "-b:a:1", "192k",
    "-c:s", "srt",
    "-metadata:s:a:0", "language=eng", "-metadata:s:a:0", "title=English 5.1 (440 Hz)",
    "-metadata:s:a:1", "language=deu", "-metadata:s:a:1", "title=Deutsch 2.0 (660 Hz)",
    "-metadata:s:s:0", "language=eng", "-metadata:s:s:0", "title=English PGS Test",
    $clip1
)

Remove-Item -LiteralPath $srtPath, $chapPath -Force -ErrorAction SilentlyContinue

# ---------------------------------------------------------------------------
# Clip 2: 02_copy_timing_repair.mkv (~45s, 1080p23.976, stream copy + TimingRepair)
# ---------------------------------------------------------------------------
$clip2 = Join-Path $outPath "02_copy_timing_repair.mkv"
Write-Host "==> Generating Clip 2: 02_copy_timing_repair.mkv (45s, 1080p23.976, High@4.1, keyint=24, nal-hrd=none, ~6 Mbps)..."
Invoke-Ffmpeg @(
    "-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=24000/1001:d=45",
    "-f", "lavfi", "-i", "sine=f=523:d=45:sample_rate=48000,aformat=channel_layouts=stereo",
    "-map", "0:v", "-map", "1:a",
    "-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-level:v", "4.1",
    "-pix_fmt", "yuv420p", "-bluray-compat", "1",
    "-x264-params", "keyint=24:min-keyint=24:nal-hrd=none",
    "-b:v", "6M",
    "-c:a", "ac3", "-b:a", "192k",
    "-metadata:s:a:0", "language=eng", "-metadata:s:a:0", "title=English 2.0 (523 Hz)",
    $clip2
)

# ---------------------------------------------------------------------------
# Clip 3: 03_secondary_720p5994.mkv (~30s, 720p59.94, per-title CLPI/MPLS switch)
# ---------------------------------------------------------------------------
$clip3 = Join-Path $outPath "03_secondary_720p5994.mkv"
Write-Host "==> Generating Clip 3: 03_secondary_720p5994.mkv (30s, 720p59.94, High@4.1, keyint=60, nal-hrd=none, ~4 Mbps)..."
Invoke-Ffmpeg @(
    "-f", "lavfi", "-i", "testsrc2=s=1280x720:r=60000/1001:d=30",
    "-f", "lavfi", "-i", "sine=f=330:d=30:sample_rate=48000,aformat=channel_layouts=stereo",
    "-map", "0:v", "-map", "1:a",
    "-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-level:v", "4.1",
    "-pix_fmt", "yuv420p", "-bluray-compat", "1",
    "-x264-params", "keyint=60:min-keyint=60:nal-hrd=none",
    "-b:v", "4M",
    "-c:a", "ac3", "-b:a", "192k",
    "-metadata:s:a:0", "language=eng", "-metadata:s:a:0", "title=English 2.0 (330 Hz)",
    $clip3
)

# ---------------------------------------------------------------------------
# CustomLayout JSON: stress_layout.json (1920x1080, passes layout::validate_bd)
# ---------------------------------------------------------------------------
$layoutPath = Join-Path $outPath "stress_layout.json"
Write-Host "==> Writing CustomLayout: stress_layout.json..."

$layoutJson = @'
{
  "version": 1,
  "canvas": {
    "width": 1920,
    "height": 1080
  },
  "highlight": {
    "color": "#FFD200",
    "selectColor": "#FF6A00",
    "opacity": 0.85
  },
  "animations": true,
  "menus": [
    {
      "id": "main",
      "name": "Main Title Menu",
      "role": "title",
      "title": null,
      "background": {
        "color": "#0F1B2E",
        "blur": 0.0,
        "gradientOverlay": "none"
      },
      "durationSecs": 10.0,
      "audio": {
        "type": "none"
      },
      "introSecs": 2.0,
      "elements": [
        {
          "id": "main_heading",
          "type": "text",
          "text": "MusaDVD Blu-ray Hardware Stress Test",
          "font": "Arial",
          "size": 62.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": true,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 120.0,
          "y": 70.0,
          "w": 1680.0,
          "h": 90.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": null,
          "intro": [
            { "t": 0.0, "x": 120.0, "y": 30.0, "w": 1680.0, "h": 90.0, "opacity": 0.0 },
            { "t": 1.2, "x": 120.0, "y": 70.0, "w": 1680.0, "h": 90.0, "opacity": 1.0 }
          ]
        },
        {
          "id": "main_subtitle",
          "type": "text",
          "text": "IGS Navigation | PGS Subtitles | TimingRepair Copy | Per-Title CLPI/MPLS",
          "font": "Arial",
          "size": 30.0,
          "color": "#A9C4E4",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 120.0,
          "y": 170.0,
          "w": 1680.0,
          "h": 50.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": null,
          "intro": [
            { "t": 0.2, "x": 120.0, "y": 170.0, "w": 1680.0, "h": 50.0, "opacity": 0.0 },
            { "t": 1.4, "x": 120.0, "y": 170.0, "w": 1680.0, "h": 50.0, "opacity": 1.0 }
          ]
        },
        {
          "id": "btn_title1",
          "type": "text",
          "text": "1. Title 1: Re-encode + PGS + Chapters (1080p23.976)",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 280.0,
          "w": 1350.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "playTitle", "title": 1 },
          "intro": [
            { "t": 0.0, "x": 60.0, "y": 280.0, "w": 1350.0, "h": 80.0, "opacity": 0.0 },
            { "t": 1.5, "x": 160.0, "y": 280.0, "w": 1350.0, "h": 80.0, "opacity": 1.0 }
          ]
        },
        {
          "id": "btn_title2",
          "type": "text",
          "text": "2. Title 2: Stream Copy + TimingRepair (1080p23.976)",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 390.0,
          "w": 1350.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "playTitle", "title": 2 },
          "intro": [
            { "t": 0.1, "x": 60.0, "y": 390.0, "w": 1350.0, "h": 80.0, "opacity": 0.0 },
            { "t": 1.6, "x": 160.0, "y": 390.0, "w": 1350.0, "h": 80.0, "opacity": 1.0 }
          ]
        },
        {
          "id": "btn_title3",
          "type": "text",
          "text": "3. Title 3: Secondary Format (720p59.94 / 1080i29.97)",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 500.0,
          "w": 1350.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "playTitle", "title": 3 },
          "intro": [
            { "t": 0.2, "x": 60.0, "y": 500.0, "w": 1350.0, "h": 80.0, "opacity": 0.0 },
            { "t": 1.7, "x": 160.0, "y": 500.0, "w": 1350.0, "h": 80.0, "opacity": 1.0 }
          ]
        },
        {
          "id": "btn_play_all",
          "type": "text",
          "text": "Play All Titles",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": true,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 640.0,
          "w": 700.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "playAll" },
          "intro": [
            { "t": 0.3, "x": 60.0, "y": 640.0, "w": 700.0, "h": 80.0, "opacity": 0.0 },
            { "t": 1.8, "x": 160.0, "y": 640.0, "w": 700.0, "h": 80.0, "opacity": 1.0 }
          ]
        },
        {
          "id": "btn_chapters",
          "type": "text",
          "text": "Title 1 Chapters",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 750.0,
          "w": 700.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "menu", "menu": "chapters_t1" },
          "intro": [
            { "t": 0.4, "x": 60.0, "y": 750.0, "w": 700.0, "h": 80.0, "opacity": 0.0 },
            { "t": 1.9, "x": 160.0, "y": 750.0, "w": 700.0, "h": 80.0, "opacity": 1.0 }
          ]
        },
        {
          "id": "btn_settings",
          "type": "text",
          "text": "Title 1 Audio & Subtitles",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 860.0,
          "w": 700.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "menu", "menu": "settings_t1" },
          "intro": [
            { "t": 0.5, "x": 60.0, "y": 860.0, "w": 700.0, "h": 80.0, "opacity": 0.0 },
            { "t": 2.0, "x": 160.0, "y": 860.0, "w": 700.0, "h": 80.0, "opacity": 1.0 }
          ]
        }
      ]
    },
    {
      "id": "chapters_t1",
      "name": "Title 1 Chapters",
      "role": "root",
      "title": 1,
      "background": {
        "color": "#132238",
        "blur": 0.0,
        "gradientOverlay": "none"
      },
      "durationSecs": null,
      "audio": {
        "type": "none"
      },
      "elements": [
        {
          "id": "chap_heading",
          "type": "text",
          "text": "Title 1 - Chapters",
          "font": "Arial",
          "size": 60.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": true,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 120.0,
          "y": 80.0,
          "w": 1680.0,
          "h": 90.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": null
        },
        {
          "id": "btn_ch1",
          "type": "text",
          "text": "Chapter 1 (00:00) - Intro & Re-encode",
          "font": "Arial",
          "size": 46.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 240.0,
          "w": 1200.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "playChapter", "title": 1, "chapter": 1 }
        },
        {
          "id": "btn_ch2",
          "type": "text",
          "text": "Chapter 2 (00:15) - PGS & Audio Switch",
          "font": "Arial",
          "size": 46.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 360.0,
          "w": 1200.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "playChapter", "title": 1, "chapter": 2 }
        },
        {
          "id": "btn_ch3",
          "type": "text",
          "text": "Chapter 3 (00:30) - Finale & Seek",
          "font": "Arial",
          "size": 46.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 480.0,
          "w": 1200.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "playChapter", "title": 1, "chapter": 3 }
        },
        {
          "id": "btn_ch_settings",
          "type": "text",
          "text": "Audio & Subtitles",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 660.0,
          "w": 700.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "menu", "menu": "settings_t1" }
        },
        {
          "id": "btn_ch_back",
          "type": "text",
          "text": "Back to Main Menu",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 780.0,
          "w": 700.0,
          "h": 80.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "menu", "menu": "main" }
        }
      ]
    },
    {
      "id": "settings_t1",
      "name": "Title 1 Audio & Subtitles",
      "role": "settings",
      "title": 1,
      "background": {
        "color": "#182940",
        "blur": 0.0,
        "gradientOverlay": "none"
      },
      "durationSecs": null,
      "audio": {
        "type": "none"
      },
      "elements": [
        {
          "id": "set_heading",
          "type": "text",
          "text": "Title 1 - Audio & Subtitles",
          "font": "Arial",
          "size": 60.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": true,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 120.0,
          "y": 80.0,
          "w": 1680.0,
          "h": 90.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": null
        },
        {
          "id": "btn_audio1",
          "type": "text",
          "text": "Audio 1: English 5.1 (440 Hz)",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 230.0,
          "w": 1000.0,
          "h": 75.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "setAudio", "source": 1 }
        },
        {
          "id": "btn_audio2",
          "type": "text",
          "text": "Audio 2: Deutsch 2.0 (660 Hz)",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 330.0,
          "w": 1000.0,
          "h": 75.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "setAudio", "source": 2 }
        },
        {
          "id": "btn_sub_on",
          "type": "text",
          "text": "Subtitles: English PGS ON",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 450.0,
          "w": 1000.0,
          "h": 75.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "setSubtitle", "source": 3 }
        },
        {
          "id": "btn_sub_off",
          "type": "text",
          "text": "Subtitles: OFF",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 550.0,
          "w": 1000.0,
          "h": 75.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "setSubtitle", "source": null }
        },
        {
          "id": "btn_set_play",
          "type": "text",
          "text": "Play Title 1",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": true,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 690.0,
          "w": 650.0,
          "h": 75.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "playTitle", "title": 1 }
        },
        {
          "id": "btn_set_chapters",
          "type": "text",
          "text": "Back to Chapters",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 790.0,
          "w": 650.0,
          "h": 75.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "menu", "menu": "chapters_t1" }
        },
        {
          "id": "btn_set_main",
          "type": "text",
          "text": "Back to Main Menu",
          "font": "Arial",
          "size": 44.0,
          "color": "#FFFFFF",
          "align": "left",
          "bold": false,
          "italic": false,
          "shadow": true,
          "wrap": false,
          "x": 160.0,
          "y": 890.0,
          "w": 650.0,
          "h": 75.0,
          "rotation": 0,
          "opacity": 1.0,
          "action": { "type": "menu", "menu": "main" }
        }
      ]
    }
  ]
}
'@

Write-Utf8NoBom -Path $layoutPath -Text $layoutJson

# Optional ffprobe sanity summary if ffprobe is available
if ($ffprobe) {
    Write-Host ""
    Write-Host "==> Verifying Clip 1 stream indices with ffprobe:"
    & $ffprobe -v error -show_entries "stream=index,codec_type,codec_name,channels:stream_tags=language" -of "compact=p=0" $clip1
}

Write-Host ""
Write-Host "==> Blu-ray stress-test kit ready in: $outPath"
Write-Host "    - $clip1"
Write-Host "    - $clip2"
Write-Host "    - $clip3"
Write-Host "    - $layoutPath"
Write-Host ""
Write-Host "Next steps (CLI):"
Write-Host "  1. Build BDMV + CERTIFICATE (Clip 1 re-encodes, Clip 2 copies + TimingRepair, Clip 3 -> 1080i29.97 under --tv ntsc):"
Write-Host "     musadvd build-dvd --disc bd25 --tv ntsc --bd-video auto --no-hw-encode --project-name BD_STRESS --layout `"$layoutPath`" --output `"$outPath\out`" --input `"$clip1`" `"$clip2`" `"$clip3`""
Write-Host "     (Tip: To keep Clip 3 at native 720p59.94 on the same disc, open these clips in the MusaDVD GUI and set Title 3's Video tab override to 720p / 59.94 fps.)"
Write-Host "  2. Build UDF 2.50 ISO:"
Write-Host "     musadvd build-iso `"$outPath\out\BD_STRESS`" --as-disc --label BD_STRESS --output `"$outPath\BD_STRESS.iso`""
