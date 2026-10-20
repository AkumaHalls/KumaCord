@echo off
rem Baixa a peca de voz (LiveKit 1.13.7 oficial, ~19 MB) e confere a impressao digital (SHA-256).
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "hub\scripts\fetch-livekit.ps1"
pause
