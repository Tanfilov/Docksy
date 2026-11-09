# Builds Docksy (release, ARM64) and its installer:
#   installer\Output\Docksy-Setup-<version>-arm64.exe
Set-Location $PSScriptRoot

# (through cmd: Windows PowerShell treats cargo's progress output on stderr as errors)
cmd /c "cargo build --release 2>&1"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found (winget install JRSoftware.InnoSetup)" }

& $iscc /Q "installer\docksy.iss"
if ($LASTEXITCODE -ne 0) { throw "installer build failed" }
Get-ChildItem installer\Output\*.exe | Select-Object Name, Length, LastWriteTime
