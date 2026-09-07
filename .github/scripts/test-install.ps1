$ErrorActionPreference = "Stop"

$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$Temporary = Join-Path ([IO.Path]::GetTempPath()) ("cxs-installer-test-" + [guid]::NewGuid())
$InstallDir = Join-Path $Temporary "install"
$Codex = Join-Path $Temporary "codex.cmd"
New-Item -ItemType Directory -Path $Temporary | Out-Null
Set-Content -LiteralPath $Codex -Value '@echo codex-cli 1.2.3-alpha.4'
$env:CXS_CODEX_PATH = $Codex

function global:Invoke-WebRequest {
    param(
        [switch]$UseBasicParsing,
        [string]$Uri,
        [string]$OutFile
    )
    if ($Uri.EndsWith('/releases.atom')) {
        return [pscustomobject]@{ Content = '<feed><id>v9.8.7-codex.1.2.3</id></feed>' }
    }
    if ($Uri.EndsWith('/v9.8.7-codex.1.2.3/cxs-cli-windows-x86_64.exe')) {
        Set-Content -LiteralPath $OutFile -Value 'fake cxs binary' -NoNewline
        return
    }
    if ($Uri.EndsWith('/v9.8.7-codex.1.2.3/SHA256SUMS')) {
        $Asset = Join-Path (Split-Path $OutFile) 'cxs-cli-windows-x86_64.exe'
        $Hash = (Get-FileHash -LiteralPath $Asset -Algorithm SHA256).Hash
        Set-Content -LiteralPath $OutFile -Value "$Hash  cxs-cli-windows-x86_64.exe"
        return
    }
    throw "Unexpected URL: $Uri"
}

try {
    $Output = & (Join-Path $Root 'install.ps1') -InstallDir $InstallDir | Out-String
    if (-not $Output.Contains('Selected Shuttle release v9.8.7-codex.1.2.3.')) {
        throw "Installer did not select the matching release"
    }
    $Installed = Join-Path $InstallDir 'cxs.exe'
    if ((Get-Content -LiteralPath $Installed -Raw) -ne 'fake cxs binary') {
        throw "Installer wrote unexpected asset contents"
    }
}
finally {
    Remove-Item function:global:Invoke-WebRequest -ErrorAction SilentlyContinue
    Remove-Item Env:CXS_CODEX_PATH -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $Temporary -Recurse -Force -ErrorAction SilentlyContinue
}
