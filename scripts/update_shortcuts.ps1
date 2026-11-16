$sh = New-Object -ComObject WScript.Shell

$root = Split-Path -Parent $PSScriptRoot
$destinations = @(
    "C:\Users\malik\Desktop",
    $root
)

foreach ($dest in $destinations) {
    if (Test-Path $dest) {
        # GUI Shortcut
        $scGui = $sh.CreateShortcut((Join-Path $dest "MusaDVD GUI.lnk"))
        $scGui.TargetPath = Join-Path $root "target\release\musadvd-gui.exe"
        $scGui.WorkingDirectory = $root
        $scGui.IconLocation = Join-Path $root "src-tauri\icons\icon.ico"
        $scGui.Description = "MusaDVD GUI (Release)"
        $scGui.Save()

        # CLI Shortcut
        $scCli = $sh.CreateShortcut((Join-Path $dest "MusaDVD CLI.lnk"))
        $scCli.TargetPath = Join-Path $root "target\release\musadvd.exe"
        $scCli.WorkingDirectory = $root
        $scCli.Description = "MusaDVD CLI (Release)"
        $scCli.Save()
    }
}

Write-Host "Updated shortcuts in Desktop and Workspace successfully."
