# Build frontend
Write-Host "==> Building frontend..."
npm run build
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Build debug binaries
Write-Host "==> Building debug binaries..."
cargo build --bin convertx-rust
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo build -p convertx-gui
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Build release binaries
Write-Host "==> Building release binaries..."
cargo build --release --bin convertx-rust
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo build --release -p convertx-gui
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Update shortcuts
Write-Host "==> Updating shortcuts..."
& (Join-Path $PSScriptRoot "update_shortcuts.ps1")

Write-Host "==> Build complete and shortcuts updated!"
