// SPDX-License-Identifier: MIT
// Benchmark-only batch planner and existing Session prefetch policies.
#include "nx_disk_source.hpp"
#include "nx_session.hpp"
#include "real_zstd.hpp"
#include <fstream>
#include <sstream>
#include <iostream>
#include <numeric>
#include <map>
#include <chrono>
#if defined(_WIN32)
#include <psapi.h>
#pragma comment(lib,"psapi.lib")
#endif
using Clock=std::chrono::steady_clock;
struct Decoder {
    std::atomic<uint64_t> calls{0};
    static bool decode(void* p,const uint8_t* in,size_t n,uint8_t* out,size_t cap,size_t* written,size_t* consumed) {
        ++static_cast<Decoder*>(p)->calls;
        thread_local RealZstd z;
        return RealZstd::decode(&z,in,n,out,cap,written,consumed);
    }
};
struct Request { nx::FileId file;uint64_t offset;size_t size;unsigned chunk;std::shared_ptr<nx::ReadCursor> cursor; };
int main(int argc,char** argv) try {
    if(argc!=8 && argc!=9)throw std::runtime_error("NX TRACE MODE CACHE_MIB THINK_US OUTPUT BATCH");
    auto disk=nx::DiskSource::open_mapped(argv[1]);if(!disk)throw std::runtime_error("disk open");
    const std::string mode=argv[3];
    if(mode!="off" && mode!="batch" && mode!="prefetch" && mode!="guarded" && mode!="adaptive")throw std::runtime_error("mode");
    Decoder decoder;nx::SessionOptions options;
    options.reader.zstd=&Decoder::decode;options.reader.zstd_context=&decoder;
    options.reader.cache_raw_verification=true;
    options.reader.max_cached_block_bytes=std::stoull(argv[4])*1024*1024;
    options.reader.max_cached_blocks=4096;options.split_reads=false;
    options.sequential_prefetch=mode=="prefetch" || mode=="guarded";
    options.sequential_reads=mode=="guarded"?8:3;options.lookahead_blocks=1;options.decode_workers=1;
    auto opened=nx::Session::open(*disk.value,options);if(!opened)throw std::runtime_error("session open");
    auto session=std::move(*opened.value);opened.value.reset();
    std::vector<Request> requests;std::ifstream input(argv[2]);std::string line;
    std::map<uint32_t,std::shared_ptr<nx::ReadCursor>> cursors;
    while(std::getline(input,line)) {
        std::istringstream row(line);std::string path;uint64_t offset;size_t size;
        std::getline(row,path,'\t');row>>offset>>size;if(!row || !size || size>1024*1024)throw std::runtime_error("trace");
        auto file=session->archive().resolve_file(0,path);if(!file)throw std::runtime_error("resolve");
        auto& cursor=cursors[file.value->id.value];if(!cursor)cursor=std::make_shared<nx::ReadCursor>();
        requests.push_back({file.value->id,offset,size,file.value->chunk_log2,cursor});
    }
    if(requests.empty())throw std::runtime_error("empty trace");
    uint64_t compressed_requests=0;
    for(const auto& request:requests)if(session->archive().block_needs_decode(request.file,request.offset))++compressed_requests;
    if(argc==9 && std::string(argv[8])=="warm") {
        std::vector<uint8_t> warm;
        for(const auto& request:requests) {
            warm.resize(request.size);
            if(!session->read(request.file,request.offset,request.size,warm.data(),warm.size()))throw std::runtime_error("warm read");
        }
    }
    const auto decodes_before=decoder.calls.load();
    // Keep a common full-output buffer so validation IO cannot give background
    // workers unmeasured idle time between requests. Copying into it is timed.
    size_t total_output=0;
    for(const auto& request:requests)total_output+=request.size;
    std::vector<uint8_t> full_output(total_output);
    size_t output_at=0;
    const size_t horizon=std::stoull(argv[7]);if(!horizon || horizon>64)throw std::runtime_error("batch bound");
    const auto think=std::chrono::microseconds(std::stoull(argv[5]));
    uint64_t service_ns=0,work_ns=0,calls=0,requested=0,merged_bytes=0,scratch_peak=0,grouped_windows=0;
    std::vector<std::vector<uint8_t>> buffers(horizon);
    std::vector<uint8_t> merged;
    std::vector<size_t> order(horizon);
    // Same timing window and pending buffers for every mode.
    const size_t step=horizon;
    for(size_t first=0;first<requests.size();first+=step) {
        const auto count=std::min(step,requests.size()-first);
        for(size_t i=0;i<count;++i)buffers[i].resize(requests[first+i].size);
        auto start=Clock::now();
        order.resize(count);std::iota(order.begin(),order.end(),0);
        bool group=mode=="batch";
        if(mode=="adaptive" && count>=3) {
            const auto& a=requests[first];const auto& b=requests[first+1];const auto& c=requests[first+2];
            group=a.file.value==b.file.value && a.file.value==c.file.value &&
                (a.offset>>a.chunk)==(b.offset>>b.chunk) && (a.offset>>a.chunk)==(c.offset>>c.chunk);
        }
        if(group)++grouped_windows;
        if(group)std::sort(order.begin(),order.end(),[&](size_t a,size_t b) {
            const auto& x=requests[first+a];const auto& y=requests[first+b];
            return std::make_pair(x.file.value,x.offset)<std::make_pair(y.file.value,y.offset);
        });
        for(size_t pos=0;pos<count;) {
            const auto i=order[pos];const auto& r=requests[first+i];
            size_t end=pos+1;auto last=r.offset+r.size;
            const auto chunk_end=((r.offset>>r.chunk)+1)<<r.chunk;
            if(group && last<=chunk_end) {
                while(end<count) {
                    const auto& next=requests[first+order[end]];
                    if(next.file.value!=r.file.value || next.offset<r.offset || next.offset+next.size>chunk_end)break;
                    last=std::max(last,next.offset+next.size);++end;
                }
            }
            if(mode=="adaptive" && end>pos+1) {
                size_t useful=0;for(auto j=pos;j<end;++j)useful+=requests[first+order[j]].size;
                if(last-r.offset>useful+useful/4) {end=pos+1;last=r.offset+r.size;}
            }
            if(end>pos+1) {
                merged.resize(static_cast<size_t>(last-r.offset));scratch_peak=std::max<uint64_t>(scratch_peak,merged.capacity());
                if(!session->read(r.file,r.offset,merged.size(),merged.data(),merged.size()))throw std::runtime_error("merged read");
                ++calls;merged_bytes+=merged.size();
                for(auto j=pos;j<end;++j) {
                    auto k=order[j];const auto& part=requests[first+k];
                    std::memcpy(buffers[k].data(),merged.data()+part.offset-r.offset,part.size);
                }
            } else {
                if(!session->read(r.file,r.offset,r.size,buffers[i].data(),buffers[i].size(),r.cursor))throw std::runtime_error("read");
                ++calls;merged_bytes+=r.size;
            }
            pos=end;
        }
        for(size_t i=0;i<count;++i) {
            std::memcpy(full_output.data()+output_at,buffers[i].data(),buffers[i].size());
            output_at+=buffers[i].size();requested+=buffers[i].size();
        }
        auto finish=Clock::now();service_ns+=std::chrono::duration_cast<std::chrono::nanoseconds>(finish-start).count();
        // Synthetic caller work after each pending window (think per logical request).
        const auto until=Clock::now()+think*count;while(Clock::now()<until) {}
        work_ns+=std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now()-start).count();
    }
    auto stats=session->stats();auto before=Clock::now();session.reset();
    auto drain=std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now()-before).count();
    uint64_t resident=0,commit=0,cpu_ticks=0;
#if defined(_WIN32)
    FILETIME created{},ended{},kernel{},user{};GetProcessTimes(GetCurrentProcess(),&created,&ended,&kernel,&user);
    cpu_ticks=(uint64_t(kernel.dwHighDateTime)<<32)+kernel.dwLowDateTime+(uint64_t(user.dwHighDateTime)<<32)+user.dwLowDateTime;
    PROCESS_MEMORY_COUNTERS_EX memory{};GetProcessMemoryInfo(GetCurrentProcess(),reinterpret_cast<PROCESS_MEMORY_COUNTERS*>(&memory),sizeof(memory));resident=memory.PeakWorkingSetSize;commit=memory.PeakPagefileUsage;
#endif
    std::cout<<"{\"service_ms\":"<<service_ns/1e6<<",\"shutdown_ms\":"<<drain/1e6<<",\"service_and_shutdown_ms\":"<<(service_ns+drain)/1e6<<",\"cpu_ms\":"<<cpu_ticks/1e4<<",\"work_ms\":"<<work_ns/1e6<<",\"decodes\":"<<(decoder.calls.load()-decodes_before)<<",\"initial_compressed_requests\":"<<compressed_requests<<",\"source_bytes\":"<<(*disk.value)->bytes_read()<<",\"prefetch_submitted\":"<<stats.prefetch_submitted<<",\"prefetch_cancelled\":"<<stats.prefetch_cancelled<<",\"prefetch_dropped\":"<<stats.prefetch_dropped<<",\"payload_peak\":"<<stats.cache.peak_bytes<<",\"resident\":"<<resident<<",\"private_commit\":"<<commit<<",\"calls\":"<<calls<<",\"grouped_windows\":"<<grouped_windows<<",\"requested_bytes\":"<<requested<<",\"merged_bytes\":"<<merged_bytes<<",\"merge_scratch_peak\":"<<scratch_peak<<"}\n";
    std::ofstream output(argv[6],std::ios::binary);
    output.write(reinterpret_cast<const char*>(full_output.data()),full_output.size());
    if(!output.good())throw std::runtime_error("output");return 0;
} catch(const std::exception& e) {std::cerr<<e.what()<<'\n';return 1;}
