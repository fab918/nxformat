#include "nx_disk_source.hpp"
#include <iostream>
#include <fstream>
#include <iterator>
#if defined(_WIN32) && defined(_MSC_VER)
#include <winioctl.h>
#include <thread>
#include <vector>

static void check(bool value,const char* message) { if(!value)throw std::runtime_error(message); }
// Deterministic test-only injection: a guard-page read is translated by a
// vectored handler into the same IN_PAGE_ERROR Windows reports for failed IO.
// Only our private armed page is affected; unrelated exceptions pass through.
static std::uintptr_t fault_page=0;
static LONG CALLBACK inject_in_page(EXCEPTION_POINTERS* fault) {
    auto* r=fault->ExceptionRecord;
    if(r->ExceptionCode==STATUS_GUARD_PAGE_VIOLATION && r->NumberParameters>=2 &&
       r->ExceptionInformation[1]>=fault_page && r->ExceptionInformation[1]-fault_page<4096) {
        r->ExceptionCode=EXCEPTION_IN_PAGE_ERROR;r->NumberParameters=3;
        r->ExceptionInformation[0]=0;r->ExceptionInformation[2]=0xc0000185;
    }
    return EXCEPTION_CONTINUE_SEARCH;
}
static void arm_fault(const std::uint8_t* pointer) {
    fault_page=reinterpret_cast<std::uintptr_t>(pointer)&~std::uintptr_t(4095);
    DWORD old=0;
    check(VirtualProtect(reinterpret_cast<void*>(fault_page),4096,PAGE_READONLY|PAGE_GUARD,&old)!=0,"arm IO fault");
}
struct FaultSource final : nx::RandomAccessSource {
    std::uint8_t* memory=nullptr;std::size_t length=0;std::uint64_t payload=0;bool arm_after_crc=false, fast_copy=true;
    explicit FaultSource(const std::vector<std::uint8_t>& bytes):length(bytes.size()) {
        memory=static_cast<std::uint8_t*>(VirtualAlloc(nullptr,length,MEM_RESERVE|MEM_COMMIT,PAGE_READWRITE));
        check(memory!=nullptr,"allocate fault fixture");std::memcpy(memory,bytes.data(),length);
        for(unsigned i=0;i<8;++i)payload|=std::uint64_t(bytes[56+i])<<(i*8);
    }
    ~FaultSource() override {VirtualFree(memory,0,MEM_RELEASE);fault_page=0;}
    std::uint64_t size() const noexcept override{return length;}
    bool immutable_reads() const noexcept override{return true;}
    nx::ImmutableReadView immutable_view(std::uint64_t at,std::size_t n) const noexcept override {
        if(at>length || n>length-at)return {};
        return {memory+at,{},&nx::detail::MappedViews::guarded_access,fast_copy?&nx::detail::MappedViews::copy:nullptr};
    }
    bool read_at(std::uint64_t at,std::uint8_t* out,std::size_t n) override {
        return at<=length && n<=length-at && nx::detail::MappedViews::copy(memory+at,out,n);
    }
    static bool cancel(void* context) {
        auto& source=*static_cast<FaultSource*>(context);
        if(source.arm_after_crc){source.arm_after_crc=false;arm_fault(source.memory+source.payload+4096);}
        return false;
    }
};
struct RaisedFault {DWORD code;ULONG_PTR address;};
static void raise_fault(void* context) {
    auto& f=*static_cast<RaisedFault*>(context);
    ULONG_PTR parameters[3]={0,f.address,0xc0000185};
    RaiseException(f.code,0,3,parameters);
}
static void allocation_failure(void*) {throw std::bad_alloc();}
static bool unrelated_escapes(const std::uint8_t* base,std::size_t length,DWORD code,ULONG_PTR address) {
    RaisedFault f{code,address};
    __try {nx::detail::MappedViews::guarded_access(base,length,&raise_fault,&f);return false;}
    __except(GetExceptionCode()==code?EXCEPTION_EXECUTE_HANDLER:EXCEPTION_CONTINUE_SEARCH) {return true;}
}
static void reader_faults(const std::filesystem::path& fixture) {
    std::ifstream file(fixture,std::ios::binary);
    std::vector<std::uint8_t> bytes{std::istreambuf_iterator<char>(file),std::istreambuf_iterator<char>()};
    check(bytes.size()>64,"read synthetic fault fixture");
    for(bool fast:{false,true})for(bool atomic:{false,true})for(unsigned stage=0;stage<3;++stage) {
        FaultSource source(bytes);source.fast_copy=fast;nx::OpenOptions options;
        options.cache_raw_verification=true;options.atomic_reads=atomic;
        options.is_cancelled=&FaultSource::cancel;options.cancellation_context=&source;
        auto archive=nx::Archive::open(source,options);check(bool(archive) && bool(archive.value->mount()),"mount fault fixture");
        std::array<std::uint8_t,8192> output{};output.fill(0xa5);
        if(stage==2)check(bool(archive.value->read_file(nx::FileId{0},0,output.size(),output.data(),output.size())),"prime verified RAW");
        output.fill(0xa5);
        if(stage==1)source.arm_after_crc=true;else arm_fault(source.memory+source.payload+(stage==2?4096:0));
        const auto failure=archive.value->read_file(nx::FileId{0},0,output.size(),output.data(),output.size());
        check(!failure && failure.error.code==nx::ErrorCode::io && failure.bytes==0,"mapped fault did not become IO error");
        for(const auto byte:output)check(byte==(atomic?0xa5:0),"mapped fault violated output contract");
        check(!archive.value->range_verification_cached(nx::FileId{0},0,output.size()),"fault recorded unverified data");
        check(bool(archive.value->read_file(nx::FileId{0},0,output.size(),output.data(),output.size())),"retry after mapped fault");
        for(std::size_t i=0;i<output.size();++i)check(output[i]==std::uint8_t((i*31+7)%256),"retry returned wrong bytes");
    }
    for(bool atomic:{false,true}) {
        FaultSource source(bytes);nx::OpenOptions options;
        options.cache_raw_verification=true;options.atomic_reads=atomic;
        auto archive=nx::Archive::open(source,options);check(bool(archive) && bool(archive.value->mount()),"mount multi-block fault fixture");
        std::vector<std::uint8_t> output(65537,0xa5);
        arm_fault(source.memory+source.payload+65536);
        const auto failure=archive.value->read_file(nx::FileId{0},0,output.size(),output.data(),output.size());
        check(!failure && failure.error.code==nx::ErrorCode::io && failure.bytes==(atomic?0:65536),"multi-block mapped fault progress");
        for(std::size_t i=0;i<output.size();++i)
            check(output[i]==(atomic?0xa5:(i<65536?std::uint8_t((i*31+7)%256):0)),"multi-block mapped fault output");
        check(bool(archive.value->read_file(nx::FileId{0},0,output.size(),output.data(),output.size())),"multi-block mapped fault retry");
        for(std::size_t i=0;i<output.size();++i)check(output[i]==std::uint8_t((i*31+7)%256),"multi-block retry returned wrong bytes");
    }
}
int main(int argc,char** argv) try {
    check(argc==3,"temporary directory and synthetic fixture required");
    const auto handler=AddVectoredExceptionHandler(1,&inject_in_page);check(handler!=nullptr,"install fault injector");
    struct RemoveHandler {PVOID handle;~RemoveHandler(){RemoveVectoredExceptionHandler(handle);}} remove{handler};
    reader_faults(argv[2]);
    const auto file=std::filesystem::path(argv[1])/"mapped-probe.bin";
    HANDLE writer=CreateFileW(file.c_str(),GENERIC_READ|GENERIC_WRITE,0,nullptr,CREATE_NEW,FILE_ATTRIBUTE_NORMAL,nullptr);
    check(writer!=INVALID_HANDLE_VALUE,"create private sparse probe");
    DWORD returned=0;
    check(DeviceIoControl(writer,FSCTL_SET_SPARSE,nullptr,0,nullptr,0,&returned,nullptr)!=0,"mark sparse probe");
    constexpr std::uint64_t window=16ull*1024*1024;
    for(unsigned i=0;i<70;++i) {
        LARGE_INTEGER position{};position.QuadPart=static_cast<LONGLONG>(i*window);
        check(SetFilePointerEx(writer,position,nullptr,FILE_BEGIN)!=0,"position marker");
        const auto byte=static_cast<unsigned char>(i+1);
        check(WriteFile(writer,&byte,1,&returned,nullptr)!=0 && returned==1,"write marker");
    }
    CloseHandle(writer);
    auto opened=nx::DiskSource::open_mapped(file);
    check(bool(opened),"open mapped source");
    auto source=std::move(*opened.value);
    check(source->immutable_reads(),"Windows source is not pinned immutable");
    auto first=source->immutable_view(0,1);
    check(first.data && first.owner && first.data[0]==1,"initial lease");
    for(unsigned i=1;i<70;++i) {
        auto view=source->immutable_view(i*window,1);
        check(view.data && view.data[0]==i+1,"window marker");
    }
    check(first.data[0]==1,"eviction invalidated a pinned view");
    auto live=source->immutable_view(0,1);
    arm_fault(live.data);
    std::uint8_t failed_byte=0xa5;
    check(!source->read_at(0,&failed_byte,1),"mapped read_at fault escaped IO handling");
    check(source->read_at(0,&failed_byte,1) && failed_byte==1,"mapped read_at fault retry failed");
    check(unrelated_escapes(first.data,1,EXCEPTION_IN_PAGE_ERROR,reinterpret_cast<ULONG_PTR>(first.data)+1),"unrelated mapped IO fault swallowed");
    check(unrelated_escapes(first.data,1,EXCEPTION_ACCESS_VIOLATION,reinterpret_cast<ULONG_PTR>(first.data)),"access violation swallowed");
    bool propagated=false;
    try {nx::detail::MappedViews::guarded_access(first.data,1,&allocation_failure,nullptr);}
    catch(const std::bad_alloc&) {propagated=true;}
    check(propagated,"C++ allocation exception was swallowed by mapped guard");
    fault_page=0;live={};
    std::array<std::uint8_t,6> crossing{};
    check(source->read_at(window-2,crossing.data(),crossing.size()) && crossing[2]==2,"cross-window read");
    check(!source->immutable_view(source->size(),1).data,"out-of-range view accepted");
    check(source->read_at(source->size(),nullptr,0),"zero-byte EOF read");
    std::atomic<unsigned> errors{0};
    std::vector<std::thread> threads;
    for(unsigned t=0;t<4;++t)threads.emplace_back([&,t]{
        for(unsigned i=0;i<200;++i) {
            const unsigned index=(i*17+t*11)%70;
            std::uint8_t byte=0;
            if(!source->read_at(index*window,&byte,1) || byte!=index+1)++errors;
        }
    });
    for(auto& thread:threads)thread.join();
    check(errors==0,"concurrent mapping eviction failed");
    auto cannot_write=[&]{
        const auto handle=CreateFileW(file.c_str(),GENERIC_WRITE,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,
                                     nullptr,OPEN_EXISTING,0,nullptr);
        if(handle!=INVALID_HANDLE_VALUE) { CloseHandle(handle);return false; }
        return GetLastError()==ERROR_SHARING_VIOLATION;
    };
    check(cannot_write(),"writer not excluded");
    source.reset();
    check(first.data[0]==1 && cannot_write(),"lease lost immutable file pin after source destruction");
    first={};
    writer=CreateFileW(file.c_str(),GENERIC_WRITE,0,nullptr,OPEN_EXISTING,0,nullptr);
    check(writer!=INVALID_HANDLE_VALUE,"file pin leaked after last lease");
    CloseHandle(writer);
    check(std::filesystem::remove(file),"probe file cleanup failed");
    std::cout<<"PASS bounded Windows mappings: 70 windows, pinned eviction, cross-window reads, 4 threads, write denial, lease lifetime and guarded IO/CRC/copy faults\n";
    return 0;
} catch(const std::exception& error) { std::cerr<<error.what()<<'\n';return 1; }
#else
int main() {
    const auto mapped=nx::DiskSource::open_mapped("unused-mapping-probe-path");
    if(mapped || mapped.error.code!=nx::ErrorCode::unsupported) {
        std::cerr<<"unprotected mapped source was accepted\n";return 1;
    }
    std::cout<<"PASS unsupported mapping rejected without Windows/MSVC SEH (ordinary reader unchanged)\n";
}
#endif
