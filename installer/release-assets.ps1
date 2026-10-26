# release-assets.ps1 — monta TODOS os artefatos da release do client AkumaLabs.
#
# Uso local (a partir da raiz do repo, com o build feito):
#   powershell -File installer\release-assets.ps1 -Version 1.0.3 -MinisignKey <caminho da chave privada>
#
# No CI (GitHub Actions): a chave vem da env AKUMALABS_SIGNING_KEY (o conteúdo
# do arquivo .key, sem senha), e o minisign.exe precisa estar no PATH.
#
# Produz em <repo>\dist\:
#   AkumaLabs-<ver>-setup.exe        instalador NSIS (o que o auto-update usa)
#   AkumaLabs-<ver>-setup.exe.sig    a assinatura minisign do instalador
#   AkumaLabs-<ver>-Windows.zip      versão portátil (sem auto-update)
#   latest.json                      o feed que o app consulta
#   release-body.md                  o corpo da release (seção do CHANGELOG)
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$MinisignKey = "",
    [string]$RepoRoot = ""
)

$ErrorActionPreference = "Stop"
if (-not $RepoRoot) { $RepoRoot = (Get-Location).Path }
$dist = Join-Path $RepoRoot "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$tag = "v$Version"
$setup = Join-Path $dist "AkumaLabs-$Version-setup.exe"

# 1. O instalador NSIS (requer makensis no PATH ou na pasta padrao, e build de release pronto).
# TODOS os caminhos vao absolutos via /D: o makensis do Windows resolve os
# caminhos relativos pelo CWD e o do Linux pelo diretorio do script - com
# absolutos, os dois constroem igual.
$makensis = Get-Command makensis -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source
if (-not $makensis) {
    $cand = Join-Path ${env:ProgramFiles(x86)} "NSIS\makensis.exe"
    if (Test-Path $cand) { $makensis = $cand }
}
if (-not $makensis) { throw "makensis nao encontrado - instale o NSIS (choco install nsis)" }
$exe = Join-Path $RepoRoot "target\release\AkumaLabs.exe"
if (-not (Test-Path $exe)) { throw "build o client antes: cargo build --release -p voxhub-app (faltou $exe)" }
$exeDir = Join-Path $RepoRoot "target\release"
$nsi = Join-Path $RepoRoot "installer\AkumaLabs.nsi"
$icon = Join-Path $RepoRoot "installer\akumalabs-setup.ico"
$leiame = Join-Path $RepoRoot "installer\LEIA-ME.txt"
& $makensis /DVERSION=$Version /DINPUT_EXE="$exe" /DOUT_FILE="$setup" /DICON_FILE="$icon" /DLEIAME_FILE="$leiame" $nsi | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makensis falhou" }
if (-not (Test-Path $setup)) { throw "o instalador não apareceu em $setup" }
Write-Host "setup.exe pronto: $setup"

# 2. O zip portátil.
$portable = Join-Path $dist "portable-$Version"
New-Item -ItemType Directory -Force -Path $portable | Out-Null
Copy-Item $exe $portable
Copy-Item (Join-Path $RepoRoot "installer\LEIA-ME.txt") $portable
$zip = Join-Path $dist "AkumaLabs-$Version-Windows.zip"
Compress-Archive -Path $portable -DestinationPath $zip -Force
Remove-Item $portable -Recurse -Force
Write-Host "zip portátil pronto: $zip"

# 3. SHA-256 do instalador.
$sha = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "sha256: $sha"

# 4. A assinatura minisign (chave da env ou do caminho dado; senha vazia).
$sig = "$setup.sig"
if (-not $MinisignKey -and $env:AKUMALABS_SIGNING_KEY) {
    $MinisignKey = Join-Path $dist "release-signing.key"
    # A env carrega o conteúdo do arquivo (com comentário e sem senha).
    $env:AKUMALABS_SIGNING_KEY -split "`n" | ForEach-Object { $_.TrimEnd("`r") } | Set-Content $MinisignKey -Encoding ascii
}
if (-not $MinisignKey) { throw "sem chave de assinatura: -MinisignKey ou env AKUMALABS_SIGNING_KEY" }
"`n" | & minisign -S -s $MinisignKey -m $setup -x $sig
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $sig)) { throw "minisign falhou ao assinar" }
# O .sig vem com CRLF (Windows); o decoder do app espera \n.
$signature = ((Get-Content $sig -Raw) -replace "`r", "").TrimEnd()
Write-Host "assinatura ok"

# 5. As notas desta versão (a seção [x.y.z] do CHANGELOG, como no KumaDrive).
$changelog = Join-Path $RepoRoot "CHANGELOG.md"
$notes = ""
if (Test-Path $changelog) {
    $lines = Get-Content $changelog
    $inSection = $false
    foreach ($l in $lines) {
        if ($l -match "^## \[$Version\]") { $inSection = $true; continue }
        if ($inSection -and $l -match "^## \[") { break }
        if ($inSection) { $notes += "$l`n" }
    }
}
if (-not $notes.Trim()) { $notes = "Veja o CHANGELOG.md do projeto para os detalhes desta versão." }

# 6. O feed que o app consulta (o campo `signature` carrega o .sig inteiro).
$manifest = [ordered]@{
    version   = $Version
    notes     = $notes.TrimEnd()
    url       = "https://github.com/AkumaHalls/KumaCord/releases/download/$tag/AkumaLabs-$Version-setup.exe"
    sha256    = $sha
    signature = $signature
}
($manifest | ConvertTo-Json) | Set-Content (Join-Path $dist "latest.json") -Encoding ascii
Write-Host "latest.json pronto"

# 7. O corpo da release.
$body = @"
## AkumaLabs $tag

### Novidades desta versão

$($notes.TrimEnd())

---

**Windows**
- **`AkumaLabs-$Version-setup.exe`** (recomendado — suporta atualização automática)
- `AkumaLabs-$Version-Windows.zip` (portátil; sem atualização automática)

O app instalado avisa e atualiza sozinho quando há versão nova.
"@
$body | Set-Content (Join-Path $dist "release-body.md") -Encoding utf8
Write-Host "release-body.md pronto"
Write-Host "TODOS OS ARTEFATOS EM $dist"
