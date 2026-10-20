/* .NET through Wine Mono where its package put it: Binding a v4 runtime
 * makes mscoree find and load Mono, and the default domain runs managed
 * code to wrap itself, so a Mono the tree cannot reach fails here instead
 * of in a game. */
#define COBJMACROS
#include "probe.h"
#include <initguid.h>
#include <mscoree.h>

typedef HRESULT (WINAPI *bind_runtime_fn)(LPCWSTR version, LPCWSTR flavor, DWORD flags,
                                          REFCLSID clsid, REFIID iid, void **out);

int main(void)
{
    HMODULE mscoree = LoadLibraryA("mscoree.dll");
    PROBE_REQUIRE(mscoree, "LoadLibrary(mscoree.dll) failed (%lu)", GetLastError());

    bind_runtime_fn bind = (bind_runtime_fn)GetProcAddress(mscoree, "CorBindToRuntimeEx");
    PROBE_REQUIRE(bind, "mscoree.dll exports no CorBindToRuntimeEx");

    ICorRuntimeHost *host = NULL;
    HRESULT hr = bind(L"v4.0.30319", NULL, 0, &CLSID_CorRuntimeHost, &IID_ICorRuntimeHost,
                      (void **)&host);
    PROBE_REQUIRE(SUCCEEDED(hr), "CorBindToRuntimeEx(v4.0.30319) failed (0x%08lx): Mono not found",
                  (unsigned long)hr);

    hr = ICorRuntimeHost_Start(host);
    PROBE_REQUIRE(SUCCEEDED(hr), "ICorRuntimeHost::Start failed (0x%08lx)", (unsigned long)hr);

    IUnknown *domain = NULL;
    hr = ICorRuntimeHost_GetDefaultDomain(host, &domain);
    PROBE_REQUIRE(SUCCEEDED(hr) && domain, "GetDefaultDomain failed (0x%08lx)", (unsigned long)hr);

    probe_verdict(1, "runtime v4.0.30319 started, default domain wrapped");
    return 0;
}
