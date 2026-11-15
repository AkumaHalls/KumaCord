# build.ps1 - regenera o pacote de icones dos jogos (app/assets/game-icons.pack).
#
# Uso (a partir da raiz do repo):
#   powershell -File tools\game_icons\build.ps1
#
# O que faz:
#   1. baixa a lista de "detectable applications" do Discord (a mesma que o cliente
#      oficial consulta; publica, sem conta) para um cache local;
#   2. cruza os nomes do catalogo do app (app/src/games.rs) com a lista, com a MESMA
#      regra de match do app (exato, depois prefixo) - tools\game_icons\manifest.py;
#   3. baixa o icone de cada jogo do CDN publico do Discord e reduz para 64x64
#      (o tile na tela tem 16-22 px, sobra);
#   4. empacota tudo em app/assets/game-icons.pack (include_bytes! no app) -
#      tools\game_icons\pack.py.
#
# Requer ffmpeg (com libpng) e Python 3. O pack vai no repo: com ele o app mostra o
# icone real do jogo SEM rede nenhuma; o que nao estiver no pack continua sendo
# resolvido ao vivo pela API publica do Discord (e, falhando, pelo Steam/emoji).
param(
    [string]$Ffmpeg = "",
    [string]$Python = "python",
    [int]$Size = 64,
    [string]$DetectableCache = "$env:TEMP\kuma-rea\detectable.json"
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not $Ffmpeg) {
    $cmd = Get-Command ffmpeg -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source
    if (-not $cmd) { throw "ffmpeg nao encontrado - passe -Ffmpeg <caminho>" }
    $Ffmpeg = $cmd
}

if (-not (Test-Path $DetectableCache)) {
    New-Item -ItemType Directory -Force -Path (Split-Path $DetectableCache) | Out-Null
    Write-Host "1/4 baixando a lista de jogos do Discord (~13 MB)..."
    Invoke-WebRequest -Uri "https://discord.com/api/v9/applications/detectable" -OutFile $DetectableCache -TimeoutSec 120 -UseBasicParsing -Headers @{"User-Agent" = "Mozilla/5.0" }
}

Write-Host "2/4 cruzando o catalogo do app com a lista..."
$manifest = Join-Path $env:TEMP "kuma-rea\manifest.json"
& $Python (Join-Path $PSScriptRoot "manifest.py") (Join-Path $root "app\src\games.rs") $DetectableCache $manifest
if ($LASTEXITCODE -ne 0) { throw "manifest.py falhou" }

Write-Host "3/4 baixando e reduzindo os icones (${Size}x${Size})..."
$icons = Join-Path $env:TEMP "kuma-rea\icons"
New-Item -ItemType Directory -Force -Path $icons | Out-Null
Get-ChildItem "$icons\*.png" -ErrorAction SilentlyContinue | Remove-Item -Force
$m = Get-Content $manifest -Raw | ConvertFrom-Json
$ok = 0; $fail = @()
foreach ($e in $m) {
    $url = "https://cdn.discordapp.com/app-assets/$($e.app)/x.png"  # placeholder, real abaixo
    $url = "https://cdn.discordapp.com/app-icons/$($e.app)/$($e.icon).png?size=256"
    $raw = Join-Path $env:TEMP "kuma-rea\icon-raw.png"
    try {
        Invoke-WebRequest -Uri $url -OutFile $raw -TimeoutSec 20 -UseBasicParsing -Headers @{"User-Agent" = "Mozilla/5.0" }
        & $Ffmpeg -hide_banner -loglevel error -y -i $raw -vf "scale=${Size}:${Size}:flags=lanczos" (Join-Path $icons "$($e.key).png")
        if ($LASTEXITCODE -ne 0) { throw "ffmpeg falhou" }
        $ok++
    } catch {
        $fail += "$($e.name): $($_.Exception.Message)"
    }
}
"icones baixados: $ok de $($m.Count)"
if ($fail.Count -gt 0) { "falhas:`n" + ($fail -join "`n") }

# Cantos arredondados (transparentes), como o Discord desenha a arte da atividade.
& $Python (Join-Path $PSScriptRoot "round_corners.py") $icons $Size
if ($LASTEXITCODE -ne 0) { throw "round_corners.py falhou" }

Write-Host "4/4 empacotando..."
& $Python (Join-Path $PSScriptRoot "pack.py") $manifest $icons (Join-Path $root "app\assets\game-icons.pack")
if ($LASTEXITCODE -ne 0) { throw "pack.py falhou" }
Write-Host "pronto: app\assets\game-icons.pack"