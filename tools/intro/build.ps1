# build.ps1 - regenera a abertura do launcher (o filme inteiro + a trilha).
#
# Uso (a partir da raiz do repo):
#   powershell -File tools\intro\build.ps1 -Video "C:\caminho\cinematica.mp4"
#
# Saida:
#   app\assets\intro\intro.pack   frames WebP empacotados (lidos do disco pelo app)
#   app\assets\intro\intro.mp3    a trilha do proprio filme (loop no mixer)
#   target\release\intro\         copia pronta para rodar o build local
#
# Requer ffmpeg (com libwebp) e Python 3 no PATH - ou passe -Ffmpeg / -Python.
param(
    [Parameter(Mandatory = $true)][string]$Video,
    [string]$Ffmpeg = "",
    [string]$Python = "python",
    [int]$Fps = 15,
    [int]$Width = 960,
    [int]$Quality = 55,
    [int]$AudioKbps = 160
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not $Ffmpeg) {
    $cmd = Get-Command ffmpeg -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source
    if (-not $cmd) { throw "ffmpeg nao encontrado - passe -Ffmpeg <caminho>" }
    $Ffmpeg = $cmd
}
if (-not (Test-Path $Video)) { throw "video nao encontrado: $Video" }

$outDir = Join-Path $root "app\assets\intro"
$frames = Join-Path $root "brag-output\work\intro-frames"
New-Item -ItemType Directory -Force -Path $outDir, $frames | Out-Null
Get-ChildItem "$frames\*.webp" -ErrorAction SilentlyContinue | Remove-Item -Force

Write-Host "1/3 trilha (mp3 $AudioKbps kbps, 48 kHz estereo)..."
& $Ffmpeg -hide_banner -loglevel error -y -i $Video -vn -c:a libmp3lame -b:a "$($AudioKbps)k" -ar 48000 -ac 2 (Join-Path $outDir "intro.mp3")
if ($LASTEXITCODE -ne 0) { throw "ffmpeg falhou na trilha" }

Write-Host "2/3 frames ($Fps fps, $Width px de largura, qualidade $Quality)..."
# -2 mantem a proporcao original (a cinematica e mais larga que 16:9).
& $Ffmpeg -hide_banner -loglevel error -y -i $Video -vf "fps=$Fps,scale=${Width}:-2:flags=lanczos" -c:v libwebp -quality $Quality -compression_level 4 "$frames\f_%04d.webp"
if ($LASTEXITCODE -ne 0) { throw "ffmpeg falhou nos frames" }

$probe = & $Ffmpeg -hide_banner -loglevel error -i "$frames\f_0001.webp" 2>&1 | Select-String -Pattern '(\d{2,4})x(\d{2,4})'
$w = if ($probe) { [int]$probe.Matches[0].Groups[1].Value } else { $Width }
$h = if ($probe) { [int]$probe.Matches[0].Groups[2].Value } else { 0 }
if (-not $h) { $h = [int]([math]::Round($Width * 101 / 240 / 2) * 2) }

Write-Host "3/3 pacote (${w}x${h})..."
& $Python (Join-Path $PSScriptRoot "pack.py") $frames (Join-Path $outDir "intro.pack") $w $h $Fps
if ($LASTEXITCODE -ne 0) { throw "pack.py falhou" }

# Copia para o lado do exe: e de la que o app le (exe\intro\...).
$bin = Join-Path $root "target\release\intro"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
Copy-Item (Join-Path $outDir "intro.pack") $bin -Force
Copy-Item (Join-Path $outDir "intro.mp3") $bin -Force
Write-Host "pronto: $outDir (e a copia em target\release\intro)"
