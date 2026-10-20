@echo off
rem Liga o servidor do concord. Feche esta janela ou aperte Ctrl+C para desligar.
rem Alguns segundos depois de ligar, ele mostra um diagnostico de conexao (IPv4/IPv6, roteador,
rem Firewall do Windows) com o que fazer se os amigos nao conseguirem entrar.
cd /d "%~dp0"
if not exist bin\livekit-server.exe (
  echo Falta a peca de voz. Rode antes o baixar-livekit.bat.
  pause
  exit /b 1
)
echo Dica: na primeira vez, rode tambem o liberar-firewall.bat (libera o servidor no Firewall do Windows).
bin\concord-server.exe --data-dir data --livekit-bin bin\livekit-server.exe --console
pause
