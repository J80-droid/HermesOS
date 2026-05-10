@echo off
cd /d "%~dp0"
chcp 65001 >nul
setlocal enabledelayedexpansion
color 06
title HERMES AGENT - SECURE UPLINK

:HEADER
cls
echo.
type branding_start.txt
echo.

:: Admin Check & Auto-Elevation
net session >nul 2>&1
if %errorLevel% neq 0 (
    if "%HERMES_ELEVATED%"=="1" (
        echo [ERROR] Elevation failed even after request. 
        pause
        exit /b 1
    )
    echo [SYS] Requesting Administrator privileges...
    set "HERMES_ELEVATED=1"
    powershell -Command "Start-Process '%~f0' -Verb RunAs"
    exit /b
)

:: Step 0: Clean Slate
echo [0] Initializing Clean-Slate Protocol...

:: Kill Apps
echo Killing conflicting processes...
taskkill /F /IM hermes_agent.exe /T 2>nul
taskkill /F /IM hermesos.exe /T 2>nul
powershell -Command "Get-Process python -ErrorAction SilentlyContinue | Where-Object { $_.CommandLine -like '*sidecar_entry.py*' } | Stop-Process -Force" 2>nul
timeout /t 2 >nul

:: Kill Ports
echo Reclaiming ports...
:: Port mapping: 5185 (Vite), 1420 (Tauri), 9119 (Web), 8001 (Gateway)
for %%p in (5185 1420 9119 8001) do (
    for /f "tokens=5" %%a in ('netstat -aon ^| findstr :%%p ^| findstr LISTENING 2^>nul') do (
        echo Killing process on port %%p (PID %%a)
        taskkill /F /PID %%a 2>nul
    )
)

:: Docker Reset
echo Resetting Docker environment...
docker-compose down 2>nul

:: Step 1: Checks
echo [1] Running dependency checks...

:: Tool checks
where docker >nul 2>&1 || (echo [ERROR] Docker not found. ^& pause ^& exit /b 1)
where python >nul 2>&1 || (echo [ERROR] Python not found. ^& pause ^& exit /b 1)
where pnpm >nul 2>&1 || (echo [ERROR] pnpm not found. ^& pause ^& exit /b 1)
where rustc >nul 2>&1 || (echo [ERROR] Rust (rustc) not found. ^& pause ^& exit /b 1)
echo [OK] All dependencies found.

:: Step 2: Infrastructure
echo [2] Preparing infrastructure...

if not exist .env (
    echo [.env] Missing. Creating from .env.example...
    copy .env.example .env
    echo.
    echo [IMPORTANT] Please update your .env file with the required API keys.
    echo.
    pause
)

echo Starting Docker services...
docker-compose up -d

:: Wait for healthy
echo Waiting for containers to be ready...
:docker_loop
set "all_ready=true"
for /f "tokens=*" %%i in ('docker ps -q') do (
    :: Check if container is running
    for /f "tokens=*" %%r in ('docker inspect --format="{{.State.Running}}" %%i 2^>nul') do (
        if "%%r"=="false" set "all_ready=false"
    )
    :: Check health if available
    for /f "tokens=*" %%j in ('docker inspect --format="{{if .State.Health}}{{.State.Health.Status}}{{else}}healthy{{end}}" %%i 2^>nul') do (
        if "%%j"=="unhealthy" (
            echo [ERROR] Container %%i is unhealthy. Check docker logs.
            pause
            exit /b 1
        )
        if not "%%j"=="healthy" if not "%%j"=="starting" set "all_ready=false"
    )
)
if "!all_ready!"=="false" (
    timeout /t 2 >nul
    goto docker_loop
)
echo [OK] Infrastructure is ready.

:: Step 3: Runtime Orchestratie
echo [3] Orchestrating runtimes...

:: Activate venv
if exist .venv\Scripts\activate.bat (
    call .venv\Scripts\activate.bat
) else if exist venv\Scripts\activate.bat (
    call venv\Scripts\activate.bat
) else (
    echo [WARNING] No virtual environment found. Running with global python.
)

:: Logging
if not exist logs mkdir logs

:: Launch App
:: Note: Tauri will automatically spawn the sidecar_entry.py process itself.
:: Manual sidecar launch is removed to prevent resource conflicts.
echo Launching Hermes App...
start "Hermes App" cmd /k "echo [%date% %time%] Starting App >> \"%~dp0logs\app.log\" & cd HermesOS ^&^& pnpm tauri dev 2>> \"%~dp0logs\app.log\""

echo.
echo [COMPLETE] Hermes Command Center is operational.
echo [SYS] Closing orchestrator in 3 seconds...
timeout /t 3 >nul
exit
