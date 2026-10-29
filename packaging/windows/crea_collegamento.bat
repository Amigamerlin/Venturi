@echo off
rem Puts a "Venturi" shortcut on the Desktop / Crea il collegamento "Venturi" sul Desktop.
setlocal
cd /d "%~dp0"
rem The folder goes through an environment variable: a quote in it (D'Amico) would break the command.
set "VDIR=%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=(New-Object -ComObject WScript.Shell).CreateShortcut([Environment]::GetFolderPath('Desktop')+'\Venturi.lnk'); $s.TargetPath=$env:VDIR+'venturi.exe'; $s.WorkingDirectory=$env:VDIR; $s.IconLocation=$env:VDIR+'venturi-video.ico'; $s.Save()"
if errorlevel 1 goto err
echo Collegamento Venturi creato sul Desktop.
pause
exit /b 0
:err
echo Non sono riuscito a creare il collegamento.
pause
exit /b 1
