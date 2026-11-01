# Drives the app on a connected device over adb (USB or wireless debugging).
#   .\scripts\phone.ps1 install              # install the debug APK and launch it
#   .\scripts\phone.ps1 launch               # restart the app
#   .\scripts\phone.ps1 shot -Out x.png      # screenshot, plus a third-scale x-small.png
#   .\scripts\phone.ps1 tap -X 630 -Y 1400   # tap at device pixels
#   .\scripts\phone.ps1 text -Text 'hello'   # type into the focused field
#   .\scripts\phone.ps1 log                  # the app's logcat output since launch
param(
    [Parameter(Mandatory)][ValidateSet("install", "launch", "shot", "tap", "text", "log")][string]$Cmd,
    [string]$Out = "target\phone.png",
    [int]$X, [int]$Y,
    [string]$Text,
    [string]$Apk = "target\debug\apk\opencode-mobile.apk"
)
$adb = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe"
$pkg = "dev.opencodemobile.app"
$ErrorActionPreference = "Continue"
function Launch {
    & $adb shell am force-stop $pkg | Out-Null
    & $adb logcat -c | Out-Null
    & $adb shell monkey -p $pkg -c android.intent.category.LAUNCHER 1 2>&1 | Out-Null
}
switch ($Cmd) {
    "install" { & $adb install -r $Apk 2>&1 | Select-Object -Last 1; Launch }
    "launch" { Launch }
    "shot" {
        & $adb shell screencap -p /sdcard/oc-shot.png | Out-Null
        & $adb pull /sdcard/oc-shot.png $Out 2>&1 | Out-Null
        Add-Type -AssemblyName System.Drawing
        $i = [System.Drawing.Image]::FromFile((Resolve-Path $Out))
        $b = New-Object System.Drawing.Bitmap $i, ([int]($i.Width / 3)), ([int]($i.Height / 3))
        $small = [IO.Path]::ChangeExtension($Out, $null).TrimEnd('.') + "-small.png"
        $b.Save($small); $i.Dispose(); $b.Dispose()
        "$Out ($small)"
    }
    "tap" { & $adb shell input tap $X $Y }
    # `input text` encodes spaces as %s; single quotes keep the device shell from interpreting ; & |.
    "text" { & $adb shell ("input text '" + (($Text -replace ' ', '%s') -replace "'", "'\''") + "'") }
    "log" {
        $appPid = (& $adb shell pidof $pkg).Trim()
        & $adb logcat -d -v brief | Select-String -Pattern "\(\s*$appPid\)|RustStdoutStderr|AndroidRuntime|FATAL" | ForEach-Object { $_.Line }
    }
}
