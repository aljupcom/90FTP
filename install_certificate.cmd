@echo off
echo ========================================================
echo   90FTP - Install Publisher Certificate (aljailane)
echo ========================================================
echo.
echo Installing aljailane certificate into Trusted Root Store...
certutil -addstore -user Root "assets\certs\aljailane.crt"
echo.
if %ERRORLEVEL% EQU 0 (
    echo [SUCCESS] Certificate installed! Windows will now verify 'aljailane' as a trusted publisher.
) else (
    echo [NOTICE] Certificate installation was canceled or requires confirmation.
)
echo.
pause
