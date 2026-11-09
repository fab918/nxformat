// Multi-block read benchmark: atomic Session::read, Lichii-style one block
// per call, and Session::read with its byte count. One case per process, so peak memory
// belongs to that case. Build against an older reader with -DNX_NO_PARTIAL.
//
// usage: partial_read_benchmark <archive.nx> <atomic|lichii|partial>
//        <rand4k|rand256k|rand4m|seq1m> <threads> <warm|cold>
// warm: the whole file fits in the NX block cache and is loaded first.
// cold: an 8 MiB NX cache over a 64 MiB file, so most reads miss. The OS file
// cache is warm in both (the archive is read once before measuring).
#include "nx_session.hpp"
#include "nx_disk_source.hpp"
#include "real_zstd.hpp"
#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <iostream>
#include <string>
#if defined(_WIN32)
#include <malloc.h>
#include <psapi.h>
#pragma comment(lib,"psapi.lib")
#endif

namespace {
std::atomic<bool> counting{false};
std::atomic<std::uint64_t> allocations{0}, allocated_bytes{0};
std::atomic<std::int64_t> live_bytes{0}, peak_live{0};
void note_alloc(void* p) {
#if defined(_WIN32)
    const auto size=static_cast<std::int64_t>(_msize(p));
    const auto now=live_bytes.fetch_add(size)+size;
    auto old=peak_live.load(); while (now>old && !peak_live.compare_exchange_weak(old,now)) {}
    if (counting.load(std::memory_order_relaxed)) { ++allocations; allocated_bytes+=static_cast<std::uint64_t>(size); }
#else
    (void)p; if (counting.load(std::memory_order_relaxed)) ++allocations;
#endif
}
}
void* operator new(std::size_t size) {
    void* p=std::malloc(size ? size : 1);
    if (!p) throw std::bad_alloc();
    note_alloc(p);
    return p;
}
void* operator new[](std::size_t size) { return ::operator new(size); }
void operator delete(void* p) noexcept {
#if defined(_WIN32)
    if (p) live_bytes-=static_cast<std::int64_t>(_msize(p));
#endif
    std::free(p);
}
void operator delete[](void* p) noexcept { ::operator delete(p); }
void operator delete(void* p,std::size_t) noexcept { ::operator delete(p); }
void operator delete[](void* p,std::size_t) noexcept { ::operator delete(p); }

namespace {
bool decode(void*,const uint8_t* frame,size_t n,uint8_t* out,size_t capacity,size_t* written,size_t* consumed) {
    thread_local RealZstd decoder;
    return RealZstd::decode(&decoder,frame,n,out,capacity,written,consumed);
}
[[noreturn]] void die(const std::string& message) { std::cerr<<message<<'\n'; std::exit(1); }

struct Reader {
    nx::Session& session; nx::FileInfo info; std::string mode;
    // Returns the byte count, like an emulator VFS Read().
    size_t read(uint64_t at,uint8_t* out,size_t length,const std::shared_ptr<nx::ReadCursor>& cursor) const {
        const uint64_t chunk=1ull<<info.chunk_log2;
        size_t done=0;
        while (done<length) {
            const uint64_t position=at+done;
            size_t part=static_cast<size_t>(std::min<uint64_t>(length-done,session.max_read_bytes()));
            if (mode=="lichii") part=static_cast<size_t>(std::min<uint64_t>(part,chunk-position%chunk));
#if !defined(NX_NO_PARTIAL)
            if (mode=="partial") {
                const auto r=session.read(info.id,position,part,out+done,part,cursor);
                done+=r.bytes;
                if (!r) break;
                continue;
            }
#endif
            if (!session.read(info.id,position,part,out+done,part,cursor)) break;
            done+=part;
        }
        return done;
    }
};
// Page p (4 KiB) starts with p as a little-endian u32.
bool check_bytes(const uint8_t* out,uint64_t at,size_t n) {
    for (uint64_t page=(at+4095)/4096; page*4096+4<=at+n; page+=17) {
        const auto* p=out+(page*4096-at);
        if ((uint32_t(p[0])|uint32_t(p[1])<<8|uint32_t(p[2])<<16|uint32_t(p[3])<<24)!=page) return false;
    }
    return true;
}
}

int main(int argc,char** argv) {
    if (argc!=6) die("usage: partial_read_benchmark <archive> <mode> <workload> <threads> <warm|cold>");
    const std::string path=argv[1], mode=argv[2], workload=argv[3], state=argv[5];
    const unsigned threads=static_cast<unsigned>(std::stoul(argv[4]));
#if defined(NX_NO_PARTIAL)
    if (mode=="partial") die("partial mode needs the new reader");
#endif
    auto disk=nx::DiskSource::open(std::filesystem::u8path(path)); if (!disk) die("open");
    {   // Warm the OS file cache only.
        std::vector<uint8_t> chunk(4*1024*1024);
        for (uint64_t at=0; at<(*disk.value)->size(); at+=chunk.size()) {
            const auto n=static_cast<size_t>(std::min<uint64_t>(chunk.size(),(*disk.value)->size()-at));
            if (!(*disk.value)->read_at(at,chunk.data(),n)) die("warm OS cache");
        }
    }
    nx::SessionOptions options;
    options.reader.zstd=&decode;
#if !defined(NX_NO_PARTIAL)
    options.reader.atomic_reads=mode=="atomic";  // other modes use the default (fast) read()
#endif
    if (state=="warm") { options.reader.max_cached_block_bytes=128ull*1024*1024; options.reader.max_cached_blocks=4096; }
    else options.reader.max_cached_block_bytes=8ull*1024*1024;
    auto opened=nx::Session::open(*disk.value,options); if (!opened) die("session: "+opened.error.message);
    auto& session=**opened.value;
    auto info=session.archive().resolve_file(0,"payload.bin"); if (!info) die("resolve");
    const Reader reader{session,*info.value,mode};
    const uint64_t size=info.value->size;
    if (state=="warm") {
        std::vector<uint8_t> all(4*1024*1024);
        for (uint64_t at=0; at<size; at+=all.size())
            if (reader.read(at,all.data(),static_cast<size_t>(std::min<uint64_t>(all.size(),size-at)),{})==0) die("warm NX cache");
    }
    // Enough reads for a stable p99.9 in warm runs; cold reads cost more.
    const bool warm=state=="warm";
    size_t length=4096, total=warm ? 200000 : 20000;
    if (workload=="rand256k") { length=256*1024; total=warm ? 50000 : 10000; }
    else if (workload=="rand4m") { length=4*1024*1024; total=warm ? 2000 : 500; }
    else if (workload=="seq1m") { length=1024*1024; total=warm ? 20000 : 2000; }
    else if (workload!="rand4k") die("workload");
    const size_t per_thread=(total+threads-1)/threads;
    std::vector<std::vector<uint32_t>> samples(threads);
    std::atomic<unsigned> errors{0}, ready{0};
    std::atomic<bool> go{false};
    const auto before=session.stats();
    std::vector<std::thread> pool;
    for (unsigned t=0;t<threads;++t) pool.emplace_back([&,t] {
        std::vector<uint8_t> out(length);
        auto cursor=std::make_shared<nx::ReadCursor>();
        auto& mine=samples[t]; mine.reserve(per_thread);
        uint64_t seed=0x9E3779B97F4A7C15ull*(t+1);
        const auto next=[&] { seed^=seed<<13; seed^=seed>>7; seed^=seed<<17; return seed; };
        uint64_t sequential=(size/threads)*t/(1024*1024)*(1024*1024);
        ++ready; while (!go.load()) std::this_thread::yield();
        for (size_t i=0;i<per_thread;++i) {
            uint64_t at;
            if (workload=="rand4k") at=(next()%(size/4096))*4096;
            else if (workload=="seq1m") { if (sequential+length>size) sequential=0; at=sequential; sequential+=length; }
            else at=next()%(size-length+1);
            const auto started=std::chrono::steady_clock::now();
            const auto got=reader.read(at,out.data(),length,cursor);
            const auto ns=std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now()-started).count();
            mine.push_back(static_cast<uint32_t>(std::min<long long>(ns,0xffffffffll)));
            if (got!=length || !check_bytes(out.data(),at,length)) ++errors;
        }
    });
    while (ready.load()<threads) std::this_thread::yield();
    const auto alloc_before=allocations.load(), bytes_before=allocated_bytes.load();
    counting=true;
    const auto started=std::chrono::steady_clock::now();
    go=true;
    for (auto& t:pool) t.join();
    const double seconds=std::chrono::duration<double>(std::chrono::steady_clock::now()-started).count();
    counting=false;
    if (errors) die("read errors: "+std::to_string(errors.load()));
    std::vector<uint32_t> all;
    for (auto& s:samples) all.insert(all.end(),s.begin(),s.end());
    std::sort(all.begin(),all.end());
    const auto pct=[&](double q) { return all[std::min(all.size()-1,static_cast<size_t>(q*all.size()))]/1000.0; };
    const auto after=session.stats();
    uint64_t private_peak=0;
#if defined(_WIN32)
    PROCESS_MEMORY_COUNTERS memory{}; memory.cb=sizeof(memory);
    if (GetProcessMemoryInfo(GetCurrentProcess(),&memory,sizeof(memory))) private_peak=memory.PeakPagefileUsage;
#endif
    const double reads=static_cast<double>(all.size());
    std::printf("RESULT,%s,%s,%s,%u,%s,%zu,%.2f,%.2f,%.2f,%.2f,%.2f,%.1f,%.2f,%.0f,%llu,%llu,%llu,%llu\n",
        std::filesystem::u8path(path).filename().string().c_str(),mode.c_str(),workload.c_str(),threads,state.c_str(),all.size(),
        pct(0.50),pct(0.95),pct(0.99),pct(0.999),all.back()/1000.0,
        reads*length/seconds/1048576.0,(allocations.load()-alloc_before)/reads,(allocated_bytes.load()-bytes_before)/reads,
        static_cast<unsigned long long>(peak_live.load()),static_cast<unsigned long long>(private_peak),
        static_cast<unsigned long long>(after.cache.decompressions-before.cache.decompressions),
        static_cast<unsigned long long>(after.prefetch_submitted-before.prefetch_submitted));
    return 0;
}
