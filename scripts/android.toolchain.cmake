# NDK toolchain wrapper for CMake-built dependencies (BoringSSL). build-android.ps1 passes it as
# CMAKE_TOOLCHAIN_FILE_<target>, which also stops btls-sys from setting CMAKE_C_COMPILER itself; that
# conflicts with the NDK toolchain and makes CMake fall back to configuring for the host.
set(ANDROID_ABI "$ENV{ANDROID_TOOLCHAIN_ABI}")
set(ANDROID_PLATFORM "$ENV{ANDROID_TOOLCHAIN_API}")
include("$ENV{ANDROID_NDK_HOME}/build/cmake/android.toolchain.cmake")
