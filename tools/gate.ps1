# Release gate: every autotest scenario against its own fresh dev hub (hub 7997, LiveKit 7970-7972;
# the host-mode scenario uses 7990/7980-7982, the server-list one a second hub on 7998/7973-7975).
# Never touches a live server on 7890/7880-7882.
# Build first:  cargo build --release --workspace   (with CARGO_TARGET_DIR=target-v02)
# Usage:        powershell -ExecutionPolicy Bypass -File tools\gate.ps1 [-Only name,name] [-Assets <main tree>]
#               (-Assets: where bin\livekit-server.exe and releases\ are, when run from a worktree)
param([string[]]$Only = @(), [string]$Assets = "")
$ErrorActionPreference = "Continue"
# -File passes "a,b" as one string: split it.
$Only = @($Only | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$R = Split-Path $PSScriptRoot -Parent
# CARGO_TARGET_DIR may be relative to the repo (target-v02) or absolute (a worktree sharing the main tree's).
$B = if (-not $env:CARGO_TARGET_DIR) { "$R\target-v02\release" } elseif ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { Join-Path $env:CARGO_TARGET_DIR "release" } else { Join-Path $R "$env:CARGO_TARGET_DIR\release" }
# bin\livekit-server.exe (and, for the gate, releases\) come from -Assets: the repo by default, or the
# main tree when this runs from a git worktree without them (read only).
if (-not $Assets) { $Assets = $R }
$LK = "$Assets\bin\livekit-server.exe"
$G = Join-Path $env:TEMP "concord-gate"
Remove-Item $G -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory $G | Out-Null
$env:VOX_NO_PLAYBACK = "1"; $env:VOX_NO_MIC = "1"; $env:VOX_FAKE_CAMERA = "1"

function Want($tag) { $Only.Count -eq 0 -or $Only -contains $tag }

function Start-Hub($tag) {
  $d = "$G\$tag\data"; New-Item -ItemType Directory $d -Force | Out-Null
  @"
server_name = "gate"
server_password = "x"
owner = "dono"
bind = "127.0.0.1:7997"
public_host = "127.0.0.1"
[livekit]
api_key = "APIgate000001"
api_secret = "0123456789abcdef0123456789abcdef0123456789abcdef"
signal_port = 7970
rtc_tcp_port = 7971
rtc_udp_port = 7972
use_external_ip = false
"@ | Set-Content "$d\config.toml" -Encoding ascii
  $h = Start-Process "$B\kumacord-server.exe" -ArgumentList "--data-dir `"$d`" --livekit-bin `"$LK`" --log-dir `"$G\$tag\hub`"" -WindowStyle Hidden -PassThru
  Start-Sleep 4
  return $h
}
function Stop-Hub($h, $tag) {
  if ($h -and -not $h.HasExited) { Stop-Process -Id $h.Id -Force }
  Start-Sleep 1
  # Only the LiveKit this scenario started (its command line names this scenario's folder).
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape("$G\$tag") } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
}
# A program on the gate's own desktop ("concord-gate", created once, never switched to): nothing it
# shows reaches the user's screen and it cannot take the foreground. For old releases, which have no
# background test mode. Returns its Process (exit code readable).
function Start-Offscreen($exe, $argline) {
  if (-not ("GateDesk" -as [type])) {
    Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class GateDesk {
  [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  static extern IntPtr CreateDesktopW(string name, IntPtr device, IntPtr devmode, uint flags, uint access, IntPtr sa);
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  struct SI { public int cb; public string reserved; public string desktop; public string title; public int x, y, xs, ys, xc, yc, fill, flags; public short show, r2; public IntPtr r3, i, o, e; }
  [StructLayout(LayoutKind.Sequential)]
  struct PI { public IntPtr process, thread; public int pid, tid; }
  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  static extern bool CreateProcessW(string app, StringBuilder cmd, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string dir, ref SI si, out PI pi);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
  static IntPtr desk;
  public static int Start(string exe, string args) {
    if (desk == IntPtr.Zero) desk = CreateDesktopW("concord-gate", IntPtr.Zero, IntPtr.Zero, 0, 0x10000000, IntPtr.Zero);
    if (desk == IntPtr.Zero) return -1;
    var si = new SI(); si.cb = Marshal.SizeOf(typeof(SI)); si.desktop = "concord-gate";
    PI pi;
    if (!CreateProcessW(null, new StringBuilder("\"" + exe + "\" " + args), IntPtr.Zero, IntPtr.Zero, false, 0, IntPtr.Zero, null, ref si, out pi)) return -1;
    CloseHandle(pi.thread); CloseHandle(pi.process);
    return pi.pid;
  }
}
"@
  }
  $id = [GateDesk]::Start($exe, $argline)
  if ($id -le 0) { throw "could not start $exe on the gate desktop" }
  $p = [System.Diagnostics.Process]::GetProcessById($id)
  $null = $p.Handle   # keeps the handle open, so ExitCode is readable once it has exited
  $p
}
# $specs: role, name, extra args, delay after start, optional exe (another app build, e.g. an old
# release) and offscreen=$true (run it on the gate's own desktop, see Start-Offscreen).
function Run-Clients($tag, $specs, $timeout, [switch]$NoHub) {
  if (-not (Want $tag)) { return }
  $hub = if ($NoHub) { $null } else { Start-Hub $tag }
  $procs = @()
  foreach ($s in $specs) {
    $exe = if ($s.exe) { $s.exe } else { "$B\AkumaLabs.exe" }
    $server = if ($s.server) { $s.server } else { "--server http://127.0.0.1:7997 --password x" }
    $args = "--autotest $($s.role) $server --name $($s.name) --run-id $tag --log-dir `"$G\$tag\$($s.name)`" $($s.extra)"
    # Our own clients keep their windows at the bottom without focus (background test mode). An
    # old release (offscreen=$true) knows nothing of that and takes the foreground, so it runs on
    # a desktop of its own that is never shown (its window can be neither seen nor focused).
    $p = if ($s.offscreen) { Start-Offscreen $exe $args } else { Start-Process $exe -ArgumentList $args -PassThru }
    $procs += [pscustomobject]@{ name = $s.name; p = $p }
    Start-Sleep $s.delay
  }
  $deadline = (Get-Date).AddSeconds($timeout)
  while ((Get-Date) -lt $deadline -and ($procs | Where-Object { -not $_.p.HasExited })) { Start-Sleep 2 }
  $line = "$tag :"
  foreach ($x in $procs) {
    if ($x.p.HasExited) { $line += " $($x.name)=$($x.p.ExitCode)" } else { $line += " $($x.name)=TIMEOUT"; Stop-Process -Id $x.p.Id -Force }
  }
  Stop-Hub $hub $tag
  $line
}

# The test windows never take the foreground (they sit at the bottom of the z-order), so
# MainWindowHandle may name another of their windows: each one prints its own "hwnd=<n>".
$tw1 = Start-Process "$B\examples\test_window.exe" -RedirectStandardOutput "$G\tw1.txt" -PassThru
$tw2 = Start-Process "$B\examples\test_window.exe" -RedirectStandardOutput "$G\tw2.txt" -PassThru
Start-Sleep 5
function TwHwnd($f) { [int64]((Get-Content $f -ErrorAction SilentlyContinue | Select-String -Pattern '^hwnd=(\d+)' | Select-Object -First 1).Matches.Groups[1].Value) }
$h1 = TwHwnd "$G\tw1.txt"; $h2 = TwHwnd "$G\tw2.txt"

Run-Clients "golive"  @(@{role="a"; name="dono"; extra="--hwnd $h1"; delay=3}, @{role="b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "preview" @(@{role="a"; name="dono"; extra="--hwnd $h1 --preview"; delay=3}, @{role="b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "camera"  @(@{role="cam-a"; name="dono"; extra=""; delay=3}, @{role="cam-b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "multi"   @(@{role="multi-a"; name="mA1"; extra="--hwnd $h1"; delay=2}, @{role="multi-a"; name="mA2"; extra="--hwnd $h2"; delay=3}, @{role="multi-b"; name="Tati"; extra=""; delay=0}) 300
# STAGE scenario (v1.0): the call stage. Two streamers (multi-a, each sharing a test window), a
# camera (cam-a) and stage-b, which pins both streams and the camera, hides a card without video,
# enters and leaves the stage fullscreen, checking that the video windows follow the cards.
Run-Clients "stage"   @(@{role="multi-a"; name="sgA1"; extra="--hwnd $h1"; delay=2}, @{role="multi-a"; name="sgA2"; extra="--hwnd $h2"; delay=2}, @{role="cam-a"; name="sgC1"; extra="--hold 20"; delay=3}, @{role="stage-b"; name="sgB1"; extra=""; delay=0}) 300
Run-Clients "audio"   @(@{role="audio-a"; name="dono"; extra=""; delay=3}, @{role="audio-b"; name="Tati"; extra=""; delay=0}) 300
Run-Clients "mod"     @(@{role="mod-a"; name="dono"; extra="--mode Tati"; delay=3}, @{role="mod-b"; name="Tati"; extra=""; delay=0}) 300
Run-Clients "avatar"  @(@{role="avatar-a"; name="dono"; extra=""; delay=3}, @{role="avatar-b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "sound"   @(@{role="sound-a"; name="dono"; extra=""; delay=3}, @{role="sound-b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "profile" @(@{role="profile-a"; name="dono"; extra=""; delay=3}, @{role="profile-b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "dm"      @(@{role="dm-b"; name="dmB1"; extra=""; delay=2}, @{role="dm-a"; name="dmA1"; extra=""; delay=0}) 300
Run-Clients "menu"    @(@{role="menu-b"; name="menuB1"; extra=""; delay=2}, @{role="menu-a"; name="menuA1"; extra="--hwnd $h1"; delay=0}) 300
Run-Clients "img"     @(@{role="img-b"; name="imgB1"; extra=""; delay=2}, @{role="img-a"; name="imgA1"; extra=""; delay=0}) 300

$env:VOX_TRAY_TEST = "1"   # notify-b gets the real tray icon and taskbar badge; notifications/sounds stay silent
Run-Clients "notify"  @(@{role="notify-c"; name="ntC1"; extra=""; delay=1}, @{role="notify-b"; name="ntB1"; extra=""; delay=1}, @{role="notify-a"; name="ntA1"; extra=""; delay=0}) 300
Remove-Item Env:VOX_TRAY_TEST

$env:VOX_FAKE_CAMERA_LOWLIGHT = "1"   # the synthetic camera plays a dim room ("Priorizar fluidez")
Run-Clients "camwin"  @(@{role="camwin"; name="dono"; extra=""; delay=0}) 180
Remove-Item Env:VOX_FAKE_CAMERA_LOWLIGHT

# HOST-MODE scenario and CHATX scenario: added below by their owners.

# HOST-MODE scenario (v0.8): no dev hub, the server runs inside host-a's app on the test block
# 7990/7980-7982 with the network part off (the router is never touched), everything on 127.0.0.1.
# A hosts and makes an invite, B joins with only the code (chat + voice), A revokes, C is refused.
# The clients exchange the code through files in $G\hostinv\x (--mode).
if (Want "hostinv") {
  $hx = "$G\hostinv\x"; New-Item -ItemType Directory $hx -Force | Out-Null
  $env:VOX_HOST_DIR = "$G\hostinv\host"; $env:VOX_HOST_OFFLINE = "1"; $env:VOX_HOST_BIND = "127.0.0.1"
  $env:VOX_HOST_FIRST_BLOCK = "1"; $env:VOX_HOST_LAN = "127.0.0.1/8"; $env:VOX_HOST_LIVEKIT_BIN = $LK
  $hs = "--mode `"$hx`""
  Run-Clients "hostinv" @(@{role="host-a"; name="hostA1"; server=$hs; extra=""; delay=1}, @{role="host-b"; name="hostB1"; server=$hs; extra=""; delay=1}, @{role="host-c"; name="hostC1"; server=$hs; extra=""; delay=0}) 360 -NoHub
  foreach ($k in "VOX_HOST_DIR", "VOX_HOST_OFFLINE", "VOX_HOST_BIND", "VOX_HOST_FIRST_BLOCK", "VOX_HOST_LAN", "VOX_HOST_LIVEKIT_BIN") { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
}

# CHATX scenario (v0.8): mentions, replies and reactions. chatx-a mentions chatx-b from the member
# list, B replies (with and without "@ ligado"), both react live in the channel and in a DM, A
# deletes a quoted message. The released v0.7.0 app (its notify-c role) reads the same channel
# alongside: the old-app check (it must skip the new events and still pass). The old app is
# unzipped once from releases\v0.7.0 into a cache under %TEMP% (kept between runs, never in the repo).
if (Want "chatx") {
  $cache = Join-Path $env:TEMP "voxhub-gate-cache\v0.7.0"
  $old = "$cache\voxhub.exe"
  if (-not (Test-Path $old)) {
    New-Item -ItemType Directory $cache -Force | Out-Null
    Expand-Archive "$Assets\releases\v0.7.0\voxhub-v0.7.0-PUBLICO.zip" $cache -Force
    $inner = Get-ChildItem $cache -Recurse -Filter voxhub.exe | Select-Object -First 1
    if ($inner -and $inner.FullName -ne $old) { Move-Item $inner.FullName $old -Force }
  }
  Run-Clients "chatx" @(@{role="notify-c"; name="ntC1"; exe=$old; offscreen=$true; extra=""; delay=1}, @{role="chatx-b"; name="cxB1"; extra=""; delay=2}, @{role="chatx-a"; name="cxA1"; extra=""; delay=0}) 300
}

# FIX scenario (v0.8.2): the owner (fix-a, "dono") and a member (fix-b) delete their own messages
# with images (channel and DM; the confirmation and Shift), a new picture after the newest one was
# deleted shows its own thumbnail (fresh ids, different content hash), a deleted and recreated
# channel opens empty with none of the old state, and the sidebar badges never cover the name.
Run-Clients "fix"     @(@{role="fix-a"; name="dono"; extra=""; delay=3}, @{role="fix-b"; name="fxB1"; extra=""; delay=0}) 300

# PRIV scenario (v0.9): private channels. The owner (priv-a, "dono") makes a text and a voice
# channel private for one member (priv-b); the other member (priv-c) sees nothing of them and its
# voice join is refused; priv-b chats there and joins the voice room; the owner takes priv-b off
# and priv-b loses both live (taken out of the LiveKit room at once, cached messages dropped).
Run-Clients "priv"    @(@{role="priv-c"; name="pvC1"; extra=""; delay=1}, @{role="priv-b"; name="pvB1"; extra=""; delay=1}, @{role="priv-a"; name="dono"; extra=""; delay=0}) 300

# NAV scenario (v1.0): the navigation and the center modes. The owner (nav-a, "dono") opens a text
# channel, joins the voice room (only the room is highlighted, the text channel loses it), goes
# through the three modes, checks that a message in the visible docked chat arrives read (and one
# in a hidden channel does not), text -> voice -> text from the sidebar, the hover bar, a DM;
# nav-b sits in the room with its camera on and answers on cue.
Run-Clients "nav"     @(@{role="nav-b"; name="navB1"; extra=""; delay=3}, @{role="nav-a"; name="dono"; extra=""; delay=0}) 300

# SRVLIST scenario (v0.9.1): the saved server list. Two dev hubs: "gate1" on 7997 (LiveKit
# 7970-7972) and "gate2" on 7998 (LiveKit 7973-7975). The app keeps its settings in a folder of the
# run (--settings-dir), first written the way v0.9.0 left it: gate1 as the single saved server with
# its password DPAPI-encrypted. Phase 1 checks the migration, saves gate2 by typing its address
# and switches back to gate1 with one click; phase 2 is the restarted app: both online, one click
# each (login token / saved password), "Trocar de servidor", the form asking only for a forgotten
# password, and removing gate2 with its secrets.
if (Want "srvlist") {
  $sd = "$G\srvlist"; $cfg = "$sd\settings"; New-Item -ItemType Directory $cfg -Force | Out-Null
  function Start-ListHub($sub, $port, $lk, $name, $pw) {
    $d = "$sd\$sub\data"; New-Item -ItemType Directory $d -Force | Out-Null
    @"
server_name = "$name"
server_password = "$pw"
owner = "dono"
bind = "127.0.0.1:$port"
public_host = "127.0.0.1"
[livekit]
api_key = "APIgate000001"
api_secret = "0123456789abcdef0123456789abcdef0123456789abcdef"
signal_port = $lk
rtc_tcp_port = $($lk + 1)
rtc_udp_port = $($lk + 2)
use_external_ip = false
"@ | Set-Content "$d\config.toml" -Encoding ascii
    Start-Process "$B\kumacord-server.exe" -ArgumentList "--data-dir `"$d`" --livekit-bin `"$LK`" --log-dir `"$sd\$sub\hub`"" -WindowStyle Hidden -PassThru
  }
  $pw1 = "srv-um-123"
  $hubs = @((Start-ListHub "hub1" 7997 7970 "gate1" $pw1), (Start-ListHub "hub2" 7998 7973 "gate2" "srv-dois-456"))
  Start-Sleep 5
  # settings.json as v0.9.0 wrote it: one server, its password encrypted for this Windows user.
  Add-Type -AssemblyName System.Security
  $enc = [Security.Cryptography.ProtectedData]::Protect([Text.Encoding]::UTF8.GetBytes($pw1), [Text.Encoding]::ASCII.GetBytes("voxhub-settings-v1"), [Security.Cryptography.DataProtectionScope]::CurrentUser)
  $hex = ($enc | ForEach-Object { $_.ToString("x2") }) -join ""
  "{`"server`":`"http://127.0.0.1:7997`",`"username`":`"slA1`",`"remember_password`":true,`"password_enc`":`"$hex`",`"volume_pct`":100,`"device_tokens`":{}}" | Set-Content "$cfg\settings.json" -Encoding ascii
  $line = "srvlist :"
  foreach ($phase in "phase1", "phase2") {
    $a = "--autotest srvlist --server http://127.0.0.1:7997 --password $pw1 --name slA1 --run-id srvlist --log-dir `"$sd\$phase`" --mode $phase --settings-dir `"$cfg`""
    $p = Start-Process "$B\AkumaLabs.exe" -ArgumentList $a -PassThru
    $null = $p.Handle   # keeps the handle, so ExitCode is readable once it has exited
    if (-not $p.WaitForExit(180000)) { Stop-Process -Id $p.Id -Force; $line += " $phase=TIMEOUT"; break }
    $line += " $phase=$($p.ExitCode)"
    if ($p.ExitCode -ne 0) { break }
  }
  foreach ($h in $hubs) { if ($h -and -not $h.HasExited) { Stop-Process -Id $h.Id -Force } }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($sd) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  $line
}

# CALLADEX scenario (v1.0.1): the easter egg. Two members sit online (idle, stopped by PID at the
# end); the owner types "calladex" on the home screen (the credits card), then in a channel sends
# a plain message containing the word (it must arrive) and the exact /calladex (it must open the
# members' card and never reach the hub).
if (Want "calladex") {
  $hub = Start-Hub "calladex"
  $parts = @()
  foreach ($n in "cdxB1", "cdxC1") {
    $parts += Start-Process "$B\AkumaLabs.exe" -ArgumentList "--autotest idle --server http://127.0.0.1:7997 --password x --name $n --run-id calladex --log-dir `"$G\calladex\$n`"" -PassThru
    Start-Sleep 2
  }
  Start-Sleep 3
  $c = Start-Process "$B\AkumaLabs.exe" -ArgumentList "--autotest calladex --server http://127.0.0.1:7997 --password x --name dono --run-id calladex --log-dir `"$G\calladex\dono`"" -PassThru
  $null = $c.Handle
  $line = "calladex :"
  if ($c.WaitForExit(180000)) { $line += " dono=$($c.ExitCode)" } else { Stop-Process -Id $c.Id -Force; $line += " dono=TIMEOUT" }
  foreach ($p in $parts) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
  Stop-Hub $hub "calladex"
  $line
}
foreach ($p in $tw1, $tw2) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
try { "live: " + (Invoke-RestMethod http://127.0.0.1:7890/api/health -TimeoutSec 3 | ConvertTo-Json -Compress) } catch { "live: off" }
