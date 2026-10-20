/* The patched kernelbase in effect: Patch 006 opens the caller for Steam's
 * placeholder PID 0xfffe, which no Wine process has, and patch 005 strips
 * PROTON_EAC_LAUNCHER_PROCESS from every child that is not the EasyAntiCheat
 * launcher, so a child of a process holding it must not inherit it. */
#include "probe.h"
#include <string.h>

#define EAC_VAR "PROTON_EAC_LAUNCHER_PROCESS"

int main(int argc, char **argv)
{
    if (argc > 1 && !strcmp(argv[1], "child"))
        return GetEnvironmentVariableA(EAC_VAR, NULL, 0) ? 1 : 0;

    HANDLE self = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, 0xfffe);
    PROBE_REQUIRE(self, "OpenProcess(0xfffe) failed (%lu): patch 006 not in effect", GetLastError());
    DWORD pid = GetProcessId(self);
    CloseHandle(self);
    PROBE_REQUIRE(pid == GetCurrentProcessId(), "OpenProcess(0xfffe) opened pid %lu, not the caller", pid);

    char exe[MAX_PATH], cmd[MAX_PATH + 16];
    PROBE_REQUIRE(GetModuleFileNameA(NULL, exe, sizeof exe), "GetModuleFileName failed (%lu)", GetLastError());
    snprintf(cmd, sizeof cmd, "\"%s\" child", exe);
    PROBE_REQUIRE(SetEnvironmentVariableA(EAC_VAR, "1"), "SetEnvironmentVariable failed (%lu)", GetLastError());

    STARTUPINFOA si = { sizeof si };
    PROCESS_INFORMATION pi;
    PROBE_REQUIRE(CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi),
                  "CreateProcess failed (%lu)", GetLastError());
    WaitForSingleObject(pi.hProcess, INFINITE);
    DWORD code = 1;
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    PROBE_REQUIRE(code == 0, "the child inherited " EAC_VAR ": patch 005 not in effect");

    probe_verdict(1, "OpenProcess(0xfffe) opened the caller, and a child lost " EAC_VAR);
    return 0;
}
