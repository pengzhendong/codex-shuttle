[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\codex-shuttle\bin")
)

$ErrorActionPreference = "Stop"
$Repository = "pengzhendong/codex-shuttle"
$ShuttleVersion = "0.2.0"
$Asset = "cxs-cli-windows-x86_64.exe"

function Find-CodexBinary {
    if ($env:CXS_CODEX_PATH) {
        return $env:CXS_CODEX_PATH
    }

    $root = Join-Path $env:LOCALAPPDATA "OpenAI\Codex\bin"
    $candidates = @()
    $direct = Join-Path $root "codex.exe"
    if (Test-Path -LiteralPath $direct -PathType Leaf) {
        $candidates += Get-Item -LiteralPath $direct
    }
    if (Test-Path -LiteralPath $root -PathType Container) {
        $candidates += Get-ChildItem -LiteralPath $root -Directory | ForEach-Object {
            $candidate = Join-Path $_.FullName "codex.exe"
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                Get-Item -LiteralPath $candidate
            }
        }
    }
    $selected = $candidates | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
    if (-not $selected) {
        throw "Codex Desktop codex.exe was not found under $root"
    }
    return $selected.FullName
}

$codex = Find-CodexBinary
$versionText = & $codex --version 2>$null
$match = [regex]::Match(($versionText -join " "), '\d+\.\d+\.\d+')
if (-not $match.Success) {
    throw "Could not detect the Codex Desktop version from $codex"
}
$codexVersion = $match.Value
$releaseTag = "v$ShuttleVersion-codex.$codexVersion"
$releaseUrl = "https://github.com/$Repository/releases/download/$releaseTag"
Write-Host "Detected Codex Desktop $codexVersion."

$workDir = Join-Path ([IO.Path]::GetTempPath()) ("cxs-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $workDir | Out-Null
try {
    $assetPath = Join-Path $workDir $Asset
    $checksumsPath = Join-Path $workDir "SHA256SUMS"
    Write-Host "Downloading $Asset..."
    Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl/$Asset" -OutFile $assetPath
    Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl/SHA256SUMS" -OutFile $checksumsPath

    $checksumLine = Get-Content -LiteralPath $checksumsPath | Where-Object {
        $_ -match ("\s\*?" + [regex]::Escape($Asset) + "$")
    } | Select-Object -First 1
    if (-not $checksumLine) {
        throw "$Asset is missing from SHA256SUMS"
    }
    $expected = ($checksumLine -split '\s+')[0].ToUpperInvariant()
    $actual = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash
    if ($actual -ne $expected) {
        throw "SHA-256 checksum mismatch"
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $destination = Join-Path $InstallDir "cxs.exe"
    Copy-Item -LiteralPath $assetPath -Destination $destination -Force
    Unblock-File -LiteralPath $destination
    Write-Host "Installed cxs to $destination"
    if (($env:Path -split ';') -notcontains $InstallDir) {
        Write-Host "Add it to PATH with:"
        Write-Host "  `$env:Path = `"$InstallDir;`$env:Path`""
    }
    Write-Host "Run cxs --version to verify the installation."
}
finally {
    Remove-Item -LiteralPath $workDir -Recurse -Force -ErrorAction SilentlyContinue
}
