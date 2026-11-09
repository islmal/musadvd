$sh = New-Object -ComObject WScript.Shell

$destinations = @(
    "C:\Users\malik\Desktop",
    "C:\convertx-rust"
)

foreach ($dest in $destinations) {
    if (Test-Path $dest) {
        # GUI Shortcut
        $scGui = $sh.CreateShortcut((Join-Path $dest "ConvertX GUI.lnk"))
        $scGui.TargetPath = "C:\convertx-rust\target\release\convertx-gui.exe"
        $scGui.WorkingDirectory = "C:\convertx-rust"
        $scGui.IconLocation = "C:\convertx-rust\src-tauri\icons\icon.ico"
        $scGui.Description = "ConvertX GUI (Release)"
        $scGui.Save()

        # CLI Shortcut
        $scCli = $sh.CreateShortcut((Join-Path $dest "ConvertX CLI.lnk"))
        $scCli.TargetPath = "C:\convertx-rust\target\release\convertx-rust.exe"
        $scCli.WorkingDirectory = "C:\convertx-rust"
        $scCli.Description = "ConvertX CLI (Release)"
        $scCli.Save()
    }
}

Write-Host "Updated shortcuts in Desktop and Workspace successfully."
