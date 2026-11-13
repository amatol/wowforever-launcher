// Автономная MinGW-сборка: официальный загрузчик WebView2 встроен в EXE.
// MSVC использует штатный статический загрузчик и этот файл не собирает.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include "webview-loader-bytes.h"

static INIT_ONCE once = INIT_ONCE_STATIC_INIT;
static HMODULE module;

static BOOL CALLBACK initialize(PINIT_ONCE init, PVOID param, PVOID *context) {
    (void)init; (void)param; (void)context;
    WCHAR directory[MAX_PATH], path[MAX_PATH];
    if (!GetTempPathW(MAX_PATH, directory) || !GetTempFileNameW(directory, L"FWV", 0, path)) return TRUE;
    HANDLE file = CreateFileW(path, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_TEMPORARY, NULL);
    if (file == INVALID_HANDLE_VALUE) { DeleteFileW(path); return TRUE; }
    DWORD written = 0;
    BOOL ok = WriteFile(file, loader_bytes, sizeof(loader_bytes), &written, NULL);
    CloseHandle(file);
    if (ok && written == sizeof(loader_bytes))
        module = LoadLibraryExW(path, NULL, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
    // Загруженный модуль остаётся до завершения процесса; Windows может отложить удаление.
    DeleteFileW(path);
    return TRUE;
}

static FARPROC resolve(const char *name) {
    InitOnceExecuteOnce(&once, initialize, NULL, NULL);
    return module ? GetProcAddress(module, name) : NULL;
}

HRESULT WINAPI __wrap_CompareBrowserVersions(LPCWSTR a, LPCWSTR b, int *result) {
    typedef HRESULT (WINAPI *Fn)(LPCWSTR,LPCWSTR,int*);
    Fn fn = (Fn)resolve("CompareBrowserVersions"); return fn ? fn(a,b,result) : E_FAIL;
}
HRESULT WINAPI __wrap_CreateCoreWebView2Environment(void *handler) {
    typedef HRESULT (WINAPI *Fn)(void*);
    Fn fn = (Fn)resolve("CreateCoreWebView2Environment"); return fn ? fn(handler) : E_FAIL;
}
HRESULT WINAPI __wrap_CreateCoreWebView2EnvironmentWithOptions(LPCWSTR browser, LPCWSTR user, void *options, void *handler) {
    typedef HRESULT (WINAPI *Fn)(LPCWSTR,LPCWSTR,void*,void*);
    Fn fn = (Fn)resolve("CreateCoreWebView2EnvironmentWithOptions"); return fn ? fn(browser,user,options,handler) : E_FAIL;
}
HRESULT WINAPI __wrap_GetAvailableCoreWebView2BrowserVersionString(LPCWSTR browser, LPWSTR *version) {
    typedef HRESULT (WINAPI *Fn)(LPCWSTR,LPWSTR*);
    Fn fn = (Fn)resolve("GetAvailableCoreWebView2BrowserVersionString"); return fn ? fn(browser,version) : E_FAIL;
}
HRESULT WINAPI __wrap_GetAvailableCoreWebView2BrowserVersionStringWithOptions(LPCWSTR browser, void *options, LPWSTR *version) {
    typedef HRESULT (WINAPI *Fn)(LPCWSTR,void*,LPWSTR*);
    Fn fn = (Fn)resolve("GetAvailableCoreWebView2BrowserVersionStringWithOptions"); return fn ? fn(browser,options,version) : E_FAIL;
}
