@echo off
rem Libera o servidor do concord no Firewall do Windows, em TODAS as redes (Dominio, Privada e
rem Publica), para IPv4 e IPv6. Cria 3 regras de entrada, todas chamadas "concord servidor (...)":
rem   - bin\concord-server.exe: TCP 7890 (o servidor);
rem   - bin\livekit-server.exe: TCP 7880 e 7881 e UDP 7882 (a voz).
rem Se data\config.toml usar outras portas, elas sao lidas de la.
rem Bloqueios que o proprio Windows criou para esses dois programas (um bloqueio vence qualquer
rem liberacao) sao desligados, nunca apagados.
rem Rodar de novo substitui as regras (nao duplica). Para desfazer: remover-firewall.bat.
rem Teste sem mudar nada (nao pede administrador): liberar-firewall.bat /teste
setlocal
cd /d "%~dp0"
set "CONCORD_FW_ACAO=liberar"
set "CONCORD_FW_BAT=%~f0"
set "CONCORD_FW_PASTA=%~dp0"
set "CONCORD_FW_MODO=aplicar"
if /i "%~1"=="/teste" set "CONCORD_FW_MODO=teste"
if /i "%~1"=="/teste" goto rodar
if /i "%~1"=="/elevado" goto rodar
echo.
echo  LIBERAR O SERVIDOR DO CONCORD NO FIREWALL DO WINDOWS
echo  ====================================================
echo.
echo  O que este arquivo faz:
echo  - cria 3 regras de ENTRADA no Firewall do Windows, chamadas "concord servidor (...)";
echo  - elas deixam entrar conexoes so para os programas desta pasta:
echo      bin\concord-server.exe  (servidor: TCP 7890)
echo      bin\livekit-server.exe  (voz: TCP 7880 e 7881, UDP 7882)
echo  - valem em todas as redes (Dominio, Privada e Publica), em IPv4 e IPv6;
echo  - se o proprio Windows tiver criado um BLOQUEIO para esses dois programas
echo    (por exemplo, quando a pergunta dele foi cancelada), ele e desligado, nao apagado;
echo  - rodar de novo substitui as regras; remover-firewall.bat desfaz tudo.
echo.
echo  Nada mais muda: nenhuma outra regra, nenhuma porta do roteador.
echo  Para mudar o Firewall o Windows pede permissao de administrador:
echo  depois de apertar uma tecla, clique em "Sim" na janela do Windows.
echo  (Para desistir, feche esta janela.)
echo.
pause
net session >nul 2>&1
if not errorlevel 1 goto rodar
powershell -NoProfile -ExecutionPolicy Bypass -Command "Start-Process -FilePath $env:CONCORD_FW_BAT -ArgumentList '/elevado' -Verb RunAs"
if errorlevel 1 (
  echo.
  echo O Windows nao deu permissao de administrador: nada foi mudado.
  pause
)
exit /b
:rodar
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s = [IO.File]::ReadAllText($env:CONCORD_FW_BAT); Invoke-Expression $s.Substring($s.IndexOf('#' + 'POWERSHELL' + '#'))"
set "RESULTADO=%errorlevel%"
echo.
pause
exit /b %RESULTADO%
#POWERSHELL# The part below runs in PowerShell (the batch file never gets here).
$ErrorActionPreference = 'Stop'
$teste = $env:CONCORD_FW_MODO -eq 'teste'
$remover = $env:CONCORD_FW_ACAO -eq 'remover'
$pasta = $env:CONCORD_FW_PASTA.TrimEnd('\')
$srv = Join-Path $pasta 'bin\concord-server.exe'
$lk = Join-Path $pasta 'bin\livekit-server.exe'
$hub = 7890; $sinal = 7880; $tcp = 7881; $udp = 7882
$cfg = Join-Path $pasta 'data\config.toml'
if (Test-Path -LiteralPath $cfg) {
  $t = [IO.File]::ReadAllText($cfg)
  if ($t -match '(?m)^\s*bind\s*=\s*"[^"]*:(\d+)"') { $hub = [int]$Matches[1] }
  if ($t -match '(?m)^\s*signal_port\s*=\s*(\d+)') { $sinal = [int]$Matches[1] }
  if ($t -match '(?m)^\s*rtc_tcp_port\s*=\s*(\d+)') { $tcp = [int]$Matches[1] }
  if ($t -match '(?m)^\s*rtc_udp_port\s*=\s*(\d+)') { $udp = [int]$Matches[1] }
}
$regras = @(
  @{ Nome = "concord servidor (servidor TCP $hub)"; Programa = $srv; Protocolo = 'TCP'; Portas = @("$hub") },
  @{ Nome = "concord servidor (voz TCP $sinal e $tcp)"; Programa = $lk; Protocolo = 'TCP'; Portas = @("$sinal", "$tcp") },
  @{ Nome = "concord servidor (voz UDP $udp)"; Programa = $lk; Protocolo = 'UDP'; Portas = @("$udp") }
)
# Block rules Windows itself made for these two programs (when its question was cancelled, or for
# the kind of network left unchecked) win over any allow rule, so they are switched off (never
# deleted) and marked; remover-firewall.bat switches them back on.
$marca = '[desativada pelo liberar-firewall.bat do concord]'
function Bloqueios {
  foreach ($f in @(Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue)) {
    $p = [Environment]::ExpandEnvironmentVariables([string]$f.Program)
    if ($p -ieq $srv -or $p -ieq $lk) {
      $f | Get-NetFirewallRule | Where-Object { $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' -and $_.Enabled -eq 'True' -and $_.Group -ne 'concord servidor' }
    }
  }
}
function Marcadas {
  @(Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object { ([string]$_.Description).StartsWith($marca) })
}
if ($teste) { Write-Host 'MODO TESTE: nada e mudado, so mostra o que seria feito.' }
try {
  $velhas = @(Get-NetFirewallRule -DisplayName 'concord servidor (*' -ErrorAction SilentlyContinue)
  foreach ($v in $velhas) {
    if ($remover) { Write-Host "Apagando a regra: $($v.DisplayName)" } else { Write-Host "Substituindo a regra antiga: $($v.DisplayName)" }
    $v | Remove-NetFirewallRule -WhatIf:$teste
  }
  if ($remover) {
    $marcadas = Marcadas
    foreach ($m in $marcadas) {
      Write-Host "Religando o bloqueio que o Windows tinha criado: $($m.DisplayName) ($($m.Profile))"
      $antes = ([string]$m.Description).Substring($marca.Length).TrimStart()
      if (-not $antes) { $antes = [string]$m.DisplayName }
      $m | Set-NetFirewallRule -Enabled True -Description $antes -WhatIf:$teste
    }
    if ($velhas.Count + $marcadas.Count -eq 0) { Write-Host 'Nenhuma regra do concord encontrada: nada a desfazer.' }
    elseif ($teste) { Write-Host 'MODO TESTE: nenhuma regra foi apagada ou religada.' }
    else { Write-Host "Pronto: $($velhas.Count) regra(s) apagada(s), $($marcadas.Count) bloqueio(s) religado(s)." }
    exit 0
  }
  foreach ($r in $regras) {
    Write-Host "Criando: $($r.Nome)"
    Write-Host "  entrada, permitir, $($r.Protocolo) $($r.Portas -join ' e '), todas as redes (Dominio, Privada, Publica), IPv4 e IPv6"
    Write-Host "  programa: $($r.Programa)"
    New-NetFirewallRule -DisplayName $r.Nome -Group 'concord servidor' -Description 'Criada pelo liberar-firewall.bat do concord. Para apagar: remover-firewall.bat.' -Direction Inbound -Action Allow -Profile Any -Program $r.Programa -Protocol $r.Protocolo -LocalPort $r.Portas -WhatIf:$teste | Out-Null
  }
  foreach ($b in @(Bloqueios)) {
    Write-Host "Desligando um bloqueio que o Windows criou para este programa: $($b.DisplayName) ($($b.Profile))"
    $b | Set-NetFirewallRule -Enabled False -Description "$marca $($b.Description)" -WhatIf:$teste
  }
  Write-Host ''
  if ($teste) { Write-Host 'MODO TESTE: nenhuma regra foi criada, apagada ou desligada.'; exit 0 }
  Write-Host 'Pronto: o servidor e a voz estao liberados no Firewall do Windows em todas as redes (IPv4 e IPv6).'
  Write-Host 'Os amigos ainda precisam alcancar este PC pela internet: IPv6, portas abertas no roteador ou VPN (veja o LEIA-ME-SERVIDOR.txt).'
} catch {
  Write-Host ''
  Write-Host "Nao deu certo: $($_.Exception.Message)"
  exit 1
}
