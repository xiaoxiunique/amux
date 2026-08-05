# amux one-command installer (Windows)
# irm https://amux.cc/install.ps1 | iex

param(
  [switch]$NoInit
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "xiaoxiunique/amux"
$BinDir = "$env:LOCALAPPDATA\amux\bin"

# --- detect architecture ---
$Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
  "AMD64" { "x86_64" }
  "ARM64" { "aarch64" }
  default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}
$Target = "x86_64-pc-windows-msvc"
# TODO: add aarch64 windows binary when GitHub CI builds it

Write-Host "==> fetching latest release…"
$Release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
$Version = $Release.tag_name -replace '^v',''
$Asset = "amux-v${Version}-${Target}.zip"
$Url = "https://github.com/$Repo/releases/download/$($Release.tag_name)/$Asset"

Write-Host "==> downloading amux $Version ($Target)…"
$Tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP "amux-install-$(Get-Random)") -Force
Invoke-WebRequest -Uri $Url -OutFile "$Tmp\amux.zip"

Write-Host "==> installing to ${BinDir}…"
Expand-Archive -Path "$Tmp\amux.zip" -DestinationPath "$Tmp\extract" -Force
New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
Copy-Item "$Tmp\extract\amux.exe" "$BinDir\amux.exe" -Force
Remove-Item -Recurse -Force $Tmp

# --- ensure BinDir is on the user PATH ---
$UserPath = [Environment]::GetEnvironmentVariable("PATH","User")
if ($UserPath -notlike "*$BinDir*") {
  [Environment]::SetEnvironmentVariable(
    "PATH", "$BinDir;$UserPath", "User"
  )
  Write-Host "Added ${BinDir} to user PATH."
  # Make it available in this session too.
  $env:PATH = "$BinDir;$env:PATH"
}

# --- install-cli handles rmux download + shell shims + mux config ---
Write-Host "==> running amux install-cli…"
& "$BinDir\amux.exe" install-cli
if (-not $?) { throw "install-cli failed" }

Write-Host "==> ensuring Claude Code and Codex are installed…"
& "$BinDir\amux.exe" install

Write-Host ""
Write-Host "Done. Open a new terminal and run 'cc' or 'cx' to start an agent."
