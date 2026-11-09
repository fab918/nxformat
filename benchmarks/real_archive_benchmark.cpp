// Reader benchmark on a real archive (for example a converted Switch 2 game).
// One case per process. Build against an older reader with -DNX_NO_PARTIAL
// (read() returns a Status, no atomic_reads), and add -DNX_NO_LOOKUP without lookup().
//
//   real_archive_benchmark mount  <archive.nx>
//   real_archive_benchmark lookup <archive.nx>
//   real_archive_benchmark read   <archive.nx> <atomic|lichii|fast|partial>
//                                 <rand4k|rand64k|rand1m|seq1m|asset> <threads> <warm|cold>
//
// The selected snapshot is the one with the largest logical size. Reads use a
// working set of files in path order: 48 MiB that fit the NX cache (warm), or
// 1 GiB over the default 64 MiB cache (cold). The OS cache is warmed first.
#include "nx_session.hpp"
#include "nx_disk_source.hpp"
#include "real_zstd.hpp"
#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <functional>
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
using Clock=std::chrono::steady_clock;
bool decode(void*,const uint8_t* frame,size_t n,uint8_t* out,size_t capacity,size_t* written,size_t* consumed) {
    thread_local RealZstd decoder;
    return RealZstd::decode(&decoder,frame,n,out,capacity,written,consumed);
}
[[noreturn]] void die(const std::string& message) { std::cerr<<message<<'\n'; std::exit(1); }
double us_since(Clock::time_point start) { return std::chrono::duration<double,std::micro>(Clock::now()-start).count(); }

std::uint32_t largest_snapshot(const nx::Archive& archive) {
    std::uint32_t best=0;
    for (std::uint32_t i=0;i<archive.snapshots().size();++i)
        if (archive.snapshots()[i].logical_size>archive.snapshots()[best].logical_size) best=i;
    return best;
}
struct FileEntry { std::string path; nx::FileInfo info; };
void walk(const nx::Archive& archive,nx::DirectoryHandle dir,const std::string& prefix,std::vector<FileEntry>& files,
          std::vector<std::size_t>* dir_sizes=nullptr) {
    std::vector<std::pair<std::string,nx::DirectoryHandle>> subdirs;
    std::size_t count=0;
    archive.enumerate(dir,[&](const nx::DirectoryEntryView& e) {
        ++count;
        const auto path=prefix.empty() ? std::string(e.name) : prefix+"/"+std::string(e.name);
        if (e.type==nx::EntryType::directory) subdirs.emplace_back(path,e.directory);
        else files.push_back({path,*archive.file_info(e.file).value});
        return true;
    });
    if (dir_sizes) dir_sizes->push_back(count);
    for (auto& [path,handle]:subdirs) walk(archive,handle,path,files,dir_sizes);
}
// Consumer-style lookup used by NXEmu/Lichii today: enumerate and compare.
bool linear_lookup(const nx::Archive& archive,nx::DirectoryHandle root,std::string_view path) {
    auto dir=root;
    for (;;) {
        const auto slash=path.find('/');
        const auto name=path.substr(0,slash);
        bool found=false; nx::DirectoryEntryView hit;
        archive.enumerate(dir,[&](const nx::DirectoryEntryView& e) {
            if (e.name!=name) return true;
            hit=e; found=true; return false;
        });
        if (!found) return false;
        if (slash==std::string_view::npos) return hit.type==nx::EntryType::file;
        dir=hit.directory; path.remove_prefix(slash+1);
    }
}
#if !defined(NX_NO_LOOKUP)
bool child_lookup(const nx::Archive& archive,nx::DirectoryHandle root,std::string_view path) {
    auto dir=root;
    for (;;) {
        const auto slash=path.find('/');
        const auto hit=archive.lookup(dir,path.substr(0,slash));
        if (!hit) return false;
        if (slash==std::string_view::npos) return hit.value->type==nx::EntryType::file;
        dir=hit.value->directory; path.remove_prefix(slash+1);
    }
}
#endif

int mount_case(const std::string& path) {
    std::vector<double> open_us,mount_us;
    for (int i=0;i<7;++i) {
        auto disk=nx::DiskSource::open(std::filesystem::u8path(path)); if (!disk) die("open");
        nx::OpenOptions options; options.zstd=&decode;
        auto started=Clock::now();
        auto archive=nx::Archive::open(**disk.value,options); if (!archive) die("archive open: "+archive.error.message);
        open_us.push_back(us_since(started));
        started=Clock::now();
        if (!archive.value->mount()) die("mount");
        mount_us.push_back(us_since(started));
    }
    std::sort(open_us.begin(),open_us.end()); std::sort(mount_us.begin(),mount_us.end());
    std::printf("MOUNT,%s,open_median_us=%.0f,mount_median_us=%.0f,mount_min_us=%.0f,peak_heap=%lld\n",
        std::filesystem::u8path(path).filename().string().c_str(),open_us[3],mount_us[3],mount_us[0],
        static_cast<long long>(peak_live.load()));
    return 0;
}

// Full verification pass: wall time, allocations and peak heap.
int verify_case(const std::string& path) {
    auto disk=nx::DiskSource::open(std::filesystem::u8path(path)); if (!disk) die("open");
    nx::OpenOptions options; options.zstd=&decode;
    auto archive=nx::Archive::open(**disk.value,options); if (!archive || !archive.value->mount()) die("mount");
    counting=true; const auto allocs_before=allocations.load();
    const auto started=Clock::now();
    const auto st=archive.value->verify_all(); if (!st) die("verify: "+st.error.message);
    std::printf("VERIFY,%s,seconds=%.2f,allocs=%lld,peak_heap=%lld\n",
        std::filesystem::u8path(path).filename().string().c_str(),us_since(started)/1e6,
        static_cast<long long>(allocations.load()-allocs_before),static_cast<long long>(peak_live.load()));
    return 0;
}

int lookup_case(const std::string& path) {
    auto disk=nx::DiskSource::open(std::filesystem::u8path(path)); if (!disk) die("open");
    nx::OpenOptions options; options.zstd=&decode;
    auto archive=nx::Archive::open(**disk.value,options); if (!archive || !archive.value->mount()) die("mount");
    auto& a=*archive.value;
    const auto snapshot=largest_snapshot(a);
    const auto root=*a.root_directory(snapshot).value;
    std::vector<FileEntry> files; std::vector<std::size_t> dir_sizes;
    walk(a,root,"",files,&dir_sizes);
    const auto biggest=*std::max_element(dir_sizes.begin(),dir_sizes.end());
    const auto time=[&](auto&& lookup) {
        // Several passes over every path; the best pass is reported.
        double best=1e300;
        for (int pass=0;pass<5;++pass) {
            const auto started=Clock::now();
            for (auto& f:files) if (!lookup(f.path)) die("lookup failed: "+f.path);
            best=std::min(best,us_since(started)*1000.0/files.size());
        }
        return best;
    };
    const double linear=time([&](const std::string& p) { return linear_lookup(a,root,p); });
    const double resolve=time([&](const std::string& p) { return bool(a.resolve_file(snapshot,p)); });
    double child=-1;
#if !defined(NX_NO_LOOKUP)
    child=time([&](const std::string& p) { return child_lookup(a,root,p); });
#endif
    std::printf("LOOKUP,%s,files=%zu,dirs=%zu,largest_dir=%zu,linear_ns=%.0f,resolve_file_ns=%.0f,lookup_child_ns=%.0f\n",
        std::filesystem::u8path(path).filename().string().c_str(),files.size(),dir_sizes.size(),biggest,linear,resolve,child);
    return 0;
}

struct Reader {
    nx::Session& session; std::string mode;
    size_t read(const nx::FileInfo& info,uint64_t at,uint8_t* out,size_t length,const std::shared_ptr<nx::ReadCursor>& cursor) const {
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

int read_case(int argc,char** argv) {
    if (argc!=7) die("read <archive> <mode> <workload> <threads> <warm|cold>");
    const std::string path=argv[2], mode=argv[3], workload=argv[4], state=argv[6];
    const unsigned threads=static_cast<unsigned>(std::stoul(argv[5]));
#if defined(NX_NO_PARTIAL)
    if (mode=="partial") die("partial mode needs the new reader");
#endif
    const bool warm=state=="warm";
    auto disk=nx::DiskSource::open(std::filesystem::u8path(path)); if (!disk) die("open");
    nx::SessionOptions options;
    options.reader.zstd=&decode;
#if !defined(NX_NO_PARTIAL)
    options.reader.atomic_reads=mode=="atomic";
#endif
    if (warm) { options.reader.max_cached_block_bytes=128ull*1024*1024; options.reader.max_cached_blocks=8192; }
    auto opened=nx::Session::open(*disk.value,options); if (!opened) die("session: "+opened.error.message);
    auto& session=**opened.value;
    const auto& archive=session.archive();
    std::vector<FileEntry> all;
    walk(archive,*archive.root_directory(largest_snapshot(archive)).value,"",all);
    std::sort(all.begin(),all.end(),[](auto& x,auto& y){return x.path<y.path;});
    // Working set: files in path order until the budget, skipping empty ones.
    const uint64_t budget=warm ? 48ull*1024*1024 : 1024ull*1024*1024;
    std::vector<FileEntry> set; uint64_t set_bytes=0;
    for (auto& f:all) {
        if (!f.info.size || f.info.size>budget-set_bytes) continue;
        set.push_back(f); set_bytes+=f.info.size;
        if (set_bytes>=budget*9/10) break;
    }
    if (set.empty()) die("no files");
    const Reader reader{session,mode};
    {   // Warm the OS cache (and, for warm runs, the NX cache) with whole-file reads.
        std::vector<uint8_t> buffer(4*1024*1024);
        for (int pass=0;pass<(warm ? 1 : 1);++pass)
            for (auto& f:set)
                for (uint64_t at=0;at<f.info.size;at+=buffer.size())
                    if (!reader.read(f.info,at,buffer.data(),static_cast<size_t>(std::min<uint64_t>(buffer.size(),f.info.size-at)),{})) die("warm read");
    }
    if (!warm) {   // A fresh session: the NX cache is empty again, the OS cache stays warm.
        opened.value->reset();
    }
    auto cold=warm ? std::shared_ptr<nx::Session>{} : *nx::Session::open(*disk.value,options).value;
    nx::Session& measured=warm ? session : *cold;
    const Reader timed{measured,mode};
    size_t length=4096, total=warm ? 200000 : 50000;
    if (workload=="rand64k") { length=64*1024; total=warm ? 100000 : 20000; }
    else if (workload=="rand1m") { length=1024*1024; total=warm ? 10000 : 3000; }
    else if (workload=="seq1m") { length=1024*1024; total=warm ? 10000 : 3000; }
    else if (workload=="asset") { length=4*1024*1024; total=warm ? 20000 : 5000; }
    else if (workload!="rand4k") die("workload");
    // Files large enough for the request, weighted by size.
    std::vector<const FileEntry*> eligible; std::vector<uint64_t> weight;
    for (auto& f:set) {
        if (workload=="asset" ? f.info.size>length : f.info.size<length) continue;
        eligible.push_back(&f); weight.push_back((weight.empty() ? 0 : weight.back())+f.info.size);
    }
    if (eligible.empty()) die("no eligible files");
    const size_t per_thread=(total+threads-1)/threads;
    std::vector<std::vector<uint32_t>> samples(threads);
    std::atomic<unsigned> errors{0}, ready{0}; std::atomic<bool> go{false};
    std::atomic<uint64_t> bytes_read{0};
    const auto before=measured.stats();
    std::vector<std::thread> pool;
    for (unsigned t=0;t<threads;++t) pool.emplace_back([&,t] {
        std::vector<uint8_t> out(length);
        auto cursor=std::make_shared<nx::ReadCursor>();
        auto& mine=samples[t]; mine.reserve(per_thread);
        uint64_t seed=0x9E3779B97F4A7C15ull*(t+1);
        const auto next=[&] { seed^=seed<<13; seed^=seed>>7; seed^=seed<<17; return seed; };
        size_t seq_file=(eligible.size()*t)/threads; uint64_t seq_at=0;
        uint64_t mine_bytes=0;
        ++ready; while (!go.load()) std::this_thread::yield();
        for (size_t i=0;i<per_thread;++i) {
            const FileEntry* file; uint64_t at=0; size_t n=length;
            if (workload=="seq1m") {
                if (seq_at>=eligible[seq_file]->info.size) { seq_file=(seq_file+1)%eligible.size(); seq_at=0; }
                file=eligible[seq_file]; at=seq_at;
                n=static_cast<size_t>(std::min<uint64_t>(length,file->info.size-at)); seq_at+=n;
            } else {
                const auto pick=std::upper_bound(weight.begin(),weight.end(),next()%weight.back())-weight.begin();
                file=eligible[static_cast<size_t>(pick)];
                if (workload=="asset") n=static_cast<size_t>(file->info.size);
                else if (workload=="rand4k") at=(next()%(file->info.size/4096))*4096;
                else at=next()%(file->info.size-length+1);
            }
            const auto started=Clock::now();
            const auto got=timed.read(file->info,at,out.data(),n,cursor);
            const auto ns=std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now()-started).count();
            mine.push_back(static_cast<uint32_t>(std::min<long long>(ns,0xffffffffll)));
            if (got!=n) ++errors;
            mine_bytes+=n;
        }
        bytes_read+=mine_bytes;
    });
    while (ready.load()<threads) std::this_thread::yield();
    const auto alloc_before=allocations.load(), bytes_before=allocated_bytes.load();
    counting=true;
    const auto started=Clock::now();
    go=true;
    for (auto& t:pool) t.join();
    const double seconds=std::chrono::duration<double>(Clock::now()-started).count();
    counting=false;
    if (errors) die("read errors: "+std::to_string(errors.load()));
    std::vector<uint32_t> lat;
    for (auto& s:samples) lat.insert(lat.end(),s.begin(),s.end());
    std::sort(lat.begin(),lat.end());
    const auto pct=[&](double q) { return lat[std::min(lat.size()-1,static_cast<size_t>(q*lat.size()))]/1000.0; };
    const auto after=measured.stats();
    uint64_t private_peak=0;
#if defined(_WIN32)
    PROCESS_MEMORY_COUNTERS memory{}; memory.cb=sizeof(memory);
    if (GetProcessMemoryInfo(GetCurrentProcess(),&memory,sizeof(memory))) private_peak=memory.PeakPagefileUsage;
#endif
    const double reads=static_cast<double>(lat.size());
    std::printf("RESULT,%s,%s,%s,%u,%s,%zu,%.2f,%.2f,%.2f,%.2f,%.2f,%.1f,%.2f,%.0f,%llu,%llu,%llu,%llu\n",
        std::filesystem::u8path(path).filename().string().c_str(),mode.c_str(),workload.c_str(),threads,state.c_str(),lat.size(),
        pct(0.50),pct(0.95),pct(0.99),pct(0.999),lat.back()/1000.0,
        bytes_read.load()/seconds/1048576.0,(allocations.load()-alloc_before)/reads,(allocated_bytes.load()-bytes_before)/reads,
        static_cast<unsigned long long>(peak_live.load()),static_cast<unsigned long long>(private_peak),
        static_cast<unsigned long long>(after.cache.decompressions-before.cache.decompressions),
        static_cast<unsigned long long>(after.prefetch_submitted-before.prefetch_submitted));
    return 0;
}
}

int main(int argc,char** argv) {
    if (argc<3) die("usage: real_archive_benchmark <mount|lookup|read> <archive> ...");
    const std::string command=argv[1];
    if (command=="mount") return mount_case(argv[2]);
    if (command=="lookup") return lookup_case(argv[2]);
    if (command=="verify") return verify_case(argv[2]);
    if (command=="read") return read_case(argc,argv);
    die("unknown command");
}
