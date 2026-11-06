#pragma once
#include <windows.h>
#include <shobjidl.h>
#include <wrl/client.h>
#include <atomic>
#include <cstring>
#include "MinHook.h"
#include "CommandAgility.h"

namespace menu_command_bridge {
using InsertFunction = void(__fastcall*)(IUnknown*, HMENU, IExplorerCommand*, UINT, void*, const void*);
using OwnerProbe = bool(*)(IUnknown*, DWORD*, ULONGLONG*) noexcept;
inline InsertFunction originalInsert = nullptr;
inline OwnerProbe ownerProbe = nullptr;
inline SRWLOCK setupLock = SRWLOCK_INIT;
inline void* target = nullptr;
inline HMODULE retainedModule = nullptr;
inline std::atomic<bool> enabled{false};
inline std::atomic<unsigned> active{0}, converted{0}, failures{0};
inline HRESULT setupFailure = S_OK;
inline constexpr BYTE originalEntry[32] = {
    0x40,0x55,0x53,0x56,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57,0x48,0x8d,0xac,
    0x24,0x08,0xff,0xff,0xff,0x48,0x81,0xec,0xf8,0x01,0x00,0x00,0x48,0x8b,0x05,0xed
};
struct Scope {
    IUnknown* native;
    DWORD owner;
    ULONGLONG generation;
};
inline bool IsCurrent(void* context) noexcept {
    const auto scope = static_cast<Scope*>(context);
    DWORD owner = 0;
    ULONGLONG generation = 0;
    return enabled.load() && ownerProbe &&
        ownerProbe(scope->native, &owner, &generation) &&
        owner == scope->owner && generation == scope->generation && owner != GetCurrentThreadId();
}
inline void __fastcall Insert(IUnknown* native, HMENU menu, IExplorerCommand* command,
        UINT index, void* storage, const void* packageName) {
    ++active;
    struct ActiveScope { ~ActiveScope() { --active; } } activeScope;
    Scope scope{native, 0, 0};
    const bool ours = enabled.load() && ownerProbe &&
        ownerProbe(native, &scope.owner, &scope.generation) && scope.owner != GetCurrentThreadId();
    Microsoft::WRL::ComPtr<IUnknown> keepNative;
    Microsoft::WRL::ComPtr<IExplorerCommand> keepCommand;
    if (ours) { keepNative = native; keepCommand = command; }
    menu_command_agility::VectorRange before{};
    size_t first = 0;
    const bool valid = ours && command && menu_command_agility::ReadRange(storage, before, first);
    // Preserve the native operation and its exception semantics exactly once.
    originalInsert(native, menu, command, index, storage, packageName);
    if (!valid || !IsCurrent(&scope)) return;
    unsigned changed = 0;
    HRESULT result = E_UNEXPECTED;
    try {
        result = menu_command_agility::ConvertInsertedCommand(
            storage, first, command, &IsCurrent, &scope, &changed);
    } catch (...) { result = E_UNEXPECTED; }
    converted.fetch_add(changed);
    if (FAILED(result)) ++failures;
}

// The caller first verifies the full Windows.UI.FileExplorer image hash and
// presenter vtable. The entry guard also rejects an unrelated existing hook.
inline HRESULT InstallVerified(HMODULE module, OwnerProbe probe) noexcept {
    if (!module || !probe) return E_INVALIDARG;
    AcquireSRWLockExclusive(&setupLock);
    HRESULT result = setupFailure;
    void* entry = reinterpret_cast<BYTE*>(module) + 0x75130;
    if (SUCCEEDED(result) && target && target != entry) result = HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
    if (SUCCEEDED(result) && ownerProbe && ownerProbe != probe) result = E_UNEXPECTED;
    if (SUCCEEDED(result) && enabled.load()) {
        result = ownerProbe == probe ? S_OK : E_UNEXPECTED;
    } else if (SUCCEEDED(result)) {
        BYTE bytes[sizeof(originalEntry)]{};
        if (!menu_command_agility::ReadBytes(entry, bytes, sizeof(bytes)) ||
            std::memcmp(bytes, originalEntry, sizeof(bytes))) {
            result = HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
        } else {
            if (!target) {
                MH_STATUS status = MH_Initialize();
                if (status != MH_OK && status != MH_ERROR_ALREADY_INITIALIZED) result = E_FAIL;
                if (SUCCEEDED(result)) {
                    status = MH_CreateHook(entry, reinterpret_cast<void*>(&Insert), reinterpret_cast<void**>(&originalInsert));
                    if (status != MH_OK || !originalInsert) result = E_FAIL;
                }
                if (SUCCEEDED(result)) {
                    target = entry;
                    // The retained trampoline and any already-entered callback
                    // must keep their native destination valid until process exit.
                    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                            reinterpret_cast<LPCWSTR>(entry), &retainedModule)) result = E_FAIL;
                }
                if (FAILED(result)) setupFailure = result;
            }
            if (SUCCEEDED(result)) {
                // Publish once before the first hook activation. Old callbacks
                // can remain in flight across a disable/re-enable cycle.
                if (!ownerProbe) ownerProbe = probe;
                const MH_STATUS status = MH_EnableHook(target);
                if (status == MH_OK) enabled.store(true);
                else result = E_FAIL;
            }
        }
    }
    ReleaseSRWLockExclusive(&setupLock);
    return result;
}

inline HRESULT Disable() noexcept {
    AcquireSRWLockExclusive(&setupLock);
    HRESULT result = S_OK;
    if (enabled.load()) {
        const MH_STATUS status = MH_DisableHook(target);
        if (status != MH_OK && status != MH_ERROR_DISABLED) result = E_FAIL;
        BYTE bytes[sizeof(originalEntry)]{};
        if (SUCCEEDED(result) && (!menu_command_agility::ReadBytes(target, bytes, sizeof(bytes)) ||
                std::memcmp(bytes, originalEntry, sizeof(bytes)))) result = HRESULT_FROM_WIN32(ERROR_REVISION_MISMATCH);
        if (SUCCEEDED(result)) enabled.store(false);
    }
    // Do not remove the trampoline or unload either module while native tasks
    // may still hold references. Rollback disables behavior and restores bytes.
    ReleaseSRWLockExclusive(&setupLock);
    return result;
}
} // namespace menu_command_bridge
