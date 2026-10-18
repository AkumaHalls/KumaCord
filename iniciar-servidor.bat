@echo off
rem Inicia o servidor do concord (chat + voz). Feche esta janela ou aperte Ctrl+C para desligar.
rem Este e o servidor "oficial" (versao estavel em bin\). Builds de desenvolvimento nao sao usados aqui.
rem Desde a v1.0.1 o programa se chama concord-server.exe; sem ele, usa o antigo voxhub-server.exe.
cd /d "%~dp0"
set SERVIDOR=bin\concord-server.exe
if not exist %SERVIDOR% set SERVIDOR=bin\voxhub-server.exe
if not exist %SERVIDOR% (
  echo Nao encontrei bin\concord-server.exe nem bin\voxhub-server.exe.
  echo Rode atualizar-servidor.bat para copiar o servidor de releases\.
  pause
  exit /b 1
)
%SERVIDOR% --data-dir data --livekit-bin bin\livekit-server.exe --console
pause
