#pragma once
#include "AdapterCommon.h"
#include <tlhelp32.h>
#include <memory>

struct HandleCloser { void operator()(void* value) const { if(value&&value!=INVALID_HANDLE_VALUE)CloseHandle(value); } };
using Handle=std::unique_ptr<void,HandleCloser>;
struct RemoteModule { uintptr_t base=0;std::wstring path; };
static DWORD ErrorOr(DWORD fallback=ERROR_GEN_FAILURE){DWORD error=GetLastError();return error?error:fallback;}
static bool IsX64Process(HANDLE process){
    using Fn=BOOL(WINAPI*)(HANDLE,USHORT*,USHORT*);
    auto fn=reinterpret_cast<Fn>(GetProcAddress(GetModuleHandleW(L"kernel32.dll"),"IsWow64Process2"));
    USHORT processMachine=0,nativeMachine=0;
    return fn&&fn(process,&processMachine,&nativeMachine)&&processMachine==IMAGE_FILE_MACHINE_UNKNOWN&&nativeMachine==IMAGE_FILE_MACHINE_AMD64;
}
static DWORD ReadModules(DWORD pid,std::vector<RemoteModule>& modules){
    Handle snapshot(CreateToolhelp32Snapshot(TH32CS_SNAPMODULE|TH32CS_SNAPMODULE32,pid));
    if(snapshot.get()==INVALID_HANDLE_VALUE)return ErrorOr();
    MODULEENTRY32W entry{};entry.dwSize=sizeof(entry);
    if(!Module32FirstW(snapshot.get(),&entry))return ErrorOr();
    do{modules.push_back({reinterpret_cast<uintptr_t>(entry.modBaseAddr),entry.szExePath});}while(Module32NextW(snapshot.get(),&entry));
    return ERROR_SUCCESS;
}
static uintptr_t FindModule(const std::vector<RemoteModule>& modules,const wchar_t* name,bool exact){
    for(const auto& module:modules)if(!_wcsicmp(exact?module.path.c_str():BaseName(module.path.c_str()),exact?name:BaseName(name)))return module.base;
    return 0;
}
static uintptr_t RemoteSystemFunction(const std::vector<RemoteModule>& modules,const char* name){
    FARPROC function=GetProcAddress(GetModuleHandleW(L"kernel32.dll"),name);if(!function)return 0;
    HMODULE owner=nullptr;
    if(!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS|GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,reinterpret_cast<LPCWSTR>(function),&owner))return 0;
    wchar_t path[32768]{};if(!GetModuleFileNameW(owner,path,static_cast<DWORD>(std::size(path))))return 0;
    uintptr_t target=FindModule(modules,path,false);
    return target?target+reinterpret_cast<uintptr_t>(function)-reinterpret_cast<uintptr_t>(owner):0;
}
static uintptr_t RemoteExport(HANDLE process,uintptr_t module,const char* wanted){
    auto read=[&](uintptr_t address,void* data,SIZE_T size){SIZE_T got=0;return ReadProcessMemory(process,reinterpret_cast<void*>(address),data,size,&got)&&got==size;};
    IMAGE_DOS_HEADER dos{};if(!read(module,&dos,sizeof(dos))||dos.e_magic!=IMAGE_DOS_SIGNATURE||dos.e_lfanew<=0||dos.e_lfanew>1048576)return 0;
    IMAGE_NT_HEADERS64 nt{};if(!read(module+dos.e_lfanew,&nt,sizeof(nt))||nt.Signature!=IMAGE_NT_SIGNATURE||nt.OptionalHeader.Magic!=IMAGE_NT_OPTIONAL_HDR64_MAGIC)return 0;
    auto directory=nt.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_EXPORT];
    if(!directory.VirtualAddress||directory.VirtualAddress>=nt.OptionalHeader.SizeOfImage)return 0;
    IMAGE_EXPORT_DIRECTORY entries{};if(!read(module+directory.VirtualAddress,&entries,sizeof(entries))||entries.NumberOfNames>4096||entries.NumberOfFunctions>4096)return 0;
    std::vector<DWORD> names(entries.NumberOfNames),functions(entries.NumberOfFunctions);std::vector<WORD> ordinals(entries.NumberOfNames);
    if(!read(module+entries.AddressOfNames,names.data(),names.size()*sizeof(DWORD))||!read(module+entries.AddressOfFunctions,functions.data(),functions.size()*sizeof(DWORD))||!read(module+entries.AddressOfNameOrdinals,ordinals.data(),ordinals.size()*sizeof(WORD)))return 0;
    for(size_t i=0;i<names.size();++i){
        char name[128]{};if(names[i]>=nt.OptionalHeader.SizeOfImage||!read(module+names[i],name,sizeof(name)-1))continue;
        if(strcmp(name,wanted)||ordinals[i]>=functions.size())continue;
        DWORD rva=functions[ordinals[i]];
        if(rva>=nt.OptionalHeader.SizeOfImage||(rva>=directory.VirtualAddress&&rva-directory.VirtualAddress<directory.Size))return 0;
        return module+rva;
    }
    return 0;
}
static DWORD RemoteCall(HANDLE process,uintptr_t function,void* parameter,DWORD timeout,DWORD& returned,bool& pending){
    pending=false;returned=0;
    Handle thread(CreateRemoteThread(process,nullptr,0,reinterpret_cast<LPTHREAD_START_ROUTINE>(function),parameter,0,nullptr));
    if(!thread)return ErrorOr();
    DWORD waited=WaitForSingleObject(thread.get(),timeout);
    if(waited!=WAIT_OBJECT_0){pending=true;return waited==WAIT_TIMEOUT?ERROR_TIMEOUT:ErrorOr();}
    if(!GetExitCodeThread(thread.get(),&returned))return ErrorOr();
    DWORD processState=0;if(!GetExitCodeProcess(process,&processState))return ErrorOr();
    return processState==STILL_ACTIVE?ERROR_SUCCESS:ERROR_PROCESS_ABORTED;
}
static DWORD CallScalar(HANDLE process,uintptr_t module,const char* name,DWORD& returned,DWORD timeout=10000){
    uintptr_t function=RemoteExport(process,module,name);if(!function)return ERROR_PROC_NOT_FOUND;
    bool pending=false;return RemoteCall(process,function,nullptr,timeout,returned,pending);
}
static DWORD LoadAdapter(HANDLE process,DWORD pid,const std::wstring& path,uintptr_t& loaded){
    std::vector<RemoteModule> modules;DWORD error=ReadModules(pid,modules);if(error)return error;
    loaded=FindModule(modules,path.c_str(),true);if(loaded)return ERROR_SUCCESS;
    uintptr_t load=RemoteSystemFunction(modules,"LoadLibraryW");if(!load)return ERROR_PROC_NOT_FOUND;
    SIZE_T length=(path.size()+1)*sizeof(wchar_t),copied=0;
    void* parameter=VirtualAllocEx(process,nullptr,length,MEM_COMMIT|MEM_RESERVE,PAGE_READWRITE);if(!parameter)return ErrorOr();
    if(!WriteProcessMemory(process,parameter,path.c_str(),length,&copied)||copied!=length){error=ErrorOr(ERROR_WRITE_FAULT);VirtualFreeEx(process,parameter,0,MEM_RELEASE);return error;}
    DWORD returned=0;bool pending=false;error=RemoteCall(process,load,parameter,10000,returned,pending);
    if(!pending)VirtualFreeEx(process,parameter,0,MEM_RELEASE);
    if(error)return error;
    modules.clear();error=ReadModules(pid,modules);if(error)return error;
    loaded=FindModule(modules,path.c_str(),true);return loaded?ERROR_SUCCESS:ERROR_DLL_INIT_FAILED;
}
