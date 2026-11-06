#include "RouteRequest.h"
#include "../RoutingProtocol.h"
#include "../support/AdapterCommon.h"
#include "../AdapterConfig.h"
#include <shellapi.h>
#include <shlobj.h>
#include <psapi.h>
#include <sddl.h>
#include <intrin.h>
#include <atomic>
#include <memory>
#include <string>
#include <vector>
#include "MinHook.h"

using shell_route::RouteRequest;
using shell_route::DispatchResult;
namespace {
routing_config::AdapterConfig gBinding;
bool gConfigured=false;
constexpr DWORD kSendTimeout = 1000;

using ShellW = HINSTANCE(WINAPI*)(HWND,LPCWSTR,LPCWSTR,LPCWSTR,LPCWSTR,INT);
using ShellExW = BOOL(WINAPI*)(SHELLEXECUTEINFOW*);
using SelectItems = HRESULT(WINAPI*)(PCIDLIST_ABSOLUTE,UINT,PCUITEMID_CHILD_ARRAY,DWORD);
ShellW gOriginalW = nullptr;
ShellExW gOriginalEx = nullptr;
SelectItems gOriginalSelect = nullptr;
void* gTargets[3]{};
bool gCreated = false;
bool gIdentified = false;
HMODULE gModule = nullptr;
uintptr_t gClientBase = 0;
DWORD gClientSize = 0;
std::wstring gWindows;
std::atomic<DWORD> gRole{0};
std::atomic<bool> gEnabled{false};
std::atomic<DWORD> gActive{0},gReceived{0},gCompleted{0},gFailed{0},gDeclined{0},gIndeterminate{0};
std::atomic<ULONGLONG> gSerial{0};
thread_local unsigned gDepth = 0;
SRWLOCK gInstallLock = SRWLOCK_INIT;
SRWLOCK gStatusLock = SRWLOCK_INIT;
DWORD gLastError = 0,gLastKind = 0,gLastCount = 0;
ULONGLONG gLastId = 0;
HWND gLastWindow = nullptr;
wchar_t gLastFolder[1024]{};

struct ActiveScope { ActiveScope(){gActive.fetch_add(1);++gDepth;} ~ActiveScope(){--gDepth;gActive.fetch_sub(1);} };
struct HandleCloser {void operator()(void* value) const {if(value&&value!=INVALID_HANDLE_VALUE)CloseHandle(value);}};
using Handle = std::unique_ptr<void,HandleCloser>;
struct TaskFree {void operator()(void* value) const {CoTaskMemFree(value);}};
struct Pidl {
    PIDLIST_ABSOLUTE value;
    explicit Pidl(PIDLIST_ABSOLUTE pointer):value(pointer){}
    ~Pidl(){CoTaskMemFree(reinterpret_cast<void*>(reinterpret_cast<uintptr_t>(value)));}
    Pidl(const Pidl&)=delete;Pidl& operator=(const Pidl&)=delete;
    PIDLIST_ABSOLUTE get()const{return value;}
    explicit operator bool()const{return value!=nullptr;}
};

bool CopyMemorySafe(void* destination,const void* source,size_t bytes) {
    __try { memcpy(destination,source,bytes); return true; }
    __except(EXCEPTION_EXECUTE_HANDLER) { return false; }
}
bool StringLengthSafe(const wchar_t* value,size_t limit,size_t& length) {
    __try { if(!value){length=0;return true;} length=wcsnlen_s(value,limit+1);return length<=limit; }
    __except(EXCEPTION_EXECUTE_HANDLER) { return false; }
}
bool CopyString(const wchar_t* value,std::wstring& result,size_t limit=32780) {
    size_t length=0;if(!StringLengthSafe(value,limit,length))return false;
    result.resize(length);return !length||CopyMemorySafe(result.data(),value,length*sizeof(wchar_t));
}
void RecordError(DWORD error) {AcquireSRWLockExclusive(&gStatusLock);gLastError=error;ReleaseSRWLockExclusive(&gStatusLock);}
bool HashMatches(const wchar_t* path,const char* expected) {std::string actual;return FileSha256(path,actual)&&actual==expected;}
bool SameUser(HANDLE process) {
    HANDLE rawFirst=nullptr,rawSecond=nullptr;
    if(!OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY,&rawFirst))return false;
    Handle first(rawFirst);
    if(!OpenProcessToken(process,TOKEN_QUERY,&rawSecond))return false;
    Handle second(rawSecond);DWORD a=0,b=0;
    GetTokenInformation(first.get(),TokenUser,nullptr,0,&a);GetTokenInformation(second.get(),TokenUser,nullptr,0,&b);
    if(!a||!b)return false;std::vector<BYTE> one(a),two(b);
    return GetTokenInformation(first.get(),TokenUser,one.data(),a,&a)&&GetTokenInformation(second.get(),TokenUser,two.data(),b,&b)&&
        EqualSid(reinterpret_cast<TOKEN_USER*>(one.data())->User.Sid,reinterpret_cast<TOKEN_USER*>(two.data())->User.Sid);
}
bool ReceiverAllowed(HWND window) {
    DWORD pid=0;GetWindowThreadProcessId(window,&pid);if(!pid||!gConfigured)return false;
    Handle process(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,FALSE,pid));if(!process)return false;
    DWORD ownSession=0,otherSession=0;
    if(!ProcessIdToSessionId(GetCurrentProcessId(),&ownSession)||!ProcessIdToSessionId(pid,&otherSession)||ownSession!=otherSession||!SameUser(process.get()))return false;
    wchar_t path[32768]{};DWORD length=static_cast<DWORD>(std::size(path));
    return QueryFullProcessImageNameW(process.get(),0,path,&length)&&shell_route::EqualPath(path,gBinding.controller)&&HashMatches(path,gBinding.controllerSHA256);
}
HWND FindReceiver() {
    HWND current=nullptr;
    for(unsigned count=0;count<32;++count){current=FindWindowExW(HWND_MESSAGE,current,gBinding.receiverClass,nullptr);if(!current)break;if(ReceiverAllowed(current))return current;}
    return nullptr;
}
bool ExistingRequest(const RouteRequest& request) {
    DWORD attributes=GetFileAttributesW(request.folder.c_str());
    if(attributes==INVALID_FILE_ATTRIBUTES||!(attributes&FILE_ATTRIBUTE_DIRECTORY))return false;
    for(const auto& item:request.items)if(GetFileAttributesW(item.c_str())==INVALID_FILE_ATTRIBUTES)return false;
    return true;
}
DispatchResult SendRequest(const RouteRequest& request,DWORD kind) {
    HWND window=FindReceiver();
    if(!window){gDeclined.fetch_add(1);RecordError(ERROR_NOT_FOUND);return DispatchResult::Declined;}
    DWORD receiverPid=0;
    GetWindowThreadProcessId(window,&receiverPid);
    if(receiverPid)AllowSetForegroundWindow(receiverPid);
    size_t characters=request.folder.size()+1;
    for(const auto& item:request.items)characters+=item.size()+1;
    const size_t bytes=sizeof(routing::PacketHeader)+characters*sizeof(wchar_t);
    if(bytes>routing::kMaxPacketBytes||request.items.size()>routing::kMaxItems){gDeclined.fetch_add(1);RecordError(ERROR_BUFFER_OVERFLOW);return DispatchResult::Declined;}
    std::vector<BYTE> data(bytes);
    routing::PacketHeader header;
    header.bytes=static_cast<DWORD>(bytes);header.itemCount=static_cast<DWORD>(request.items.size());
    header.sourcePid=GetCurrentProcessId();header.sourceKind=kind;
    header.requestId=(static_cast<ULONGLONG>(header.sourcePid)<<32)|(gSerial.fetch_add(1)+1);
    memcpy(data.data(),&header,sizeof(header));size_t offset=sizeof(header);
    auto append=[&](const std::wstring& value){size_t amount=(value.size()+1)*sizeof(wchar_t);memcpy(data.data()+offset,value.c_str(),amount);offset+=amount;};
    append(request.folder);for(const auto& item:request.items)append(item);
    COPYDATASTRUCT copy{routing::kCopyDataTag,header.bytes,data.data()};DWORD_PTR reply=0;
    gReceived.fetch_add(1);
    AcquireSRWLockExclusive(&gStatusLock);gLastId=header.requestId;gLastKind=kind;gLastCount=header.itemCount;gLastWindow=window;
    wcsncpy_s(gLastFolder,request.folder.c_str(),_TRUNCATE);ReleaseSRWLockExclusive(&gStatusLock);
    SetLastError(ERROR_SUCCESS);
    const LRESULT result=SendMessageTimeoutW(window,WM_COPYDATA,0,reinterpret_cast<LPARAM>(&copy),SMTO_ABORTIFHUNG|SMTO_BLOCK,kSendTimeout,&reply);
    if(result&&reply==1){gCompleted.fetch_add(1);RecordError(ERROR_SUCCESS);return DispatchResult::Accepted;}
    if(result&&reply==0){gDeclined.fetch_add(1);RecordError(ERROR_NOT_READY);return DispatchResult::Declined;}
    const DWORD error=GetLastError();
    if(!result&&(error==ERROR_ACCESS_DENIED||error==ERROR_INVALID_WINDOW_HANDLE)){
        gDeclined.fetch_add(1);RecordError(error);return DispatchResult::Declined;
    }
    gIndeterminate.fetch_add(1);RecordError(error?error:ERROR_TIMEOUT);return DispatchResult::Indeterminate;
}
bool CallerAllowed(void* caller) {
    uintptr_t address=reinterpret_cast<uintptr_t>(caller);
    return address>=gClientBase&&address-gClientBase<gClientSize;
}
bool BuildExplorer(const wchar_t* executable,const wchar_t* operation,const wchar_t* parameters,int show,RouteRequest& request,DWORD& kind,bool extended) {
    std::wstring file,verb,args;
    if(!CopyString(executable,file)||!CopyString(operation,verb)||!CopyString(parameters,args))return false;
    if(shell_route::ExplorerSelect(file,verb,args,show,gWindows,request))kind=extended?3:2;
    else if(shell_route::ExplorerDirectory(file,verb,args,show,gWindows,request))kind=4;
    else if(shell_route::DirectDirectory(file,verb,args,show,request))kind=extended?9:8;
    else return false;
    return ExistingRequest(request);
}
bool FileSystemPath(PCIDLIST_ABSOLUTE pidl,std::wstring& path) {
    if(!pidl)return false;PWSTR text=nullptr;
    if(FAILED(SHGetNameFromIDList(pidl,SIGDN_FILESYSPATH,&text))||!text)return false;
    std::unique_ptr<wchar_t,TaskFree> owner(text);
    return CopyString(text,path,shell_route::kMaximumPathCharacters);
}
bool SingleRelativeChild(PCUITEMID_CHILD child) {
    USHORT bytes=0,terminator=1;
    if(!CopyMemorySafe(&bytes,child,sizeof(bytes))||bytes<sizeof(USHORT))return false;
    const auto* following=reinterpret_cast<const BYTE*>(child)+bytes;
    return CopyMemorySafe(&terminator,following,sizeof(terminator))&&terminator==0;
}
bool BuildPidls(PCIDLIST_ABSOLUTE folder,UINT count,PCUITEMID_CHILD_ARRAY children,DWORD flags,RouteRequest& request,bool& absoluteChild) {
    absoluteChild=false;
    if(flags||!folder||count>routing::kMaxItems||(count&&!children))return false;
    std::wstring parent;
    if(!FileSystemPath(folder,parent))return false;
    if(!count)return shell_route::SingleItem(parent,request)&&ExistingRequest(request);
    std::vector<std::wstring> paths;paths.reserve(count);
    for(UINT index=0;index<count;++index) {
        PCUITEMID_CHILD child=nullptr;if(!CopyMemorySafe(&child,children+index,sizeof(child))||!child)return false;
        USHORT firstSize=0;if(!CopyMemorySafe(&firstSize,child,sizeof(firstSize))||firstSize<sizeof(USHORT))return false;
        // Electron passes a desktop-parsed absolute file PIDL in this array.
        // A relative file PIDL can also resolve against the desktop, so accept
        // the standalone interpretation only when its actual parent matches.
        std::wstring standalone;RouteRequest direct;
        if(FileSystemPath(reinterpret_cast<PCIDLIST_ABSOLUTE>(child),standalone)&&
           shell_route::Selection(parent,{standalone},flags,direct)) {
            paths.push_back(std::move(direct.items.front()));absoluteChild=true;continue;
        }
        // The documented child contract describes one immediate child, not a
        // second complete absolute chain appended beneath the supplied folder.
        if(!SingleRelativeChild(child))return false;
        Pidl combined(ILCombine(folder,child));std::wstring path;
        if(!combined||!FileSystemPath(combined.get(),path))return false;
        paths.push_back(std::move(path));
    }
    return shell_route::Selection(parent,paths,flags,request)&&ExistingRequest(request);
}
bool BuildFolderIdList(const SHELLEXECUTEINFOW& info,RouteRequest& request) {
    constexpr ULONG allowed=SEE_MASK_INVOKEIDLIST|SEE_MASK_CLASSNAME|SEE_MASK_FLAG_NO_UI|SEE_MASK_NOASYNC|
        SEE_MASK_UNICODE|SEE_MASK_NO_CONSOLE|SEE_MASK_NOCLOSEPROCESS|SEE_MASK_ASYNCOK|SEE_MASK_FLAG_LOG_USAGE|SEE_MASK_WAITFORINPUTIDLE;
    if((info.fMask&~allowed)||!shell_route::VisibleOpenCommand(info.nShow))return false;
    if((info.fMask&(SEE_MASK_INVOKEIDLIST^SEE_MASK_IDLIST))&&
       (info.fMask&SEE_MASK_INVOKEIDLIST)!=SEE_MASK_INVOKEIDLIST)return false;
    std::wstring file,parameters,verb,kind,path;
    if(!CopyString(info.lpFile,file)||!CopyString(info.lpParameters,parameters)||!shell_route::Trim(parameters).empty()||
       !CopyString(info.lpVerb,verb)||!shell_route::FolderOpenVerb(verb))return false;
    if(info.fMask&SEE_MASK_CLASSNAME){
        if(!CopyString(info.lpClass,kind)||(!shell_route::EqualPath(kind,L"folder")&&!shell_route::EqualPath(kind,L"directory")))return false;
    }
    if((info.fMask&SEE_MASK_IDLIST)&&info.lpIDList){
        if(!FileSystemPath(static_cast<PCIDLIST_ABSOLUTE>(info.lpIDList),path))return false;
        if(!file.empty()){
            std::wstring normalized;if(!shell_route::NormalizeAbsolute(file,normalized)||!shell_route::EqualPath(normalized,path))return false;
        }
    }else{
        if((info.fMask&SEE_MASK_IDLIST)&&((info.fMask&SEE_MASK_INVOKEIDLIST)!=SEE_MASK_INVOKEIDLIST))return false;
        path=std::move(file);
    }
    RouteRequest folder;if(!shell_route::NormalizeAbsolute(path,folder.folder)||!ExistingRequest(folder))return false;
    request=std::move(folder);return true;
}

HINSTANCE WINAPI HookW(HWND owner,LPCWSTR operation,LPCWSTR file,LPCWSTR parameters,LPCWSTR directory,INT show) {
    const DWORD saved=GetLastError();
    if(gDepth||!gEnabled.load()||!CallerAllowed(_ReturnAddress()))return gOriginalW(owner,operation,file,parameters,directory,show);
    ActiveScope active;
    try {
        RouteRequest request;DWORD kind=0;
        if(BuildExplorer(file,operation,parameters,show,request,kind,false)) {
            const auto result=SendRequest(request,kind);
            if(result!=DispatchResult::Declined){SetLastError(result==DispatchResult::Accepted?ERROR_SUCCESS:ERROR_TIMEOUT);return reinterpret_cast<HINSTANCE>(static_cast<INT_PTR>(33));}
        }
    } catch(...) {gFailed.fetch_add(1);RecordError(ERROR_NOT_ENOUGH_MEMORY);}
    SetLastError(saved);return gOriginalW(owner,operation,file,parameters,directory,show);
}
BOOL WINAPI HookEx(SHELLEXECUTEINFOW* info) {
    const DWORD saved=GetLastError();
    if(gDepth||!gEnabled.load()||!CallerAllowed(_ReturnAddress()))return gOriginalEx(info);
    ActiveScope active;
    try {
        SHELLEXECUTEINFOW copy{};
        constexpr ULONG allowed=SEE_MASK_FLAG_NO_UI|SEE_MASK_NOASYNC|SEE_MASK_UNICODE|SEE_MASK_NO_CONSOLE|
            SEE_MASK_NOCLOSEPROCESS|SEE_MASK_ASYNCOK|SEE_MASK_FLAG_LOG_USAGE|SEE_MASK_WAITFORINPUTIDLE;
        if(info&&CopyMemorySafe(&copy,info,sizeof(copy))&&copy.cbSize==sizeof(copy)) {
            RouteRequest request;DWORD kind=0;
            bool supported=false;
            if(!(copy.fMask&~allowed))supported=BuildExplorer(copy.lpFile,copy.lpVerb,copy.lpParameters,copy.nShow,request,kind,true);
            else if(BuildFolderIdList(copy,request)){supported=true;kind=7;}
            if(supported) {
                const auto result=SendRequest(request,kind);
                if(result!=DispatchResult::Declined) {
                    HINSTANCE success=reinterpret_cast<HINSTANCE>(static_cast<INT_PTR>(33));HANDLE empty=nullptr;
                    CopyMemorySafe(&info->hInstApp,&success,sizeof(success));CopyMemorySafe(&info->hProcess,&empty,sizeof(empty));
                    SetLastError(result==DispatchResult::Accepted?ERROR_SUCCESS:ERROR_TIMEOUT);return TRUE;
                }
            }
        }
    } catch(...) {gFailed.fetch_add(1);RecordError(ERROR_NOT_ENOUGH_MEMORY);}
    SetLastError(saved);return gOriginalEx(info);
}
HRESULT WINAPI HookSelect(PCIDLIST_ABSOLUTE folder,UINT count,PCUITEMID_CHILD_ARRAY children,DWORD flags) {
    const DWORD saved=GetLastError();
    if(gDepth||!gEnabled.load()||!CallerAllowed(_ReturnAddress()))return gOriginalSelect(folder,count,children,flags);
    ActiveScope active;
    try {
        RouteRequest request;bool absoluteChild=false;
        if(BuildPidls(folder,count,children,flags,request,absoluteChild)) {
            const auto result=SendRequest(request,count>1?5:absoluteChild?6:1);
            if(result!=DispatchResult::Declined){SetLastError(result==DispatchResult::Accepted?ERROR_SUCCESS:ERROR_TIMEOUT);return S_OK;}
        }
    } catch(...) {gFailed.fetch_add(1);RecordError(ERROR_NOT_ENOUGH_MEMORY);}
        SetLastError(saved);return gOriginalSelect(folder,count,children,flags);
}
bool ValidHash(const char* text,size_t capacity) {
    const char* end=static_cast<const char*>(memchr(text,0,capacity));if(!end||end-text!=64)return false;
    for(const char* p=text;p!=end;++p)if(!((*p>='0'&&*p<='9')||(*p>='a'&&*p<='f')||(*p>='A'&&*p<='F')))return false;return true;
}
bool ValidBinding(const routing_config::AdapterConfig& config) {
    if(config.magic!=routing_config::kMagic||config.version!=routing_config::kVersion||config.bytes!=sizeof(config)||!config.applicationId)return false;
    if(!wmemchr(config.executable,0,std::size(config.executable))||!wmemchr(config.callerModule,0,std::size(config.callerModule))||
       !wmemchr(config.controller,0,std::size(config.controller))||!wmemchr(config.receiverClass,0,std::size(config.receiverClass))||!config.receiverClass[0])return false;
    std::wstring normalized;
    if(!shell_route::NormalizeAbsolute(config.executable,normalized)||!shell_route::NormalizeAbsolute(config.callerModule,normalized)||!shell_route::NormalizeAbsolute(config.controller,normalized))return false;
    return ValidHash(config.executableSHA256,sizeof(config.executableSHA256))&&ValidHash(config.callerSHA256,sizeof(config.callerSHA256))&&ValidHash(config.controllerSHA256,sizeof(config.controllerSHA256));
}
DWORD Identify() {
    if(!gConfigured)return ERROR_NOT_READY;
    wchar_t path[32768]{};if(!GetModuleFileNameW(nullptr,path,static_cast<DWORD>(std::size(path))))return GetLastError();
    if(!shell_route::EqualPath(path,gBinding.executable)||!HashMatches(path,gBinding.executableSHA256))return ERROR_REVISION_MISMATCH;
    int argc=0;LPWSTR* argv=CommandLineToArgvW(GetCommandLineW(),&argc);if(!argv)return GetLastError();bool child=false;
    for(int i=1;i<argc;++i)if(!wcsncmp(argv[i],L"--type=",7)||!wcscmp(argv[i],L"--type"))child=true;LocalFree(argv);if(child)return ERROR_NOT_SUPPORTED;
    switch(gBinding.profile){
        case routing_config::Profile::NativeShell:if(!shell_route::EqualPath(path,gBinding.callerModule))return ERROR_NOT_SUPPORTED;break;
        case routing_config::Profile::Chromium:if(_wcsicmp(BaseName(gBinding.callerModule),L"chrome.dll"))return ERROR_NOT_SUPPORTED;break;
        case routing_config::Profile::WeChat:if(_wcsicmp(BaseName(gBinding.callerModule),L"Weixin.dll"))return ERROR_NOT_SUPPORTED;break;
        default:return ERROR_NOT_SUPPORTED;
    }
    HMODULE module=shell_route::EqualPath(path,gBinding.callerModule)?GetModuleHandleW(nullptr):GetModuleHandleW(gBinding.callerModule);
    if(!module)return ERROR_NOT_READY;wchar_t loaded[32768]{};
    if(!GetModuleFileNameW(module,loaded,DWORD(std::size(loaded)))||!shell_route::EqualPath(loaded,gBinding.callerModule)||!HashMatches(loaded,gBinding.callerSHA256))return ERROR_REVISION_MISMATCH;
    MODULEINFO info{};if(!GetModuleInformation(GetCurrentProcess(),module,&info,sizeof(info)))return GetLastError();
    gClientBase=reinterpret_cast<uintptr_t>(info.lpBaseOfDll);gClientSize=info.SizeOfImage;gRole=gBinding.applicationId;
    wchar_t windows[32768]{};UINT length=GetWindowsDirectoryW(windows,DWORD(std::size(windows)));if(!length||length>=std::size(windows))return ERROR_BUFFER_OVERFLOW;gWindows=windows;return 0;
}
DWORD PrepareHooks() {
    HMODULE shell=LoadLibraryExW(L"shell32.dll",nullptr,LOAD_LIBRARY_SEARCH_SYSTEM32);if(!shell)return GetLastError();
    MH_STATUS state=MH_Initialize();if(state!=MH_OK&&state!=MH_ERROR_ALREADY_INITIALIZED)return ERROR_NOT_SUPPORTED;
    const char* names[]{"ShellExecuteW","ShellExecuteExW","SHOpenFolderAndSelectItems"};
    void* detours[]{reinterpret_cast<void*>(HookW),reinterpret_cast<void*>(HookEx),reinterpret_cast<void*>(HookSelect)};
    void** originals[]{reinterpret_cast<void**>(&gOriginalW),reinterpret_cast<void**>(&gOriginalEx),reinterpret_cast<void**>(&gOriginalSelect)};
    for(unsigned index=0;index<3;++index) {
        gTargets[index]=reinterpret_cast<void*>(GetProcAddress(shell,names[index]));
        if(!gTargets[index])return ERROR_PROC_NOT_FOUND;
        state=MH_CreateHook(gTargets[index],detours[index],originals[index]);
        if(state!=MH_OK&&state!=MH_ERROR_ALREADY_CREATED)return ERROR_NOT_SUPPORTED;
    }
    HMODULE pinned=nullptr;
    if(!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS|GET_MODULE_HANDLE_EX_FLAG_PIN,reinterpret_cast<LPCWSTR>(gModule),&pinned))return GetLastError();
    gCreated=true;return 0;
}
} // namespace

extern "C" __declspec(dllexport) DWORD WINAPI RouterGetAbi(void*) { return routing_config::kVersion; }
extern "C" __declspec(dllexport) DWORD WINAPI RouterConfigure(void* parameter) {
    routing_config::AdapterConfig incoming;
    if(!parameter||!CopyMemorySafe(&incoming,parameter,sizeof(incoming))||!ValidBinding(incoming))return ERROR_INVALID_PARAMETER;
    AcquireSRWLockExclusive(&gInstallLock);
    if(gConfigured&&!memcmp(&incoming,&gBinding,sizeof(incoming))){ReleaseSRWLockExclusive(&gInstallLock);return 0;}
    if(gEnabled.load()||gActive.load()){ReleaseSRWLockExclusive(&gInstallLock);return ERROR_BUSY;}
    gBinding=incoming;gConfigured=true;gIdentified=false;gClientBase=0;gClientSize=0;
    ReleaseSRWLockExclusive(&gInstallLock);return 0;
}
extern "C" __declspec(dllexport) DWORD WINAPI RouterInstall(void*) {
    AcquireSRWLockExclusive(&gInstallLock);DWORD error=0;
    try {
        if(gEnabled.load()){ReleaseSRWLockExclusive(&gInstallLock);return 0;}
        if(gActive.load())error=ERROR_BUSY;
        if(!error&&!gIdentified){error=Identify();if(!error)gIdentified=true;}
        if(!error&&!gCreated)error=PrepareHooks();
        if(!error) {
            for(void* target:gTargets)if(MH_QueueEnableHook(target)!=MH_OK){error=ERROR_NOT_SUPPORTED;break;}
            if(!error&&MH_ApplyQueued()!=MH_OK)error=ERROR_NOT_SUPPORTED;
            if(!error)gEnabled.store(true);
        }
    } catch(...) {error=ERROR_NOT_ENOUGH_MEMORY;}
    if(error&&gCreated){gEnabled.store(false);MH_DisableHook(MH_ALL_HOOKS);}
    RecordError(error);ReleaseSRWLockExclusive(&gInstallLock);return error;
}
extern "C" __declspec(dllexport) DWORD WINAPI RouterUninstall(void*) {
    AcquireSRWLockExclusive(&gInstallLock);gEnabled.store(false);
    MH_STATUS status=gCreated?MH_DisableHook(MH_ALL_HOOKS):MH_OK;
    DWORD error=(status==MH_OK||status==MH_ERROR_DISABLED)?0:ERROR_NOT_SUPPORTED;
    if(!error&&gActive.load())error=ERROR_BUSY;
    RecordError(error);ReleaseSRWLockExclusive(&gInstallLock);return error;
}
extern "C" __declspec(dllexport) DWORD WINAPI RouterGetStatus(routing::Status* output) {
    if(!output)return ERROR_INVALID_PARAMETER;routing::Status result;
    result.role=gRole;result.installed=gEnabled.load()?1:0;result.activeCalls=gActive.load();result.received=gReceived.load();result.completed=gCompleted.load();
    result.failed=gFailed.load();result.declined=gDeclined.load();result.indeterminate=gIndeterminate.load();
    AcquireSRWLockShared(&gStatusLock);result.lastError=gLastError;result.lastSourceKind=gLastKind;result.lastItemCount=gLastCount;
    result.lastId=gLastId;result.bridgeWindow=reinterpret_cast<ULONG_PTR>(gLastWindow);memcpy(result.lastFolder,gLastFolder,sizeof(gLastFolder));ReleaseSRWLockShared(&gStatusLock);
    return CopyMemorySafe(output,&result,sizeof(result))?0:ERROR_NOACCESS;
}
BOOL WINAPI DllMain(HINSTANCE instance,DWORD reason,LPVOID) {
    if(reason==DLL_PROCESS_ATTACH){gModule=instance;DisableThreadLibraryCalls(instance);}return TRUE;
}
