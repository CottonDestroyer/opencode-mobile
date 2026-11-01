# Cross-compiles the app for Android with the NDK. Every variable is set for this process only.
#   .\scripts\build-android.ps1                            # cargo build --lib for arm64, debug
#   .\scripts\build-android.ps1 -Arch x86_64               # for the x86_64 emulator
#   .\scripts\build-android.ps1 -Release
#   .\scripts\build-android.ps1 -Cargo "apk build --lib"   # any cargo command, e.g. cargo-apk
param(
    [ValidateSet("arm64", "x86_64")][string]$Arch = "arm64",
    # Slint's Android backend compiles Java against android.jar and runs d8, so the SDK is needed
    # even to compile.
    [string]$Sdk = $(if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { "$env:LOCALAPPDATA\Android\Sdk" }),
    # Defaults to ANDROID_NDK_HOME, else the newest NDK installed through the SDK Manager.
    [string]$Ndk = $env:ANDROID_NDK_HOME,
    [int]$Api = 26,  # minimum supported by Slint's Android backend
    [switch]$Release,
    [string]$Cargo = "build --lib"
)
$ErrorActionPreference = "Stop"
if (-not $Ndk) {
    $Ndk = Get-ChildItem "$Sdk\ndk" -Directory -ErrorAction SilentlyContinue |
        Sort-Object { [version]($_.Name -replace '[^0-9.].*$', '') } | Select-Object -Last 1 -ExpandProperty FullName
}
if (-not $Ndk) { throw "No NDK found: set ANDROID_NDK_HOME, pass -Ndk, or install one with the SDK Manager" }
$target, $abi = if ($Arch -eq "arm64") { "aarch64-linux-android", "arm64-v8a" } else { "x86_64-linux-android", "x86_64" }
$t = $target -replace '-', '_'           # suffix for target-specific variables
$T = $t.ToUpper()
$bin = Join-Path $Ndk "toolchains\llvm\prebuilt\windows-x86_64\bin"
if (-not (Test-Path "$bin\$target$Api-clang.cmd")) { throw "NDK clang not found under $bin" }
if (-not (Test-Path "$Sdk\platforms")) { Write-Warning "Android SDK not found at $Sdk (pass -Sdk or set ANDROID_HOME)" }

# SDK Manager now installs platforms as `platforms/android-37.0`, but cargo-apk only recognizes
# `android-<int>`. Expose the SDK through a shim of junctions under target/ that adds the integer
# names, leaving the real SDK untouched.
if (Test-Path "$Sdk\platforms") {
    $shim = Join-Path (Split-Path $PSScriptRoot -Parent) "target\android-sdk-shim"
    New-Item -ItemType Directory -Force "$shim\platforms" | Out-Null
    foreach ($d in Get-ChildItem $Sdk -Directory | Where-Object { $_.Name -ne "platforms" }) {
        if (-not (Test-Path "$shim\$($d.Name)")) { New-Item -ItemType Junction -Path "$shim\$($d.Name)" -Target $d.FullName | Out-Null }
    }
    foreach ($p in Get-ChildItem "$Sdk\platforms" -Directory) {
        $name = $p.Name -replace '^(android-\d+)\.0$', '$1'
        if (-not (Test-Path "$shim\platforms\$name")) { New-Item -ItemType Junction -Path "$shim\platforms\$name" -Target $p.FullName | Out-Null }
    }
    $Sdk = $shim
}

$env:ANDROID_HOME = $Sdk
$env:ANDROID_SDK_ROOT = $Sdk
$env:ANDROID_NDK_HOME = $Ndk
$env:ANDROID_NDK_ROOT = $Ndk
$env:ANDROID_TOOLCHAIN_ABI = $abi        # read by android.toolchain.cmake
$env:ANDROID_TOOLCHAIN_API = "$Api"
Set-Item "env:CC_$t" "$bin\$target$Api-clang.cmd"
Set-Item "env:CXX_$t" "$bin\$target$Api-clang++.cmd"
Set-Item "env:AR_$t" "$bin\llvm-ar.exe"
Set-Item "env:CARGO_TARGET_${T}_LINKER" "$bin\$target$Api-clang.cmd"
# BoringSSL: the Visual Studio generator can't target Android, so use Ninja (bundled with VS), with
# the NDK toolchain wrapped to pin the ABI and API level.
Set-Item "env:CMAKE_GENERATOR_$t" "Ninja"
Set-Item "env:CMAKE_TOOLCHAIN_FILE_$t" ((Join-Path $PSScriptRoot "android.toolchain.cmake") -replace '\\', '/')
# bindgen for btls-sys uses .tools/libclang, which lacks clang's builtin headers (stddef.h and
# friends), and the NDK headers require a versioned target triple.
$llvm = Split-Path $bin -Parent
$clangInc = (Get-ChildItem "$llvm\lib\clang" -Directory | Select-Object -First 1).FullName + "\include"
Set-Item "env:BINDGEN_EXTRA_CLANG_ARGS_$t" "--target=$target$Api -isystem `"$($clangInc -replace '\\','/')`" --sysroot=`"$(($llvm + '\sysroot') -replace '\\','/')`""
$ninja = "C:\Program Files\Microsoft Visual Studio\18\Professional\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja"
$env:PATH = "$ninja;$env:PATH"

# cargo-apk signs release APKs with CARGO_APK_RELEASE_KEYSTORE(_PASSWORD). Unless those are set, use the
# keystore kept outside the repo in ~/.android, with its password in a file alongside it.
$keystore = "$env:USERPROFILE\.android\opencode-mobile-release.jks"
if ($Release -and -not $env:CARGO_APK_RELEASE_KEYSTORE -and (Test-Path $keystore)) {
    $env:CARGO_APK_RELEASE_KEYSTORE = $keystore
    $env:CARGO_APK_RELEASE_KEYSTORE_PASSWORD = (Get-Content "$env:USERPROFILE\.android\opencode-mobile-release.password" -Raw).Trim()
}

$cargoArgs = @($Cargo -split '\s+' | Where-Object { $_ }) + @("--target", $target)
if ($Release) { $cargoArgs += "--release" }
Write-Host "cargo $($cargoArgs -join ' ')"
$ErrorActionPreference = "Continue"  # cargo reports progress on stderr
& cargo @cargoArgs
exit $LASTEXITCODE
