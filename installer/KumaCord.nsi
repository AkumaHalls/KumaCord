; KumaCord - instalador NSIS (per-user, sem administrador).
;
; Build (veja installer\release-assets.ps1, que passa os /D; rode o makensis
; a partir da RAIZ do repositorio, que os caminhos relativos daqui dependem disso):
;   makensis /DVERSION=1.2.0 /DINPUT_EXE=target/release/KumaCord.exe /DOUT_FILE=dist\KumaCord-1.2.0-setup.exe installer\KumaCord.nsi
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
  !error "passe /DINPUT_EXE=<caminho absoluto do KumaCord.exe>"
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
!ifndef INTRO_PACK
  !error "passe /DINTRO_PACK=<caminho absoluto do intro.pack>"
!endif
!ifndef INTRO_MP3
  !error "passe /DINTRO_MP3=<caminho absoluto do intro.mp3>"
!endif

; The app is KumaCord; the community (and its server) kept the AkumaLabs name.
!define APPNAME "KumaCord"
!define COMPANY "AkumaLabs"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\KumaCord"
; v1.1.9 and older installed as "AkumaLabs" (exe, folder, shortcuts). The
; upgrade from one of those cleans the old layout up (see the install section).
!define OLDAPP "AkumaLabs"

Name "${APPNAME} ${VERSION}"
OutFile "${OUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\KumaCord"
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
UninstPage uninstfiles

Section "Instalar"
  ; Uma instancia aberta travaria a troca do exe: fecha antes (best effort).
  nsExec::Exec 'taskkill /IM ${APPNAME}.exe /F'
  Pop $0
  nsExec::Exec 'taskkill /IM ${OLDAPP}.exe /F'
  Pop $0

  ; v1.2: the app was renamed. Remove the old install in place (the user data
  ; in %APPDATA% and %LOCALAPPDATA% is NOT touched) so no stale 1.1.9 binary,
  ; shortcut or "Apps & features" entry is left behind.
  ReadRegStr $0 HKCU "Software\${OLDAPP}" "InstallDir"
  ${If} $0 != ""
    Delete "$0\${OLDAPP}.exe"
    Delete "$0\Uninstall.exe"
    Delete "$0\LEIA-ME.txt"
    Delete "$0\intro\intro.pack"
    Delete "$0\intro\intro.mp3"
    RMDir "$0\intro"
    RMDir "$0"
  ${EndIf}
  Delete "$LOCALAPPDATA\Programs\${OLDAPP}\${OLDAPP}.exe"
  Delete "$LOCALAPPDATA\Programs\${OLDAPP}\Uninstall.exe"
  Delete "$LOCALAPPDATA\Programs\${OLDAPP}\LEIA-ME.txt"
  Delete "$LOCALAPPDATA\Programs\${OLDAPP}\intro\intro.pack"
  Delete "$LOCALAPPDATA\Programs\${OLDAPP}\intro\intro.mp3"
  RMDir "$LOCALAPPDATA\Programs\${OLDAPP}\intro"
  RMDir "$LOCALAPPDATA\Programs\${OLDAPP}"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${OLDAPP}"
  DeleteRegKey HKCU "Software\${OLDAPP}"
  Delete "$SMPROGRAMS\${OLDAPP}\${OLDAPP}.lnk"
  Delete "$SMPROGRAMS\${OLDAPP}\Desinstalar ${OLDAPP}.lnk"
  RMDir "$SMPROGRAMS\${OLDAPP}"
  Delete "$DESKTOP\${OLDAPP}.lnk"

  SetOutPath "$INSTDIR"
  File /oname=${APPNAME}.exe "${INPUT_EXE}"
  File /oname=LEIA-ME.txt "${LEIAME_FILE}"

  ; The cinematic and its soundtrack live next to the exe (the app streams the
  ; frames from disk; nothing of it is inside the exe).
  SetOutPath "$INSTDIR\intro"
  File /oname=intro.pack "${INTRO_PACK}"
  File /oname=intro.mp3 "${INTRO_MP3}"
  SetOutPath "$INSTDIR"

  WriteUninstaller "$INSTDIR\Uninstall.exe"

  WriteRegStr HKCU "Software\${APPNAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "${COMPANY}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\${APPNAME}.exe"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IfErrors +2
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"

  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortCut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\${APPNAME}.exe"
  CreateShortCut "$SMPROGRAMS\${APPNAME}\Desinstalar ${APPNAME}.lnk" "$INSTDIR\Uninstall.exe"
  CreateShortCut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\${APPNAME}.exe"
SectionEnd

Function .onInstSuccess
  ${If} ${Silent}
    ; No fluxo de atualizacao (/S) nao ha pagina de fim: reabre o app aqui.
    Exec "$INSTDIR\${APPNAME}.exe"
  ${Else}
    MessageBox MB_YESNO|MB_ICONQUESTION "O ${APPNAME} foi instalado.$\nAbrir agora?" /SD IDYES IDNO skip
    Exec "$INSTDIR\${APPNAME}.exe"
    skip:
  ${EndIf}
FunctionEnd

Function un.onInit
  ; Desinstalar com o app aberto tambem nao funciona.
  nsExec::Exec 'taskkill /IM ${APPNAME}.exe /F'
  Pop $0
FunctionEnd

Section "Uninstall"
  Delete "$INSTDIR\${APPNAME}.exe"
  Delete "$INSTDIR\LEIA-ME.txt"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$INSTDIR\intro\intro.pack"
  Delete "$INSTDIR\intro\intro.mp3"
  RMDir "$INSTDIR\intro"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Desinstalar ${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"
  Delete "$DESKTOP\${APPNAME}.lnk"
  DeleteRegKey HKCU "${UNINSTKEY}"
  DeleteRegValue HKCU "Software\${APPNAME}" "InstallDir"
  ; Os dados do usuario (%APPDATA%\AkumaLabs) ficam; apague na mao se quiser.
SectionEnd
