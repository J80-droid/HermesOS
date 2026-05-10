@echo off
cd /d "%~dp0"
set "TASK_NAME=HermesAgentLaunch"
set "SCRIPT_PATH=%~dp0START-agent.bat"

echo [SYS] Creating zero-click Administrator shortcut...

:: 1. Verwijder oude taak indien aanwezig
schtasks /delete /tn "%TASK_NAME%" /f >nul 2>&1

:: 2. Maak de nieuwe taak aan met hoogste privileges
:: We gebruiken de huidige gebruiker expliciet om UAC bypass te maximaliseren
schtasks /create /tn "%TASK_NAME%" /tr "\"%SCRIPT_PATH%\"" /sc ONCE /st 00:00 /rl HIGHEST /f /it

if %errorLevel% equ 0 (
    echo [OK] Scheduled Task created.
    
    :: 3. Maak een VBScript om een snelkoppeling op het bureaublad te plaatsen
    set "VBS_FILE=%temp%\shortcut.vbs"
    echo Set oWS = WScript.CreateObject("WScript.Shell") > "%VBS_FILE%"
    echo sLinkFile = oWS.SpecialFolders("Desktop") ^& "\Hermes Command Center.lnk" >> "%VBS_FILE%"
    echo Set oLink = oWS.CreateShortcut(sLinkFile) >> "%VBS_FILE%"
    echo oLink.TargetPath = "C:\Windows\System32\schtasks.exe" >> "%VBS_FILE%"
    echo oLink.Arguments = "/run /tn ""%TASK_NAME%""" >> "%VBS_FILE%"
    echo oLink.Description = "Start Hermes Agent with zero-click Admin rights" >> "%VBS_FILE%"
    echo oLink.IconLocation = "C:\Windows\System32\shell32.dll,24" >> "%VBS_FILE%"
    echo oLink.Save >> "%VBS_FILE%"
    
    cscript //nologo "%VBS_FILE%"
    del "%VBS_FILE%"
    
    echo [COMPLETE] Snelkoppeling 'Hermes Command Center' staat op je bureaublad.
    echo Vanaf nu kun je deze gebruiken om te starten ZONDER UAC-pop-ups.
) else (
    echo [ERROR] Kon de taak niet aanmaken. Voer dit script eenmalig uit als Administrator.
)

timeout /t 5
exit
