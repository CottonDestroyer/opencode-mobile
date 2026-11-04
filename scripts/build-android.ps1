# Cross-compiles the app for Android with the NDK. Every variable is set for this process only.
# Runs on Windows PowerShell 5.1 and on PowerShell 7 (pwsh) on Windows, Linux and macOS.
#   .\scripts\build-android.ps1                            # cargo build --lib for arm64, debug
#   .\scripts\build-android.ps1 -Arch x86_64               # for the x86_64 emulator
#   .\scripts\build-android.ps1 -Release
#   .\scripts\build-android.ps1 -Cargo "apk build --lib"   # any cargo command, e.g. cargo-apk
param(
    [ValidateSet("arm64", "x86_64")][string]$Arch = "arm64",
    # Slint's Android backend compiles Java against android.jar and runs d8, so the SDK is needed
    # even to compile. Defaults to ANDROID_HOME, else Android Studio's default location.
    [string]$Sdk = $(if ($env:ANDROID_HOME) { $env:ANDROID_HOME }
        elseif ($env:OS -eq "Windows_NT") { Join-Path $env:LOCALAPPDATA "Android\Sdk" }
        elseif ($IsMacOS) { Join-Path $HOME "Library/Android/sdk" }
        else { Join-Path $HOME "Android/Sdk" }),
    # Defaults to ANDROID_NDK_HOME, else the newest NDK installed through the SDK Manager.
    [string]$Ndk = $env:ANDROID_NDK_HOME,
    [int]$Api = 26,  # minimum supported by Slint's Android backend
    [switch]$Release,
    [string]$Cargo = "build --lib"
)
$ErrorActionPreference = "Stop"
$onWindows = $env:OS -eq "Windows_NT"
$hostTag = if ($onWindows) { "windows-x86_64" } elseif ($IsMacOS) { "darwin-x86_64" } else { "linux-x86_64" }
$cmd = if ($onWindows) { ".cmd" } else { "" }
$exe = if ($onWindows) { ".exe" } else { "" }

if (-not $Ndk) {
    $Ndk = Get-ChildItem (Join-Path $Sdk "ndk") -Directory -ErrorAction SilentlyContinue |
        Sort-Object { [version]($_.Name -replace '[^0-9.].*$', '') } | Select-Object -Last 1 -ExpandProperty FullName
}
if (-not $Ndk) { throw "No NDK found: set ANDROID_NDK_HOME, pass -Ndk, or install one with the SDK Manager" }
$target, $abi = if ($Arch -eq "arm64") { "aarch64-linux-android", "arm64-v8a" } else { "x86_64-linux-android", "x86_64" }
$t = $target -replace '-', '_'           # suffix for target-specific variables
$T = $t.ToUpper()
$llvm = Join-Path $Ndk "toolchains/llvm/prebuilt/$hostTag"
$bin = Join-Path $llvm "bin"
$clang = Join-Path $bin "$target$Api-clang$cmd"
if (-not (Test-Path $clang)) { throw "NDK clang not found at $clang" }
$platforms = Join-Path $Sdk "platforms"
if (-not (Test-Path $platforms)) { Write-Warning "Android SDK not found at $Sdk (pass -Sdk or set ANDROID_HOME)" }

# SDK Manager now installs platforms as `platforms/android-37.0`, but cargo-apk only recognizes
# `android-<int>`. Expose the SDK through a shim of links under target/ that adds the integer
# names, leaving the real SDK untouched.
if (Test-Path $platforms) {
    $linkType = if ($onWindows) { "Junction" } else { "SymbolicLink" }
    $shim = Join-Path (Split-Path $PSScriptRoot -Parent) "target/android-sdk-shim"
    New-Item -ItemType Directory -Force (Join-Path $shim "platforms") | Out-Null
    foreach ($d in Get-ChildItem $Sdk -Directory | Where-Object { $_.Name -ne "platforms" }) {
        $link = Join-Path $shim $d.Name
        if (-not (Test-Path $link)) { New-Item -ItemType $linkType -Path $link -Target $d.FullName | Out-Null }
    }
    foreach ($p in Get-ChildItem $platforms -Directory) {
        $link = Join-Path $shim "platforms/$($p.Name -replace '^(android-\d+)\.0$', '$1')"
        if (-not (Test-Path $link)) { New-Item -ItemType $linkType -Path $link -Target $p.FullName | Out-Null }
    }
    $Sdk = $shim
}

# CMake and Ninja (BoringSSL): from PATH, else the copies bundled with Visual Studio.
function Find-Tool($name, $vsSubdir) {
    $found = Get-Command $name -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($found) { return Split-Path $found.Source -Parent }
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if ($onWindows -and (Test-Path $vswhere)) {
        $vs = & $vswhere -latest -products * -property installationPath
        if ($vs) {
            $dir = Join-Path $vs "Common7\IDE\CommonExtensions\Microsoft\CMake\$vsSubdir"
            if (Test-Path (Join-Path $dir "$name.exe")) { return $dir }
        }
    }
    throw "$name not found: install it and add it to PATH"
}
$toolDirs = @(Find-Tool "ninja" "Ninja")
if (-not $env:CMAKE) { $toolDirs += Find-Tool "cmake" "CMake\bin" }
$env:PATH = (($toolDirs | Select-Object -Unique) + $env:PATH) -join [IO.Path]::PathSeparator

$env:ANDROID_HOME = $Sdk
$env:ANDROID_SDK_ROOT = $Sdk
$env:ANDROID_NDK_HOME = $Ndk
$env:ANDROID_NDK_ROOT = $Ndk
$env:ANDROID_TOOLCHAIN_ABI = $abi        # read by android.toolchain.cmake
$env:ANDROID_TOOLCHAIN_API = "$Api"
Set-Item "env:CC_$t" $clang
Set-Item "env:CXX_$t" (Join-Path $bin "$target$Api-clang++$cmd")
Set-Item "env:AR_$t" (Join-Path $bin "llvm-ar$exe")
Set-Item "env:CARGO_TARGET_${T}_LINKER" $clang
# BoringSSL: the Visual Studio generator can't target Android, so use Ninja, with the NDK toolchain
# wrapped to pin the ABI and API level.
Set-Item "env:CMAKE_GENERATOR_$t" "Ninja"
Set-Item "env:CMAKE_TOOLCHAIN_FILE_$t" ((Join-Path $PSScriptRoot "android.toolchain.cmake") -replace '\\', '/')
# bindgen for btls-sys: use the NDK's own libclang, even over a LIBCLANG_PATH set for host builds.
# Another LLVM brings its own builtin headers, and two copies of stdint.h on the include path hide
# the sysroot's (its include guard makes the second copy a no-op), so uint32_t and friends go
# missing. The builtin headers are also passed explicitly, for a libclang without them, and the NDK
# headers require a versioned target triple.
$ndkLibclang = @($bin, (Join-Path $llvm "lib"), (Join-Path $llvm "lib64")) |
    Where-Object { Get-ChildItem $_ -Filter "libclang*" -File -ErrorAction SilentlyContinue } | Select-Object -First 1
if ($ndkLibclang) { $env:LIBCLANG_PATH = $ndkLibclang }
Write-Host "LIBCLANG_PATH=$env:LIBCLANG_PATH"
$clangInc = (Get-ChildItem (Join-Path $llvm "lib/clang") -Directory | Select-Object -First 1).FullName
$clangInc = (Join-Path $clangInc "include") -replace '\\', '/'
$sysroot = (Join-Path $llvm "sysroot") -replace '\\', '/'
Set-Item "env:BINDGEN_EXTRA_CLANG_ARGS_$t" "--target=$target$Api -isystem `"$clangInc`" --sysroot=`"$sysroot`""

# cargo-apk signs release APKs with CARGO_APK_RELEASE_KEYSTORE(_PASSWORD). Unless those are set, use
# ~/.android/opencode-mobile-release.jks if it exists, with its password in a .password file alongside.
$keystore = Join-Path $HOME ".android/opencode-mobile-release.jks"
if ($Release -and -not $env:CARGO_APK_RELEASE_KEYSTORE -and (Test-Path $keystore)) {
    $env:CARGO_APK_RELEASE_KEYSTORE = $keystore
    $env:CARGO_APK_RELEASE_KEYSTORE_PASSWORD = (Get-Content ([IO.Path]::ChangeExtension($keystore, ".password")) -Raw).Trim()
}

$cargoArgs = @($Cargo -split '\s+' | Where-Object { $_ }) + @("--target", $target)
if ($Release) { $cargoArgs += "--release" }
Write-Host "cargo $($cargoArgs -join ' ')"
$ErrorActionPreference = "Continue"  # cargo reports progress on stderr
& cargo @cargoArgs
exit $LASTEXITCODE
