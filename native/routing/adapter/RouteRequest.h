#pragma once

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

namespace shell_route {

inline constexpr size_t kMaximumItems = 128;
inline constexpr size_t kMaximumPathCharacters = 32767;

struct RouteRequest {
    std::wstring folder;
    std::vector<std::wstring> items; // Absolute filesystem paths, in caller order.
};

// Indeterminate is not permission to repeat the action through the original API.
enum class DispatchResult { Declined, Accepted, Indeterminate };

inline bool EqualPath(std::wstring_view first, std::wstring_view second) {
    if (first.size() != second.size()) return false;
    return CompareStringOrdinal(first.data(), static_cast<int>(first.size()),
                                second.data(), static_cast<int>(second.size()), TRUE) == CSTR_EQUAL;
}

inline std::wstring_view Trim(std::wstring_view value) {
    while (!value.empty() && (value.front() == L' ' || value.front() == L'\t')) value.remove_prefix(1);
    while (!value.empty() && (value.back() == L' ' || value.back() == L'\t')) value.remove_suffix(1);
    return value;
}

inline size_t RootLength(std::wstring_view path) {
    if (path.size() >= 3 && ((path[0] >= L'A' && path[0] <= L'Z') ||
                            (path[0] >= L'a' && path[0] <= L'z')) &&
        path[1] == L':' && path[2] == L'\\') return 3;
    if (path.size() < 5 || path[0] != L'\\' || path[1] != L'\\' ||
        path[2] == L'?' || path[2] == L'.') return 0;
    const size_t serverEnd = path.find(L'\\', 2);
    if (serverEnd == std::wstring_view::npos || serverEnd == 2) return 0;
    const size_t shareEnd = path.find(L'\\', serverEnd + 1);
    if (shareEnd == serverEnd + 1 || serverEnd + 1 == path.size()) return 0;
    return shareEnd == std::wstring_view::npos ? path.size() : shareEnd + 1;
}

// Lexical normalization only; this performs no filesystem or network I/O.
// Extended/device paths and shell namespace objects are intentionally declined.
inline bool NormalizeAbsolute(std::wstring_view path, std::wstring& output) {
    if (path.empty() || path.size() > kMaximumPathCharacters || !RootLength(path)) return false;
    for (size_t index = 0; index < path.size(); ++index) {
        const wchar_t ch = path[index];
        if (ch < 0x20 || ch == L'/' || ch == L'"' || ch == L'<' || ch == L'>' ||
            ch == L'|' || ch == L'*' || ch == L'?' || (ch == L':' && index != 1)) return false;
    }
    const std::wstring input(path);
    const DWORD needed = GetFullPathNameW(input.c_str(), 0, nullptr, nullptr);
    if (!needed || needed > kMaximumPathCharacters + 1) return false;
    std::wstring full(needed, L'\0');
    const DWORD length = GetFullPathNameW(input.c_str(), needed, full.data(), nullptr);
    if (!length || length >= needed) return false;
    full.resize(length);
    const size_t root = RootLength(full);
    if (!root) return false;
    while (full.size() > root && full.back() == L'\\') full.pop_back();
    if (full.size() == root && full.size() >= 2 && full[0] == L'\\' && full[1] == L'\\' &&
        full.back() != L'\\') full.push_back(L'\\');
    output = std::move(full);
    return true;
}

inline bool ParentOfNormalized(std::wstring_view item, std::wstring& parent) {
    const size_t root = RootLength(item);
    if (!root || item.size() <= root) return false;
    const size_t separator = item.find_last_of(L'\\');
    if (separator == std::wstring_view::npos) return false;
    return NormalizeAbsolute(item.substr(0, separator < root ? root : separator), parent);
}

inline bool SingleItem(std::wstring_view item, RouteRequest& output) {
    std::wstring normalized, parent;
    if (!NormalizeAbsolute(item, normalized) || !ParentOfNormalized(normalized, parent)) return false;
    RouteRequest request;
    request.folder = std::move(parent);
    request.items.push_back(std::move(normalized));
    output = std::move(request);
    return true;
}

// For cidl > 0 after the adapter converts the absolute folder and each combined
// folder/child PIDL to SIGDN_FILESYSPATH. cidl == 0 uses SingleItem instead.
inline bool Selection(std::wstring_view folder, const std::vector<std::wstring>& items,
                      DWORD flags, RouteRequest& output) {
    if (flags != 0 || items.empty() || items.size() > kMaximumItems) return false;
    RouteRequest request;
    if (!NormalizeAbsolute(folder, request.folder)) return false;
    request.items.reserve(items.size());
    for (const auto& item : items) {
        std::wstring normalized, parent;
        if (!NormalizeAbsolute(item, normalized) || !ParentOfNormalized(normalized, parent) ||
            !EqualPath(parent, request.folder)) return false;
        request.items.push_back(std::move(normalized));
    }
    output = std::move(request);
    return true;
}

inline bool IsExplorer(std::wstring_view executable, std::wstring_view windowsDirectory) {
    if (EqualPath(executable, L"explorer.exe")) return true;
    std::wstring actual, expected;
    if (!NormalizeAbsolute(executable, actual)) return false;
    std::wstring path(windowsDirectory);
    if (path.empty()) return false;
    if (path.back() != L'\\') path.push_back(L'\\');
    path += L"explorer.exe";
    return NormalizeAbsolute(path, expected) && EqualPath(actual, expected);
}

// Accept only the known Weixin open/explorer.exe /select,<one absolute path>
// form. Unsupported Explorer switches, non-default show modes and verbs fall back.
inline bool ExplorerSelect(std::wstring_view executable, std::wstring_view operation,
                           std::wstring_view parameters, int showCommand,
                           std::wstring_view windowsDirectory, RouteRequest& output) {
    if ((!operation.empty() && !EqualPath(operation, L"open")) || showCommand != SW_SHOWNORMAL ||
        !IsExplorer(executable, windowsDirectory)) return false;
    parameters = Trim(parameters);
    constexpr std::wstring_view prefix = L"/select,";
    if (parameters.size() <= prefix.size() || !EqualPath(parameters.substr(0, prefix.size()), prefix)) return false;
    auto path = Trim(parameters.substr(prefix.size()));
    if (path.empty()) return false;
    if (path.front() == L'"') {
        if (path.size() < 2 || path.back() != L'"') return false;
        path.remove_prefix(1);
        path.remove_suffix(1);
        if (path.find(L'"') != std::wstring_view::npos) return false;
    } else if (path.find(L'"') != std::wstring_view::npos || path.find(L',') != std::wstring_view::npos) {
        return false;
    }
    return SingleItem(path, output);
}

// The caller must additionally verify FILE_ATTRIBUTE_DIRECTORY before dispatch.
inline bool ExplorerDirectory(std::wstring_view executable, std::wstring_view operation,
                              std::wstring_view parameters, int showCommand,
                              std::wstring_view windowsDirectory, RouteRequest& output) {
    if ((!operation.empty() && !EqualPath(operation, L"open")) || showCommand != SW_SHOWNORMAL ||
        !IsExplorer(executable, windowsDirectory)) return false;
    auto path = Trim(parameters);
    if (path.empty()) return false;
    if (path.front() == L'"') {
        if (path.size() < 2 || path.back() != L'"') return false;
        path.remove_prefix(1);
        path.remove_suffix(1);
    } else if (path.find(L',') != std::wstring_view::npos) return false;
    RouteRequest request;
    if (!NormalizeAbsolute(path, request.folder)) return false;
    output = std::move(request);
    return true;
}

} // namespace shell_route
