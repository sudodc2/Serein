@echo off
rem Runs Aethon with its data beside this file. Security software can deny an unsigned build any
rem write inside the user profile (ERROR_ACCESS_DENIED), and then the cache, saved logins and the
rem diagnostic log all stay read-only. Put this file next to aethon.exe and start Aethon with it.
setlocal
set "ROOT=%~dp0"
if not exist "%ROOT%aethon.exe" (
	echo Put this file next to aethon.exe.
	exit /b 1
)
if not exist "%ROOT%data" mkdir "%ROOT%data"
set "AETHON_DATA_DIR=%ROOT%data"
start "" "%ROOT%aethon.exe" %*
endlocal
