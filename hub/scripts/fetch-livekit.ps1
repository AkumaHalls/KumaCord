<#
.SYNOPSIS
  Downloads the pinned livekit-server release for Windows, checks it against the SHA256 pinned
  in this script (and, as a second check, the release's checksums.txt) and installs it to
  bin\livekit-server.exe.
.DESCRIPTION
  The expected hashes below were taken from the official v1.13.7 release zip once, and are what
  every later download is compared against: if the release assets were ever replaced (both the
  zip and checksums.txt, by whoever controls the release), the download is refused. checksums.txt
  from the release is still checked, but it is not the trust root.

  Another -Version needs its own -ExpectedZipSha256 (get it from a second, independent source).
  -SourceDir takes the zip and checksums.txt from a local folder instead of GitHub (offline
  installs and the tests).
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File hub\scripts\fetch-livekit.ps1
#>
param(
  [string]$Version = "1.13.7",
  [string]$Dest = (Join-Path $PSScriptRoot "..\..\bin"),
  [string]$ExpectedZipSha256 = "",
  [string]$SourceDir = ""
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# Pinned: livekit_1.13.7_windows_amd64.zip and the livekit-server.exe inside it.
$Pinned = @{
  "1.13.7" = @{
    Zip = "e539e7d2f75807b9c9202cd2a0bf2cb3d52fc4c52978a6953e0f47bc339fe77f"
    Exe = "951f9466cd4450b3c3c7f1a5830a05a9bb97cc11d93cf1be9ebcd3b47cc316d1"
  }
}

$expectedZip = $ExpectedZipSha256.Trim().ToLowerInvariant()
$expectedExe = ""
if ($Pinned.ContainsKey($Version)) {
  if ($expectedZip -and $expectedZip -ne $Pinned[$Version].Zip) {
    throw "-ExpectedZipSha256 differs from the hash pinned in this script for v$Version"
  }
  $expectedZip = $Pinned[$Version].Zip
  $expectedExe = $Pinned[$Version].Exe
}
if ($expectedZip -notmatch '^[0-9a-f]{64}$') {
  throw "No SHA256 pinned for v${Version}: pass -ExpectedZipSha256 (from a source other than the release itself)"
}

$base  = "https://github.com/livekit/livekit/releases/download/v$Version"
$asset = "livekit_${Version}_windows_amd64.zip"
$tmp   = Join-Path ([IO.Path]::GetTempPath()) ("livekit-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
  if ($SourceDir) {
    Write-Host "Using $asset and checksums.txt from $SourceDir"
    Copy-Item (Join-Path $SourceDir $asset) (Join-Path $tmp $asset)
    Copy-Item (Join-Path $SourceDir "checksums.txt") (Join-Path $tmp "checksums.txt")
  } else {
    Write-Host "Downloading $asset and checksums.txt (v$Version)..."
    Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $tmp $asset)
    Invoke-WebRequest "$base/checksums.txt" -OutFile (Join-Path $tmp "checksums.txt")
  }

  # 1. The pinned hash: the trust root.
  $actual = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($actual -ne $expectedZip) { throw "SHA256 of $asset does not match the pinned hash`n pinned $expectedZip`n actual $actual" }

  # 2. The release's own checksums.txt (lines look like: "<sha256>  <file name>").
  $line = Get-Content (Join-Path $tmp "checksums.txt") |
    Where-Object { ($_ -split "\s+")[-1] -eq $asset } | Select-Object -First 1
  if (-not $line) { throw "$asset not listed in checksums.txt" }
  $listed = ($line -split "\s+")[0].ToLowerInvariant()
  if ($listed -ne $actual) { throw "Checksum mismatch for $asset in checksums.txt`n listed $listed`n actual $actual" }
  Write-Host "SHA256 OK (pinned and checksums.txt): $actual"

  Expand-Archive (Join-Path $tmp $asset) -DestinationPath (Join-Path $tmp "x")
  $exe = Get-ChildItem (Join-Path $tmp "x") -Recurse -Filter "livekit-server.exe" | Select-Object -First 1
  if (-not $exe) { throw "livekit-server.exe not found in the archive" }
  if ($expectedExe) {
    $exeHash = (Get-FileHash $exe.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($exeHash -ne $expectedExe) { throw "SHA256 of livekit-server.exe does not match the pinned hash" }
  }

  New-Item -ItemType Directory -Force -Path $Dest | Out-Null
  $out = Join-Path (Resolve-Path $Dest) "livekit-server.exe"
  Copy-Item $exe.FullName $out -Force
  Write-Host "Installed $out"
  & $out --version
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
