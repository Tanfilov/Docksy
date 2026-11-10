# Builds Docksy (release) and its installers, one per architecture:
#   installer\Output\Docksy-Setup-<version>-arm64.exe
#   installer\Output\Docksy-Setup-<version>-x64.exe
# .\build-installer.ps1 -Arch arm64   builds just one of them.
param([ValidateSet("arm64", "x64", "all")][string]$Arch = "all")
Set-Location $PSScriptRoot

$targets = @{ arm64 = "aarch64-pc-windows-msvc"; x64 = "x86_64-pc-windows-msvc" }
$archs = if ($Arch -eq "all") { @("arm64", "x64") } else { @($Arch) }

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found (winget install JRSoftware.InnoSetup)" }

foreach ($a in $archs) {
    # (through cmd: Windows PowerShell treats cargo's progress output on stderr as errors)
    cmd /c "cargo build --release --target $($targets[$a]) 2>&1"
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($a)" }
    & $iscc /Q "/DArch=$a" "installer\docksy.iss"
    if ($LASTEXITCODE -ne 0) { throw "installer build failed ($a)" }
}
Get-ChildItem installer\Output\*.exe | Select-Object Name, Length, LastWriteTime
