# release-assets.ps1 - monta TODOS os artefatos da release do client KumaCord.
#
# Uso local (a partir da raiz do repo, com o build feito):
#   powershell -File installer\release-assets.ps1 -Version 1.2.0 -MinisignKey <caminho da chave privada>
#
# No CI (GitHub Actions): a chave vem da env AKUMALABS_SIGNING_KEY (o conteudo
# do arquivo .key, sem senha), e o minisign.exe precisa estar no PATH.
#
# Produz em <repo>\dist\:
#   KumaCord-<ver>-setup.exe        instalador NSIS (o que o auto-update usa)
#   KumaCord-<ver>-setup.exe.sig    a assinatura minisign do instalador
#   KumaCord-<ver>-Windows.zip      versao portatil (sem auto-update)
#   latest.json                     o feed que o app consulta
#   release-body.md                 o corpo da release (secao do CHANGELOG)
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
$setup = Join-Path $dist "KumaCord-$Version-setup.exe"

# 0. Os assets da abertura (o filme inteiro e a trilha): vao AO LADO do exe,
# nao dentro dele, e sao obrigatorios para empacotar.
$introPack = Join-Path $RepoRoot "app\assets\intro\intro.pack"
$introMp3 = Join-Path $RepoRoot "app\assets\intro\intro.mp3"
foreach ($a in @($introPack, $introMp3)) {
    if (-not (Test-Path $a)) { throw "falta o asset da abertura: $a (rode tools\intro\build.ps1)" }
}

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
$exe = Join-Path $RepoRoot "target\release\KumaCord.exe"
if (-not (Test-Path $exe)) { throw "build o client antes: cargo build --release -p voxhub-app (faltou $exe)" }
$nsi = Join-Path $RepoRoot "installer\KumaCord.nsi"
$icon = Join-Path $RepoRoot "installer\akumalabs-setup.ico"
$leiame = Join-Path $RepoRoot "installer\LEIA-ME.txt"
& $makensis /DVERSION=$Version /DINPUT_EXE="$exe" /DOUT_FILE="$setup" /DICON_FILE="$icon" /DLEIAME_FILE="$leiame" /DINTRO_PACK="$introPack" /DINTRO_MP3="$introMp3" $nsi | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makensis falhou" }
if (-not (Test-Path $setup)) { throw "o instalador nao apareceu em $setup" }
Write-Host "setup.exe pronto: $setup"

# 2b. Uma copia com nome FIXO (sem a versao): a landing page linka
#     .../releases/latest/download/KumaCord-setup.exe sem precisar saber a versao,
#     e o mesmo URL serve para sempre. O hash no latest.json continua sendo o que
#     o app confere - o conteudo e identico.
$fixed = Join-Path $dist "KumaCord-setup.exe"
Copy-Item $setup $fixed -Force
Write-Host "setup.exe de nome fixo pronto: $fixed"

# 2. O zip portatil (exe + a abertura, que o app le do lado do exe).
$portable = Join-Path $dist "portable-$Version"
New-Item -ItemType Directory -Force -Path $portable | Out-Null
Copy-Item $exe (Join-Path $portable "KumaCord.exe")
Copy-Item (Join-Path $RepoRoot "installer\LEIA-ME.txt") $portable
New-Item -ItemType Directory -Force -Path (Join-Path $portable "intro") | Out-Null
Copy-Item $introPack (Join-Path $portable "intro")
Copy-Item $introMp3 (Join-Path $portable "intro")
$zip = Join-Path $dist "KumaCord-$Version-Windows.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Push-Location $portable
Compress-Archive -Path * -DestinationPath $zip -Force
Pop-Location
Remove-Item $portable -Recurse -Force
Write-Host "zip portatil pronto: $zip"

# 3. SHA-256 do instalador.
$sha = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "sha256: $sha"

# 4. A assinatura minisign (chave da env ou do caminho dado; senha vazia).
$sig = "$setup.sig"
if (-not $MinisignKey -and $env:AKUMALABS_SIGNING_KEY) {
    $MinisignKey = Join-Path $dist "release-signing.key"
    # A env carrega o conteudo do arquivo
    ($env:AKUMALABS_SIGNING_KEY -split "`n" | ForEach-Object { $_.TrimEnd("`r") }) | Set-Content $MinisignKey -Encoding ascii
}
if (-not $MinisignKey) { throw "sem chave de assinatura: -MinisignKey ou env AKUMALABS_SIGNING_KEY" }
"`n" | & minisign -S -s $MinisignKey -m $setup -x $sig
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $sig)) { throw "minisign falhou ao assinar" }
# O .sig vem com CRLF (Windows); o decoder do app espera \n.
$signature = ((Get-Content $sig -Raw) -replace "`r", "").TrimEnd()
Write-Host "assinatura ok"

# 5. As notas desta versao (a secao [x.y.z] do CHANGELOG, como no KumaDrive).
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
if (-not $notes.Trim()) { $notes = "Veja o CHANGELOG.md do projeto para os detalhes desta versao." }

# 6. O feed que o app consulta (o campo `signature` carrega o .sig inteiro).
#    A URL usa o nome fixo: `releases/latest/download/KumaCord-setup.exe` aponta
#    para a release mais recente, entao o feed nao carrega a versao no caminho.
$manifest = [ordered]@{
    version   = $Version
    notes     = $notes.TrimEnd()
    url       = "https://github.com/AkumaHalls/KumaCord/releases/latest/download/KumaCord-setup.exe"
    sha256    = $sha
    signature = $signature
}
($manifest | ConvertTo-Json) | Set-Content (Join-Path $dist "latest.json") -Encoding ascii
Write-Host "latest.json pronto"

# 7. O corpo da release.
$body = @"
## KumaCord $tag

### Novidades desta versao

$($notes.TrimEnd())

---

**Windows**
- **`KumaCord-$Version-setup.exe`** (recomendado - suporta atualizacao automatica)
- `KumaCord-$Version-Windows.zip` (portatil; sem atualizacao automatica)

O app instalado avisa e atualiza sozinho quando ha versao nova.
"@
$body | Set-Content (Join-Path $dist "release-body.md") -Encoding utf8
Write-Host "release-body.md pronto"
Write-Host "TODOS OS ARTEFATOS EM $dist"
