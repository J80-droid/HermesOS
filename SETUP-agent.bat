@echo off
chcp 65001 >nul
setlocal enabledelayedexpansion
color 06
title HERMES AGENT - SYSTEM SETUP

:HEADER
cls
echo.
echo           ____________________________________________________________________
echo          /                                                                    \
echo         ^|  C:\^> SETUP-agent.bat                                                ^|
echo         ^|  [SYS] Establishing secure uplink... DONE                            ^|
echo         ^|  [SYS] Initializing setup protocol...                                ^|
echo         ^|  [███████████████████████████░░░░░░] 78%%                             ^|
echo         ^|                                                                    ^|
echo         ^|  ██╗  ██╗███████╗██████╗ ███╗    ███╗███████╗███████╗                  ^|
echo         ^|  ██║  ██║██╔════╝██╔══██╗████╗ ████║██╔════╝██╔════╝                  ^|
echo         ^|  ███████║█████╗  ██████╔╝██╔████╔██║█████╗  ███████╗                  ^|
echo         ^|  ██╔══██║██╔══╝  ██╔══██╗██║╚██╔╝██║██╔══╝  ╚════██║                  ^|
echo         ^|  ██║  ██║███████╗██║  ██║██║ ╚═╝ ██║███████╗███████║                  ^|
echo         ^|  ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝╚══════╝                  ^|
echo         ^|                                                                    ^|
echo         ^|        █████╗  ██████╗  ███████╗███╗   ██╗████████╗                   ^|
echo         ^|       ██╔══██╗██╔════╝  ██╔════╝████╗  ██║╚══██╔══╝                   ^|
echo         ^|       ███████║██║  ███╗ █████╗   ██╔██╗ ██║   ██║                      ^|
echo         ^|       ██╔══██║██║   ██║ ██╔══╝  ██║╚██╗██║   ██║                      ^|
echo         ^|       ██║  ██║╚██████╔╝ ███████╗██║ ╚████║   ██║                      ^|
echo         ^|       ╚═╝  ╚═╝ ╚═════╝  ╚══════╝╚═╝  ╚═══╝   ╚═╝                      ^|
echo         ^|                                                                    ^|
echo         ^|                                                        [ * ]       ^|
echo          \_____________________________________________________________________/
echo                         \_________________________________________/
echo                  ___________________________________________________________
echo               _-'    .-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.  --- `-_
echo            _-'.-.-. .---.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.--.  .-.-.`-_
echo         _-'.-.-.-. .---.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-`__`. .-.-.-.`-_
echo      _-'.-.-.-.-. .-----.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-.-----. .-.-.-.-.`-_
echo   _-'.-.-.-.-.-. .---.-. .---------------------------------------. .-.---. .---.-.-.-.`-_
echo  :-----------------------------------------------------------------------------------------:
echo  `---._.-----------------------------------------------------------------------------._.---'
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
    docker-compose pull --ignore-pull-failures
) else (
    echo [WARNING] Docker not found. Skipping image pull.
)

echo.
echo [COMPLETE] Setup finished successfully.
echo You can now run START-agent.bat.
echo.
pause
