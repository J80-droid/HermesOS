@echo off
setlocal enabledelayedexpansion
color 06
title Hermes Setup Wizard

:: ============================================================================
:: HERMES AGENT - SETUP WIZARD
:: ============================================================================

echo.
echo   _    _ ______ _____  __  __ ______  _____            _____ ______ _   _ _______ 
echo  ^| ^|  ^| ^|  ____^|  __ \^|  \/  ^|  ____^|/ ____^|     /\   / ____^|  ____^| \ ^| ^|__   __^|
echo  ^| ^|__^| ^| ^|__  ^| ^|__) ^| \  / ^| ^|__  ^| (___      /  \ ^| ^|  __^| ^|__  ^|  \^| ^|  ^| ^|   
echo  ^|  __  ^|  __^| ^|  _  /^| ^|\/^| ^|  __^|  \___ \    / /\ \^| ^| ^|_ ^|  __^| ^| . ` ^|  ^| ^|   
echo  ^| ^|  ^| ^| ^|____^| ^| \ \^| ^|  ^| ^| ^|____ ____) ^|  / ____ \ ^|__^| ^| ^|____^| ^|\  ^|  ^| ^|   
echo  ^|_^|  ^|_^|______^|_^|  \_\_^|  ^|_^|______^|_____/  /_/    \_\_____^|______^|_^| \_^|  ^|_^|   
echo.
echo  [ Setup Wizard - Initializing environment ]
echo.

:: Step 1: Python Venv
echo [1/3] Creating Python Virtual Environment...
if not exist .venv (
    python -m venv .venv
)
call .venv\Scripts\activate.bat

echo Installing Python dependencies...
python -m pip install --upgrade pip
pip install -e .

:: Step 2: Frontend Dependencies
echo [2/3] Installing Frontend dependencies...
if exist HermesOS (
    cd HermesOS
    call pnpm install
    cd ..
) else (
    echo [ERROR] HermesOS directory not found.
    pause
    exit /b 1
)

:: Step 3: Docker Images
echo [3/3] Pulling Docker images...
where docker >nul 2>&1
if %errorLevel% == 0 (
    docker-compose pull
) else (
    echo [WARNING] Docker not found. Skipping image pull.
)

echo.
echo [COMPLETE] Setup finished successfully.
echo You can now run START-agent.bat.
echo.
pause
