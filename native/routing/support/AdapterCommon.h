#pragma once
#include <windows.h>
#include <bcrypt.h>
#include <string>
#include <vector>
#include <iterator>
#include <cstdint>
#include <cwchar>
#include <cstring>

static bool FileSha256(const wchar_t* path, std::string& digest) {
    HANDLE file = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                             nullptr, OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN, nullptr);
    if (file == INVALID_HANDLE_VALUE) return false;
    BCRYPT_ALG_HANDLE algorithm = nullptr;
    BCRYPT_HASH_HANDLE hash = nullptr;
    bool good = BCryptOpenAlgorithmProvider(&algorithm, BCRYPT_SHA256_ALGORITHM, nullptr, 0) >= 0;
    DWORD objectSize = 0, returned = 0;
    if (good) good = BCryptGetProperty(algorithm, BCRYPT_OBJECT_LENGTH, reinterpret_cast<PUCHAR>(&objectSize), sizeof(objectSize), &returned, 0) >= 0;
    std::vector<BYTE> object(objectSize), buffer(65536);
    BYTE bytes[32]{};
    if (good) good = BCryptCreateHash(algorithm, &hash, object.data(), objectSize, nullptr, 0, 0) >= 0;
    while (good) {
        DWORD count = 0;
        if (!ReadFile(file, buffer.data(), static_cast<DWORD>(buffer.size()), &count, nullptr)) { good = false; break; }
        if (!count) break;
        good = BCryptHashData(hash, buffer.data(), count, 0) >= 0;
    }
    if (good) good = BCryptFinishHash(hash, bytes, sizeof(bytes), 0) >= 0;
    if (hash) BCryptDestroyHash(hash);
    if (algorithm) BCryptCloseAlgorithmProvider(algorithm, 0);
    CloseHandle(file);
    if (!good) return false;
    static const char digits[] = "0123456789abcdef";
    digest.clear(); digest.reserve(64);
    for (BYTE byte : bytes) { digest.push_back(digits[byte >> 4]); digest.push_back(digits[byte & 15]); }
    return true;
}
static const wchar_t* BaseName(const wchar_t* path) {
    const wchar_t* name = path;
    for (const wchar_t* p = path; *p; ++p) if (*p == L'\\' || *p == L'/') name = p + 1;
    return name;
}
