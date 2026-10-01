@echo off
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0loom.ps1" %*
exit /b %errorlevel%
