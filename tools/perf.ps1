# Performance harness (v1.0 plan, "leveza"): RAM, CPU and GPU of ONE client build in four
# scenarios, each against its own fresh dev hub (hub 7997, LiveKit 7970-7972). Never touches a live
# server on 7890/7880-7882.
#
# Scenarios (the client under test is always "pfA1"):
#   home    the home screen, nothing happening (a normal run, not an autotest, with its own empty
#           APPDATA / LOCALAPPDATA under the run folder: no saved servers, no tray icon)
#   call    in a voice call with a second client, the chat of a text channel shown with a few
#           messages (autotest role "voice --show previews"), once everything has settled
#   watch   watching a Go Live of a test_window inline in the call view (autotest roles a / b)
#   camera  own camera on, the synthetic one (VOX_FAKE_CAMERA=1), self-view in the call view
#
# For each scenario, after it has settled, samples the client once per second for -Secs seconds:
#   working set and private bytes (average), CPU % of one core (process CPU time / wall time) and
#   GPU engine utilisation ("\GPU Engine(pid_<PID>*)\Utilization Percentage": all engines summed,
#   and the 3D engines alone), averaged.
# v1.0 phase 3b: "busy" is how many "continuous repaint" lines the client logged while it was
#   measured (`repaint_watch.rs`: more than 12 frames a second nobody caused, with the callers
#   named, at most one line every 10 s); 0 is expected in "home" and "call".
#
# Background test mode: every client keeps its windows behind the others without taking the focus
# (VOX_TEST_BACKGROUND=1 / --autotest), the hub runs hidden, nothing is audible (VOX_NO_PLAYBACK=1,
# VOX_NO_MIC=1). Processes are stopped by their exact PID only.
#
# Build first:  cargo build --release --workspace   (with CARGO_TARGET_DIR=target-v02)
# Usage:        powershell -ExecutionPolicy Bypass -File tools\perf.ps1 [-Exe <concord.exe>] [-Assets <main tree>] [-Label name]
#                 [-Out results.csv] [-Only home,call,watch,camera] [-Secs 20] [-Repeat 1]
param(
  [string]$Exe = "",
  [string]$Label = "build",
  [string]$Out = "",
  [string[]]$Only = @(),
  [int]$Secs = 20,
  [int]$Repeat = 1,
  [string]$Assets = ""
)
$ErrorActionPreference = "Continue"
$Only = @($Only | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$R = Split-Path $PSScriptRoot -Parent
# CARGO_TARGET_DIR may be relative to the repo (target-v02) or absolute (a worktree sharing the main tree's).
$B = if (-not $env:CARGO_TARGET_DIR) { "$R\target-v02\release" } elseif ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { Join-Path $env:CARGO_TARGET_DIR "release" } else { Join-Path $R "$env:CARGO_TARGET_DIR\release" }
# bin\livekit-server.exe (and, for the gate, releases\) come from -Assets: the repo by default, or the
# main tree when this runs from a git worktree without them (read only).
if (-not $Assets) { $Assets = $R }
if (-not $Exe) { $Exe = "$B\concord.exe" }
$Exe = (Resolve-Path $Exe).Path
$LK = "$Assets\bin\livekit-server.exe"
$G = Join-Path $env:TEMP "concord-perf"
Remove-Item $G -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory $G | Out-Null
$env:VOX_NO_PLAYBACK = "1"; $env:VOX_NO_MIC = "1"; $env:VOX_FAKE_CAMERA = "1"

function Want($tag) { $Only.Count -eq 0 -or $Only -contains $tag }

function Start-Hub($tag) {
  $d = "$G\$tag\data"; New-Item -ItemType Directory $d -Force | Out-Null
  @"
server_name = "perf"
server_password = "x"
owner = "dono"
bind = "127.0.0.1:7997"
public_host = "127.0.0.1"
[livekit]
api_key = "APIperf000001"
api_secret = "0123456789abcdef0123456789abcdef0123456789abcdef"
signal_port = 7970
rtc_tcp_port = 7971
rtc_udp_port = 7972
use_external_ip = false
"@ | Set-Content "$d\config.toml" -Encoding ascii
  $h = Start-Process "$B\concord-server.exe" -ArgumentList "--data-dir `"$d`" --livekit-bin `"$LK`" --log-dir `"$G\$tag\hub`"" -WindowStyle Hidden -PassThru
  Start-Sleep 4
  return $h
}
function Stop-Hub($h, $tag) {
  if ($h -and -not $h.HasExited) { Stop-Process -Id $h.Id -Force }
  Start-Sleep 1
  # Only the LiveKit this scenario started (its command line names this scenario's folder).
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape("$G\$tag") } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
}
function Start-Client($exe, $tag, $name, $argline) {
  $p = Start-Process $exe -ArgumentList "$argline --name $name --run-id $tag --log-dir `"$G\$tag\$name`"" -PassThru
  $null = $p.Handle
  $p
}
function Stop-All($procs) {
  foreach ($p in $procs) { if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
}
# The "continuous repaint" lines of a client's log (phase 3b's repaint watch).
# (Only the lines from $since on: the log's timestamps are UTC, "yyyy-MM-ddTHH:mm:ss...".)
function Busy-Lines($tag, $name, $since) {
  $f = Get-ChildItem "$G\$tag\$name" -Filter "app.*.log" -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $f) { return 0 }
  @(Select-String -Path $f.FullName -Pattern "continuous repaint" | Where-Object { $_.Line.Length -ge 19 -and $_.Line.Substring(0, 19) -ge $since }).Count
}
function Now-Utc { (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss") }
function With-Busy($m, $tag, $name, $since) {
  if ($m) { $m | Add-Member -NotePropertyName busy -NotePropertyValue (Busy-Lines $tag $name $since) -Force }
  $m
}
# Waits until the client's log has a line matching $pattern (an autotest step), or $timeout s.
function Wait-Log($tag, $name, $pattern, $timeout) {
  $deadline = (Get-Date).AddSeconds($timeout)
  while ((Get-Date) -lt $deadline) {
    $f = Get-ChildItem "$G\$tag\$name" -Filter "app.*.log" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($f -and (Select-String -Path $f.FullName -Pattern $pattern -Quiet)) { return $true }
    Start-Sleep 1
  }
  return $false
}

# One measurement of process $p over $Secs seconds.
function Measure-Client($p, $scenario, $run) {
  $p.Refresh()
  $cpu0 = $p.TotalProcessorTime; $t0 = Get-Date
  $ws = @(); $priv = @(); $gAll = @(); $g3d = @()
  $path = "\GPU Engine(pid_$($p.Id)_*)\Utilization Percentage"
  for ($i = 0; $i -lt $Secs; $i++) {
    $s0 = Get-Date
    $all = 0.0; $d3 = 0.0
    try {
      # (A rate counter: Get-Counter samples it over ~1 s.)
      $c = Get-Counter -Counter $path -MaxSamples 1 -ErrorAction Stop
      foreach ($x in $c.CounterSamples) { $all += $x.CookedValue; if ($x.Path -match 'engtype_3d') { $d3 += $x.CookedValue } }
    } catch { }
    $gAll += $all; $g3d += $d3
    if ($p.HasExited) { break }
    $p.Refresh()
    $ws += $p.WorkingSet64; $priv += $p.PrivateMemorySize64
    $left = 1.0 - ((Get-Date) - $s0).TotalSeconds
    if ($left -gt 0) { Start-Sleep -Milliseconds ([int]($left * 1000)) }
  }
  if ($p.HasExited) { return [pscustomobject]@{ label = $Label; scenario = $scenario; run = $run; ok = $false } }
  $p.Refresh()
  $wall = ((Get-Date) - $t0).TotalSeconds
  $cpu = ($p.TotalProcessorTime - $cpu0).TotalSeconds / $wall * 100.0
  [pscustomobject]@{
    label = $Label; scenario = $scenario; run = $run; ok = $true
    ws_mb = [math]::Round(($ws | Measure-Object -Average).Average / 1MB, 1)
    ws_max_mb = [math]::Round(($ws | Measure-Object -Maximum).Maximum / 1MB, 1)
    private_mb = [math]::Round(($priv | Measure-Object -Average).Average / 1MB, 1)
    cpu_pct = [math]::Round($cpu, 2)
    gpu_all_pct = [math]::Round(($gAll | Measure-Object -Average).Average, 2)
    gpu_3d_pct = [math]::Round(($g3d | Measure-Object -Average).Average, 2)
    secs = [math]::Round($wall, 1)
  }
}

$results = @()
$tw = $null
if (Want "watch") {
  $tw = Start-Process "$B\examples\test_window.exe" -RedirectStandardOutput "$G\tw.txt" -PassThru
  Start-Sleep 5
}
$h1 = [int64]((Get-Content "$G\tw.txt" -ErrorAction SilentlyContinue | Select-String -Pattern '^hwnd=(\d+)' | Select-Object -First 1).Matches.Groups[1].Value)

for ($run = 1; $run -le $Repeat; $run++) {
  # (a) Home screen, idle: a normal run with an empty profile of its own.
  if (Want "home") {
    $tag = "home$run"
    $ad = "$G\$tag\appdata"
    # (Both folders: -Exe may be an older voxhub.exe, compared with the new concord.exe.)
    foreach ($app in "concord", "voxhub") {
      New-Item -ItemType Directory "$ad\$app" -Force | Out-Null
      '{"notify":{"windows":"off","tray_on_close":false}}' | Set-Content "$ad\$app\settings.json" -Encoding ascii
    }
    $saved = @{ APPDATA = $env:APPDATA; LOCALAPPDATA = $env:LOCALAPPDATA }
    $env:APPDATA = $ad; $env:LOCALAPPDATA = $ad; $env:VOX_TEST_BACKGROUND = "1"
    $p = Start-Client $Exe $tag "pfA1" ""
    $env:APPDATA = $saved.APPDATA; $env:LOCALAPPDATA = $saved.LOCALAPPDATA; Remove-Item Env:VOX_TEST_BACKGROUND
    Start-Sleep 10
    $t0 = Now-Utc
    $results += With-Busy (Measure-Client $p "home" $run) $tag "pfA1" $t0
    Stop-All @($p)
  }
  # (b) In a voice call, the chat shown, nothing changing.
  if (Want "call") {
    $tag = "call$run"; $hub = Start-Hub $tag
    $srv = "--server http://127.0.0.1:7997 --password x"
    $pb = Start-Client $Exe $tag "pfB1" "--autotest voice $srv"
    Start-Sleep 3
    $pa = Start-Client $Exe $tag "pfA1" "--autotest voice --show previews $srv"
    if (Wait-Log $tag "pfA1" "step \d+ -> 8:" 120) {
      Start-Sleep 12   # resting, link previews loaded
      $t0 = Now-Utc
      $results += With-Busy (Measure-Client $pa "call" $run) $tag "pfA1" $t0
    } else { $results += [pscustomobject]@{ label = $Label; scenario = "call"; run = $run; ok = $false } }
    Stop-All @($pa, $pb); Stop-Hub $hub $tag
  }
  # (c) Watching a Go Live (inline, in the call view).
  if (Want "watch") {
    $tag = "watch$run"; $hub = Start-Hub $tag
    $srv = "--server http://127.0.0.1:7997 --password x"
    $pa = Start-Client $Exe $tag "pfB1" "--autotest a --hwnd $h1 --hold 240 $srv"
    Start-Sleep 3
    $pb = Start-Client $Exe $tag "pfA1" "--autotest b --watch-secs 120 $srv"
    if (Wait-Log $tag "pfA1" "step \d+ -> 29:" 240) {
      Start-Sleep 4
      $t0 = Now-Utc
      $results += With-Busy (Measure-Client $pb "watch" $run) $tag "pfA1" $t0
    } else { $results += [pscustomobject]@{ label = $Label; scenario = "watch"; run = $run; ok = $false } }
    Stop-All @($pa, $pb); Stop-Hub $hub $tag
  }
  # (d) Own camera on (synthetic camera), in a call with a second client.
  if (Want "camera") {
    $tag = "camera$run"; $hub = Start-Hub $tag
    $srv = "--server http://127.0.0.1:7997 --password x"
    $pb = Start-Client $Exe $tag "pfB1" "--autotest voice $srv"
    Start-Sleep 3
    $pa = Start-Client $Exe $tag "pfA1" "--autotest cam-a --hold 240 $srv"
    if (Wait-Log $tag "pfA1" "step \d+ -> 6:" 150) {
      Start-Sleep 8
      $t0 = Now-Utc
      $results += With-Busy (Measure-Client $pa "camera" $run) $tag "pfA1" $t0
    } else { $results += [pscustomobject]@{ label = $Label; scenario = "camera"; run = $run; ok = $false } }
    Stop-All @($pa, $pb); Stop-Hub $hub $tag
  }
}
if ($tw -and -not $tw.HasExited) { Stop-Process -Id $tw.Id -Force }

$results | Format-Table -AutoSize | Out-String -Width 200
if ($Out) {
  $exists = Test-Path $Out
  $results | Where-Object { $_.ok } | Export-Csv -Path $Out -NoTypeInformation -Append:$exists
}
