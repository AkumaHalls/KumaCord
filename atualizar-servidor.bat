@echo off
rem Atualiza o servidor oficial do concord para a versao indicada (padrao: v1.0.2) e o liga.
rem Uso: feche antes a janela do servidor que estiver aberta, depois de dois cliques aqui.
rem Para voltar a versao anterior: atualizar-servidor.bat v1.0.1
rem Desde a v1.0.1 o programa se chama concord-server.exe (antes: voxhub-server.exe). As versoes
rem antigas continuam funcionando: elas voltam a ser o bin\voxhub-server.exe.
cd /d "%~dp0"
set VERSAO=%1
if "%VERSAO%"=="" set VERSAO=v1.0.2
for %%P in (concord-server.exe voxhub-server.exe) do (
  tasklist /fi "imagename eq %%P" 2>nul | find /i "%%P" >nul && (
    echo O servidor ainda esta aberto: %%P
    echo Feche a janela do servidor e rode este arquivo de novo.
    pause
    exit /b 1
  )
)
if exist releases\%VERSAO%\concord-server.exe (
  copy /y releases\%VERSAO%\concord-server.exe bin\concord-server.exe >nul
) else if exist releases\%VERSAO%\voxhub-server.exe (
  rem Uma versao de antes do nome concord: ela volta a ser o bin\voxhub-server.exe, e o
  rem concord-server.exe sai de bin\ para o iniciar-servidor.bat usar a versao escolhida.
  copy /y releases\%VERSAO%\voxhub-server.exe bin\voxhub-server.exe >nul
  if not errorlevel 1 if exist bin\concord-server.exe del bin\concord-server.exe
) else (
  echo Versao %VERSAO% nao encontrada em releases\%VERSAO%\
  pause
  exit /b 1
)
if errorlevel 1 (
  echo.
  echo Nao consegui substituir o servidor: ele ainda esta aberto.
  echo Feche a janela do servidor antigo e rode este arquivo de novo.
  pause
  exit /b 1
)
echo Servidor atualizado para %VERSAO%. Ligando...
call "%~dp0iniciar-servidor.bat"
