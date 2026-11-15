#include "nx_disk_source.hpp"
#include <iostream>
#if defined(_WIN32)
#include <winioctl.h>
#include <thread>
#include <vector>

static void check(bool value,const char* message) { if(!value)throw std::runtime_error(message); }
int main(int argc,char** argv) try {
    check(argc==2,"temporary directory required");
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
    std::cout<<"PASS bounded Windows mappings: 70 windows, pinned eviction, cross-window reads, 4 threads, write denial and lease lifetime\n";
    return 0;
} catch(const std::exception& error) { std::cerr<<error.what()<<'\n';return 1; }
#else
int main() { std::cout<<"SKIP Windows-only mapping probe (ordinary POSIX disk reader unchanged)\n"; }
#endif
