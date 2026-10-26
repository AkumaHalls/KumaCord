; AkumaLabs - instalador NSIS (per-user, sem administrador).
;
; Build (veja installer\release-assets.ps1, que passa os /D; rode o makensis
; a partir da RAIZ do repositorio, que os caminhos relativos daqui dependem disso):
;   makensis /DVERSION=1.0.3 /DINPUT_EXE=target/release/AkumaLabs.exe /DOUT_FILE=dist\AkumaLabs-1.0.3-setup.exe installer\AkumaLabs.nsi
;
; UI classica do NSIS (o MUI2 trava o makensis do Linux com ICOs modernos, e
; quase toda instalacao aqui e silenciosa: /S no fluxo do auto-update).
; Atualizacao silenciosa: setup.exe /S - instala e REABRE o app.
; Instalacao interativa: pasta + progresso + pergunta de abrir no fim.
; OBS: os caminhos completos dos arquivos de entrada chegam PRONTOS via /D (o
; PowerShell do release-assets.ps1 monta cada um no separador nativo do OS) -
; nao se junta separador nenhum aqui, que o NSIS do Windows rejeita path
; misto (backslash + "/") no comando File.

Unicode true
ManifestDPIAware true

!ifndef VERSION
  !error "passe /DVERSION=<a.b.c>"
!endif
!ifndef INPUT_EXE
  !error "passe /DINPUT_EXE=<caminho absoluto do AkumaLabs.exe>"
!endif
!ifndef OUT_FILE
  !error "passe /DOUT_FILE=<caminho absoluto do setup.exe>"
!endif
!ifndef ICON_FILE
  !error "passe /DICON_FILE=<caminho absoluto do akumalabs-setup.ico>"
!endif
!ifndef LEIAME_FILE
  !error "passe /DLEIAME_FILE=<caminho absoluto do LEIA-ME.txt>"
!endif

!define APPNAME "AkumaLabs"
!define COMPANY "AkumaLabs"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\AkumaLabs"

Name "${APPNAME} ${VERSION}"
OutFile "${OUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\AkumaLabs"
InstallDirRegKey HKCU "Software\${APPNAME}" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma

Icon "${ICON_FILE}"
UninstallIcon "${ICON_FILE}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "${COMPANY}"
VIAddVersionKey "FileDescription" "Instalador do ${APPNAME}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Baseado no projeto concord (MIT). Marca e logo AkumaLabs."

!include "LogicLib.nsh"
!include "FileFunc.nsh"

LoadLanguageFile "${NSISDIR}\Contrib\Language files\PortugueseBR.nlf"

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Instalar"
  SetOutPath "$INSTDIR"
  ; Uma instância aberta travaria a troca do exe: fecha antes (best effort).
  nsExec::Exec 'taskkill /IM AkumaLabs.exe /F'
  Pop $0

  File /oname=AkumaLabs.exe "${INPUT_EXE}"
  File /oname=LEIA-ME.txt "${LEIAME_FILE}"

  WriteUninstaller "$INSTDIR\Uninstall.exe"

  WriteRegStr HKCU "Software\${APPNAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "${COMPANY}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\AkumaLabs.exe"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IfErrors +2
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"

  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortCut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\AkumaLabs.exe"
  CreateShortCut "$SMPROGRAMS\${APPNAME}\Desinstalar ${APPNAME}.lnk" "$INSTDIR\Uninstall.exe"
  CreateShortCut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\AkumaLabs.exe"
SectionEnd

Function .onInstSuccess
  ${If} ${Silent}
    ; No fluxo de atualização (/S) não há página de fim: reabre o app aqui.
    Exec "$INSTDIR\AkumaLabs.exe"
  ${Else}
    MessageBox MB_YESNO|MB_ICONQUESTION "O ${APPNAME} foi instalado.$\nAbrir agora?" /SD IDYES IDNO skip
    Exec "$INSTDIR\AkumaLabs.exe"
    skip:
  ${EndIf}
FunctionEnd

Function un.onInit
  ; Desinstalar com o app aberto também não funciona.
  nsExec::Exec 'taskkill /IM AkumaLabs.exe /F'
  Pop $0
FunctionEnd

Section "Uninstall"
  Delete "$INSTDIR\AkumaLabs.exe"
  Delete "$INSTDIR\LEIA-ME.txt"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Desinstalar ${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"
  Delete "$DESKTOP\${APPNAME}.lnk"
  DeleteRegKey HKCU "${UNINSTKEY}"
  DeleteRegValue HKCU "Software\${APPNAME}" "InstallDir"
  ; Os dados do usuário (%APPDATA%\AkumaLabs) ficam; apague na mão se quiser.
SectionEnd
