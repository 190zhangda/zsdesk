#pragma once
#include <windows.h>
#include <shlobj.h>
#include <shobjidl.h>
#include <wrl/client.h>
#include <memory>
#include <new>
#include <utility>
#include <vector>

namespace menu_selection_agile {
// This one-method interface is consumed by GetTruncatedArrayIfNeeded in the
// exact guarded Windows.UI.FileExplorer image. For a complete immutable array,
// its original item count is identical to IShellItemArray::GetCount.
struct __declspec(uuid("D4C5A1A7-E152-4A59-B3F6-0190A5A6D385")) OriginalCount : IUnknown {
    virtual HRESULT STDMETHODCALLTYPE GetOriginalCount(DWORD* count) = 0;
};

struct TaskFree {
    using pointer = PIDLIST_ABSOLUTE;
    void operator()(pointer value) const noexcept {
        CoTaskMemFree(reinterpret_cast<void*>(reinterpret_cast<ULONG_PTR>(value)));
    }
};

// The snapshot owns only immutable absolute PIDLs and its aggregated FTM.
// Every native Shell interface is created and released in the calling apartment.
class Selection final : public IShellItemArray, public OriginalCount {
    LONG references_ = 1;
    std::vector<std::unique_ptr<ITEMIDLIST, TaskFree>> owners_;
    std::vector<PCIDLIST_ABSOLUTE> identifiers_;
    Microsoft::WRL::ComPtr<IUnknown> marshaler_;

    Selection() = default;
    ~Selection() = default;

    HRESULT LocalArray(IShellItemArray** output) const noexcept {
        return SHCreateShellItemArrayFromIDLists(
            static_cast<UINT>(identifiers_.size()),
            const_cast<PCIDLIST_ABSOLUTE*>(identifiers_.data()), output);
    }

    HRESULT Capture(IShellItemArray* source) noexcept {
        try {
            DWORD count = 0;
            HRESULT result = source->GetCount(&count);
            if (FAILED(result) || !count) return FAILED(result) ? result : S_FALSE;
            owners_.reserve(count);
            identifiers_.reserve(count);
            for (DWORD index = 0; index < count; ++index) {
                Microsoft::WRL::ComPtr<IShellItem> item;
                result = source->GetItemAt(index, &item);
                if (FAILED(result)) return result;
                if (!item) return E_UNEXPECTED;
                PIDLIST_ABSOLUTE identifier = nullptr;
                result = SHGetIDListFromObject(item.Get(), &identifier);
                std::unique_ptr<ITEMIDLIST, TaskFree> owned(identifier);
                if (FAILED(result)) return result;
                if (!identifier) return E_UNEXPECTED;
                identifiers_.push_back(identifier);
                owners_.push_back(std::move(owned));
            }
            return CoCreateFreeThreadedMarshaler(static_cast<IShellItemArray*>(this), &marshaler_);
        } catch (const std::bad_alloc&) {
            return E_OUTOFMEMORY;
        } catch (...) {
            return E_UNEXPECTED;
        }
    }

public:
    static HRESULT Create(IShellItemArray* source, IShellItemArray** output) noexcept {
        if (!output) return E_POINTER;
        *output = nullptr;
        if (!source) return S_FALSE;
        Selection* value = new (std::nothrow) Selection();
        if (!value) return E_OUTOFMEMORY;
        HRESULT result = value->Capture(source);
        if (result != S_OK) {
            value->Release();
            return result;
        }
        *output = value;
        return S_OK;
    }

    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, void** output) override {
        if (!output) return E_POINTER;
        *output = nullptr;
        if (iid == IID_IUnknown || iid == IID_IShellItemArray) {
            *output = static_cast<IShellItemArray*>(this);
            AddRef();
            return S_OK;
        }
        if (iid == __uuidof(OriginalCount)) {
            *output = static_cast<OriginalCount*>(this);
            AddRef();
            return S_OK;
        }
        if (iid == IID_IMarshal && marshaler_) return marshaler_->QueryInterface(iid, output);
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override {
        return static_cast<ULONG>(InterlockedIncrement(&references_));
    }
    ULONG STDMETHODCALLTYPE Release() override {
        ULONG remaining = static_cast<ULONG>(InterlockedDecrement(&references_));
        if (!remaining) delete this;
        return remaining;
    }
    HRESULT STDMETHODCALLTYPE BindToHandler(IBindCtx* context, REFGUID handler, REFIID iid, void** output) override {
        if (!output) return E_POINTER;
        *output = nullptr;
        Microsoft::WRL::ComPtr<IShellItemArray> local;
        HRESULT result = LocalArray(&local);
        return FAILED(result) ? result : local->BindToHandler(context, handler, iid, output);
    }
    HRESULT STDMETHODCALLTYPE GetPropertyStore(GETPROPERTYSTOREFLAGS flags, REFIID iid, void** output) override {
        if (!output) return E_POINTER;
        *output = nullptr;
        Microsoft::WRL::ComPtr<IShellItemArray> local;
        HRESULT result = LocalArray(&local);
        return FAILED(result) ? result : local->GetPropertyStore(flags, iid, output);
    }
    HRESULT STDMETHODCALLTYPE GetPropertyDescriptionList(REFPROPERTYKEY key, REFIID iid, void** output) override {
        if (!output) return E_POINTER;
        *output = nullptr;
        Microsoft::WRL::ComPtr<IShellItemArray> local;
        HRESULT result = LocalArray(&local);
        return FAILED(result) ? result : local->GetPropertyDescriptionList(key, iid, output);
    }
    HRESULT STDMETHODCALLTYPE GetAttributes(SIATTRIBFLAGS flags, SFGAOF mask, SFGAOF* attributes) override {
        if (!attributes) return E_POINTER;
        *attributes = 0;
        Microsoft::WRL::ComPtr<IShellItemArray> local;
        HRESULT result = LocalArray(&local);
        return FAILED(result) ? result : local->GetAttributes(flags, mask, attributes);
    }
    HRESULT STDMETHODCALLTYPE GetCount(DWORD* count) override {
        if (!count) return E_POINTER;
        *count = static_cast<DWORD>(identifiers_.size());
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GetOriginalCount(DWORD* count) override {
        return GetCount(count);
    }
    HRESULT STDMETHODCALLTYPE GetItemAt(DWORD index, IShellItem** item) override {
        if (!item) return E_POINTER;
        *item = nullptr;
        if (index >= identifiers_.size()) return E_INVALIDARG;
        return SHCreateItemFromIDList(identifiers_[index], IID_PPV_ARGS(item));
    }
    HRESULT STDMETHODCALLTYPE EnumItems(IEnumShellItems** enumerator) override {
        if (!enumerator) return E_POINTER;
        *enumerator = nullptr;
        Microsoft::WRL::ComPtr<IShellItemArray> local;
        HRESULT result = LocalArray(&local);
        return FAILED(result) ? result : local->EnumItems(enumerator);
    }
};

// Requires the exact verified Old presenter, called on its UI thread between
// successful PrepareForContextMenu and DoContextMenu.
inline HRESULT DetachVerifiedPresenterSelection(IUnknown* presenter) noexcept {
    if (!presenter) return E_POINTER;
    Microsoft::WRL::ComPtr<IUnknown> keepPresenter(presenter);
    auto slot = reinterpret_cast<IShellItemArray**>(reinterpret_cast<BYTE*>(presenter) + 0x60);
    IShellItemArray* original = *slot;
    if (!original) return S_FALSE;
    original->AddRef();
    IShellItemArray* snapshot = nullptr;
    HRESULT result = Selection::Create(original, &snapshot);
    if (result == S_OK && snapshot) {
        void* previous = InterlockedCompareExchangePointer(
            reinterpret_cast<void**>(slot), snapshot, original);
        if (previous == original) {
            original->Release();
            snapshot = nullptr;
        } else {
            result = HRESULT_FROM_WIN32(ERROR_RETRY);
        }
    }
    if (snapshot) snapshot->Release();
    original->Release();
    return result;
}
} // namespace menu_selection_agile
