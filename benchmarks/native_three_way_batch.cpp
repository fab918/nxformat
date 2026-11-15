// SPDX-License-Identifier: GPL-3.0-or-later
// Reuse the unmodified external native NUP adapter and the same mapped provider.
#define main nx_single_benchmark_main
#include "native_nup_comparison.cpp"
#undef main
#include <numeric>
int main(int argc,char** argv) try {
    if(argc!=9)throw std::runtime_error("NUP NX TRACE OUTPUT CACHE_MIB SPILL MODE WARM");
    const std::string mode=argv[7];const bool nup_mode=mode=="nup";
    if(!nup_mode && mode!="main" && mode!="current" && mode!="selective")throw std::runtime_error("mode");
    const auto budget=std::stoull(argv[5])*1024*1024;
    std::shared_ptr<Input> source;
    std::unique_ptr<FileSys::NupContainer> nup;
    FileSys::VirtualDir romfs;
    std::optional<nx::Archive> archive;
    auto* decoder=nup_mode?nullptr:ZSTD_createDCtx();
    if(!nup_mode && !decoder)throw std::runtime_error("decoder");
    if(nup_mode) {
        source=std::make_shared<Input>(argv[1]);
        nup=std::make_unique<FileSys::NupContainer>(source,budget,argv[6]);
        if(nup->GetStatus()!=Loader::ResultStatus::Success)throw std::runtime_error("NUP open");
        auto base=nup->FindBasePackage();if(!base)throw std::runtime_error("base");
        romfs=nup->GetRomFSDir(*base);
    } else {
        source=std::make_shared<Input>(argv[2]);nx::OpenOptions options;
        options.zstd=decode;options.zstd_context=decoder;options.collect_read_stats=true;
        options.max_cached_block_bytes=budget;options.max_cached_blocks=4096;
#if !defined(NX_BENCH_ORIGINAL)
        options.cache_raw_verification=true;
#endif
        auto opened=nx::Archive::open(*source,options);if(!opened)throw std::runtime_error("NX open");
        archive.emplace(std::move(*opened.value));if(!archive->mount())throw std::runtime_error("NX mount");
    }
    struct Request {nx::FileId file{};uint64_t offset;size_t size;unsigned chunk;FileSys::VirtualFile nup_file;};
    std::vector<Request> requests;std::ifstream input(argv[3]);std::string line;
    while(std::getline(input,line)) {
        std::istringstream row(line);std::string path;uint64_t offset;size_t size;
        std::getline(row,path,'\t');row>>offset>>size;if(!row || !size)throw std::runtime_error("trace");
        Request r{};r.offset=offset;r.size=size;
        if(nup_mode) {r.nup_file=romfs->GetFileRelative(path.substr(6));if(!r.nup_file)throw std::runtime_error("resolve NUP");}
        else {auto file=archive->resolve_file(0,path);if(!file)throw std::runtime_error("resolve NX");r.file=file.value->id;r.chunk=file.value->chunk_log2;}
        requests.push_back(std::move(r));
    }
    if(requests.empty())throw std::runtime_error("empty trace");
    const auto read=[&](const Request& r,uint64_t at,size_t length,uint8_t* bytes,size_t capacity) {
        if(length>capacity)return false;
        return nup_mode?r.nup_file->Read(bytes,length,at)==length:bool(archive->read_file(r.file,at,length,bytes,capacity));
    };
    if(std::string(argv[8])=="1") {
        std::vector<uint8_t> warm;
        for(const auto& r:requests) {warm.resize(r.size);if(!read(r,r.offset,r.size,warm.data(),warm.size()))throw std::runtime_error("warm");}
    }
    using Clock=std::chrono::steady_clock;
    // Keep a common full-output buffer so validation IO cannot give background
    // workers unmeasured idle time between requests. Copying into it is timed.
    size_t total_output=0;
    for(const auto& request:requests)total_output+=request.size;
    std::vector<uint8_t> full_output(total_output);
    size_t output_at=0;
    const size_t horizon=std::stoull("16");if(!horizon || horizon>64)throw std::runtime_error("batch bound");
    const auto think=std::chrono::microseconds(0);
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
        bool group=false;
        if(mode=="selective" && count>=3) {
            const auto& a=requests[first];const auto& b=requests[first+1];const auto& c=requests[first+2];
            group=a.file.value==b.file.value && a.file.value==c.file.value &&
                (a.offset>>a.chunk)==(b.offset>>b.chunk) && (a.offset>>a.chunk)==(c.offset>>c.chunk);
        }
        // Large reads already verified/cached do not benefit from another
        // merged buffer and scatter copy. The hint is never used to skip
        // checks: each chosen path still goes through Session::read().
#if !defined(NX_BENCH_ORIGINAL)
        if(mode=="selective" && group && requests[first].size>4096) {
            bool all_cached=true;
            for(size_t i=0;i<count;++i) {
                const auto& r=requests[first+i];
                if(!archive->range_verification_cached(r.file,r.offset,r.size)) {all_cached=false;break;}
            }
            if(all_cached)group=false;
        }
#endif
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
            if(mode=="selective" && end>pos+1) {
                size_t useful=0;for(auto j=pos;j<end;++j)useful+=requests[first+order[j]].size;
                if(last-r.offset>useful+useful/4) {end=pos+1;last=r.offset+r.size;}
            }
            if(end>pos+1) {
                merged.resize(static_cast<size_t>(last-r.offset));scratch_peak=std::max<uint64_t>(scratch_peak,merged.capacity());
                if(!read(r,r.offset,merged.size(),merged.data(),merged.size()))throw std::runtime_error("merged read");
                ++calls;merged_bytes+=merged.size();
                for(auto j=pos;j<end;++j) {
                    auto k=order[j];const auto& part=requests[first+k];
                    std::memcpy(buffers[k].data(),merged.data()+part.offset-r.offset,part.size);
                }
            } else {
                if(!read(r,r.offset,r.size,buffers[i].data(),buffers[i].size()))throw std::runtime_error("read");
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

    PROCESS_MEMORY_COUNTERS_EX memory{};GetProcessMemoryInfo(GetCurrentProcess(),reinterpret_cast<PROCESS_MEMORY_COUNTERS*>(&memory),sizeof(memory));
    auto stats=nup_mode?nx::CacheStats{}:archive->cache_stats();
    uint64_t spill=0;for(const auto& file:std::filesystem::directory_iterator(argv[6]))if(file.is_regular_file())spill+=file.file_size();
    std::cout<<"{\"service_ms\":"<<service_ns/1e6<<",\"resident\":"<<memory.PeakWorkingSetSize<<",\"private_commit\":"<<memory.PeakPagefileUsage<<",\"spill_bytes\":"<<spill<<",\"calls\":"<<calls<<",\"grouped_windows\":"<<grouped_windows<<",\"merged_bytes\":"<<merged_bytes<<",\"requested_bytes\":"<<requested<<",\"merge_scratch_peak\":"<<scratch_peak<<",\"payload_peak\":"<<stats.peak_bytes;
#if !defined(NX_BENCH_ORIGINAL)
    std::cout<<",\"verification_bytes\":"<<stats.raw_verification_bytes;
#endif
    std::cout<<"}\n";
    std::ofstream output(argv[4],std::ios::binary);output.write(reinterpret_cast<const char*>(full_output.data()),full_output.size());
    ZSTD_freeDCtx(decoder);return output.good()?0:1;
} catch(const std::exception& e) {std::cerr<<e.what()<<'\n';return 1;}
