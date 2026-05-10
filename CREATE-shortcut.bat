@echo off
cd /d "%~dp0"
chcp 65001 >nul

:: Admin Check & Auto-Elevation
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo [SYS] Requesting Administrator privileges to register task...
    powershell -Command "Start-Process '%~f0' -Verb RunAs"
    exit /b
)

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
    
    :: 3. Maak de snelkoppeling via PowerShell (robuuster dan VBScript)
    echo [SYS] Placing shortcut on desktop...
    powershell -Command "$s=(New-Object -ComObject WScript.Shell).CreateShortcut([System.IO.Path]::Combine([Environment]::GetFolderPath('Desktop'), 'Hermes Command Center.lnk')); $s.TargetPath='C:\Windows\System32\schtasks.exe'; $s.Arguments='/run /tn \"%TASK_NAME%\"'; $s.Description='Start Hermes Agent with zero-click Admin rights'; $s.IconLocation='C:\Windows\System32\shell32.dll,24'; $s.Save()"
    
    if %errorLevel% equ 0 (
        echo [COMPLETE] Snelkoppeling 'Hermes Command Center' staat op je bureaublad.
        echo Vanaf nu kun je deze gebruiken om te starten ZONDER UAC-pop-ups.
    ) else (
        echo [ERROR] Kon de snelkoppeling niet aanmaken op het bureaublad.
    )
) else (
    echo [ERROR] Kon de taak niet aanmaken. Voer dit script eenmalig uit als Administrator.
)

timeout /t 5
exit
