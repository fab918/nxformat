#include "nx_session.hpp"
#include "nx_disk_source.hpp"
#include "real_zstd.hpp"
#include <iostream>
#include <stdexcept>
#include <atomic>
#include <condition_variable>
#if defined(_WIN32)
#include <psapi.h>
#pragma comment(lib,"psapi.lib")
#else
#include <sys/resource.h>
#endif

using namespace std::chrono_literals;
namespace {
void check(bool value,const char* message) { if (!value) throw std::runtime_error(message); }
struct Decoder {
    std::atomic<unsigned> delay{0},active{0},peak{0};
    std::mutex mutex; std::condition_variable cv;
    bool hold=false,entered=false;
    static bool decode(void* p,const uint8_t* frame,size_t n,uint8_t* out,size_t capacity,size_t* written,size_t* consumed) {
        auto& self=*static_cast<Decoder*>(p);
        const auto active=++self.active;
        auto old=self.peak.load(); while (active>old && !self.peak.compare_exchange_weak(old,active)) {}
        thread_local RealZstd decoder;
        const bool ok=RealZstd::decode(&decoder,frame,n,out,capacity,written,consumed);
        std::this_thread::sleep_for(std::chrono::milliseconds(self.delay.load()));
        if (ok && out[0]==2) {
            std::unique_lock<std::mutex> lock(self.mutex);
            if (self.hold) { self.entered=true; self.cv.notify_all(); self.cv.wait(lock,[&]{return !self.hold;}); }
        }
        --self.active;
        return ok;
    }
};
struct Opened { std::shared_ptr<nx::DiskSource> source; std::shared_ptr<nx::Session> session; nx::FileId file; };
// One decode worker by default: the queue/cancellation tests count jobs that
// a single worker leaves queued. split() covers the pool.
Opened open(const std::filesystem::path& path,Decoder& decoder,bool prefetch=false,uint64_t cache=64*1024*1024,unsigned ahead=1,bool timing=false,bool atomic=false,unsigned workers=1) {
    auto source=nx::DiskSource::open(path); check(bool(source),"disk open");
    nx::SessionOptions options; options.sequential_prefetch=prefetch; options.lookahead_blocks=ahead;
    options.decode_workers=workers;
    options.reader.zstd=&Decoder::decode; options.reader.zstd_context=&decoder;
    options.reader.max_cached_block_bytes=cache;
    options.reader.collect_read_timing=timing; options.reader.atomic_reads=atomic;
    auto session=nx::Session::open(*source.value,options); check(bool(session),"session open/mount");
    auto info=(*session.value)->archive().resolve_file(0,"data.bin"); check(bool(info),"resolve once");
    return {*source.value,*session.value,info.value->id};
}
void read(Opened& s,uint64_t at,size_t n,const std::shared_ptr<nx::ReadCursor>& cursor={}) {
    std::vector<uint8_t> out(n,0);
    check(bool(s.session->read(s.file,at,n,out.data(),out.size(),cursor)),"read failed");
    for (size_t i=0;i<n;++i) check(out[i]==(at+i)/(256*1024)+1,"wrong bytes");
}
template<class Predicate> void until(Predicate&& predicate) {
    const auto deadline=std::chrono::steady_clock::now()+5s;
    while (!predicate()) { check(std::chrono::steady_clock::now()<deadline,"timed out"); std::this_thread::sleep_for(1ms); }
}
void reuse(const std::filesystem::path& path,bool zstd) {
    Decoder decoder; auto s=open(path,decoder); const auto indexed=s.source->read_calls();
    read(s,100*1024,4*1024); read(s,120*1024,8*1024); read(s,180*1024,16*1024);
    for (uint32_t snapshot=0;snapshot<2;++snapshot) {
        auto file=s.session->archive().resolve_file(snapshot,"mirror.bin"); check(bool(file),"mirror resolve");
        std::array<uint8_t,32> bytes{};
        check(bool(s.session->read(file.value->id,100*1024,bytes.size(),bytes.data(),bytes.size())),"mirror read");
        check(bytes[0]==1,"mirror bytes");
    }
    const auto stats=s.session->stats().cache;
    if (zstd) {
        check(stats.decompressions==1 && stats.raw_reads==0,"shared physical cache missed");
        check(s.source->read_calls()==indexed+1,"indices reread or archive reopened");
        check(stats.hits==4 && stats.bytes==256*1024,"cache counters");
    } else {
        // RAW is never cached: each read fetches only its checked 16 KiB slices.
        check(stats.decompressions==0 && stats.raw_reads==5 && stats.hits==0 && stats.bytes==0,"RAW slice reads");
        check(s.source->read_calls()==indexed+5,"indices reread or archive reopened");
        check(stats.stored_bytes_read==4*16*1024+2*16*1024,"RAW slice sizes");
    }
    read(s,250*1024,32*1024); // bounded random range crossing two physical blocks
}
void diagnostic_timing(const std::filesystem::path& path) {
    Decoder decoder; auto s=open(path,decoder,false,64*1024*1024,1,true);
    read(s,0,4*1024*1024);
    const auto stats=s.session->stats().cache;
#if defined(NX_READER_DIAGNOSTICS)
    check(stats.io_ns>0 && stats.hash_ns>0 && stats.copy_ns>0,"diagnostic clocks absent");
#else
    check(stats.io_ns==0 && stats.hash_ns==0 && stats.copy_ns==0,"production diagnostic clocks active");
#endif
}
void concurrent(const std::filesystem::path& path) {
    Decoder decoder; decoder.delay=30; auto s=open(path,decoder);
    std::atomic<bool> go{false}; std::atomic<unsigned> errors{0}; std::vector<std::thread> threads;
    for (unsigned i=0;i<24;++i) threads.emplace_back([&] {
        while (!go.load()) std::this_thread::yield();
        try { read(s,120*1024,4096); } catch (...) { ++errors; }
    });
    go=true; for (auto& t:threads) t.join();
    check(!errors && s.session->stats().cache.decompressions==1,"concurrent duplicate decode");
    check(s.session->stats().cache.coalesced>0,"misses did not coalesce");
    auto different=open(path,decoder); threads.clear(); decoder.peak=0;
    for (unsigned i=0;i<8;++i) threads.emplace_back([&,i]{try {read(different,uint64_t(i)*256*1024,4096);}catch(...){++errors;}});
    for (auto& t:threads) t.join();
    check(!errors && decoder.peak>1,"independent blocks serialized globally");
}
void eviction(const std::filesystem::path& path) {
    Decoder decoder; auto s=open(path,decoder,false,512*1024);
    for (unsigned b:{0u,1u,2u,0u}) read(s,uint64_t(b)*256*1024,4096);
    auto stats=s.session->stats().cache;
    check(stats.decompressions==4 && stats.evictions==2 && stats.bytes==512*1024 && stats.peak_bytes<=512*1024,"LRU/budget");
}
void prefetch(const std::filesystem::path& path) {
    Decoder decoder; decoder.hold=true; auto s=open(path,decoder,true,1024*1024,2);
    auto cursor=std::make_shared<nx::ReadCursor>();
    read(s,0,4096,cursor); read(s,4096,4096,cursor); read(s,8192,4096,cursor);
    { std::unique_lock<std::mutex> lock(decoder.mutex); check(decoder.cv.wait_for(lock,5s,[&]{return decoder.entered;}),"prefetch not started"); }
    const auto generation=cursor->generation.load(); std::array<uint8_t,1> invalid{};
    check(!s.session->read(s.file,5*1024*1024,1,invalid.data(),1,cursor) && cursor->generation>generation,
          "failed seek retained obsolete prediction");
    read(s,10*256*1024,4096,cursor); // invalidates the queued block 2
    { std::lock_guard<std::mutex> lock(decoder.mutex); decoder.hold=false; decoder.cv.notify_all(); }
    until([&]{return s.session->stats().prefetch_cancelled>=2;});
    check(s.session->stats().cache.decompressions==3,"stale queued prefetch performed IO");
    s.session->stop();
    std::array<uint8_t,32> out; out.fill(0xee);
    check(!s.session->read(s.file,0,out.size(),out.data(),out.size()),"read after stop");
    check(out[0]==0xee,"stop modified output");
    Decoder next; auto useful=open(path,next,true); auto sequential=std::make_shared<nx::ReadCursor>();
    read(useful,0,4096,sequential); read(useful,4096,4096,sequential); read(useful,8192,4096,sequential);
    until([&]{return useful.session->stats().prefetch_completed==1;});
    const auto decoded=useful.session->stats().cache.decompressions;
    read(useful,256*1024,4096);
    check(useful.session->stats().cache.decompressions==decoded,"prefetch did not warm demand cache");
    read(useful,12288,4096,sequential);
    read(useful,16384,4096,sequential);
    check(useful.session->stats().prefetch_submitted==1,"repeated prediction for same next block");
    useful.session->stop();
    Decoder random; auto r=open(path,random,true); auto rc=std::make_shared<nx::ReadCursor>();
    for (unsigned b:{1u,8u,3u,12u,0u}) read(r,uint64_t(b)*256*1024,4096,rc);
    check(r.session->stats().prefetch_submitted==0,"random access prefetched");
    Decoder low_budget; auto tiny=open(path,low_budget,true,64*1024); auto tc=std::make_shared<nx::ReadCursor>();
    read(tiny,0,4096,tc); read(tiny,4096,4096,tc); read(tiny,8192,4096,tc);
    check(tiny.session->stats().prefetch_submitted==0,"uncacheable blocks prefetched");
}
// A multi-block Zstd read hands its far half to the worker; RAW never does.
void split(const std::filesystem::path& zstd,const std::filesystem::path& raw) {
    Decoder decoder; decoder.delay=20; auto s=open(zstd,decoder);
    read(s,100*1024,4*256*1024);  // blocks 0..4: the worker takes 4 and 3
    const auto stats=s.session->stats();
    check(stats.prefetch_submitted==2 && stats.cache.decompressions==5,"split read work");
    check(decoder.peak>1,"split read did not decode in parallel");
    read(s,100*1024,4*256*1024);
    check(s.session->stats().prefetch_submitted==2,"cached blocks resubmitted");
    // Four workers: the caller keeps block 0, the pool decodes 1..4 at once.
    Decoder pooled; pooled.delay=20; auto p=open(zstd,pooled,false,64*1024*1024,1,false,false,4);
    read(p,100*1024,4*256*1024);
    const auto pool_stats=p.session->stats();
    check(pool_stats.prefetch_submitted==4 && pool_stats.cache.decompressions==5,"pooled split read work");
    check(pooled.peak>2,"pooled split read did not use several workers");
    Decoder unused; auto r=open(raw,unused,true); auto rc=std::make_shared<nx::ReadCursor>();
    read(r,100*1024,4*256*1024);
    for (unsigned i=0;i<4;++i) read(r,uint64_t(i)*4096,4096,rc);
    check(r.session->stats().prefetch_submitted==0,"RAW blocks warmed by the worker");
}
void corrupt(const std::filesystem::path& path) {
    Decoder decoder; auto s=open(path,decoder,false,64*1024*1024,1,false,true);
    std::vector<uint8_t> out(256*1024+1,0xee);
    check(!s.session->read(s.file,0,out.size(),out.data(),out.size()),"corruption accepted");
    check(std::all_of(out.begin(),out.end(),[](auto b){return b==0xee;}),"failed range changed output");
    // Default (fast) reads keep the verified first block, never the bad one.
    Decoder fast_decoder; auto f=open(path,fast_decoder);
    check(!f.session->read(f.file,0,out.size(),out.data(),out.size()),"fast corruption accepted");
    for (size_t i=0;i<out.size();++i) check(out[i]==(i<256*1024 ? 1 : 0xee),"fast read exposed a bad block");
}
void limits(const std::filesystem::path& path) {
    check(nx::SessionOptions{}.sequential_prefetch && nx::SessionOptions{}.split_reads,"measured prefetch/split defaults");
    Decoder decoder; auto s=open(path,decoder);
    std::array<uint8_t,32> output; output.fill(0xee);
    check(!s.session->read(s.file,0,output.size(),output.data(),output.size()-1),"short output accepted");
    check(!s.session->read(s.file,4*1024*1024,1,output.data(),output.size()),"past EOF accepted");
    check(output[0]==0xee,"invalid read changed output");
    check(!s.session->read(s.file,0,512*1024,nullptr,512*1024),"null split-read output accepted");
    check(s.session->stats().prefetch_submitted==0,"invalid output scheduled background IO");
    nx::SessionOptions bounded;
    bounded.reader.zstd=&Decoder::decode; bounded.reader.zstd_context=&decoder;
    bounded.reader.max_read_bytes=4096;
    auto limited=nx::Session::open(s.source,bounded); check(bool(limited),"bounded session open");
    check(!(*limited.value)->read(s.file,0,512*1024,output.data(),512*1024),"oversized split read accepted");
    check((*limited.value)->stats().prefetch_submitted==0,"oversized read scheduled background IO");
    nx::SessionOptions options; options.reader.verify_read_blocks=false;
    check(!nx::Session::open(s.source,options),"unverified runtime accepted");
    options.reader.verify_read_blocks=true; options.parallel_reads=65;
    check(!nx::Session::open(s.source,options),"unbounded admission accepted");
}
void cancellation(const std::filesystem::path& path) {
    Decoder decoder; decoder.hold=true; auto s=open(path,decoder);
    std::array<uint8_t,32> output; output.fill(0xee);
    bool accepted=true;
    std::thread reading([&] { accepted=bool(s.session->read(s.file,256*1024,output.size(),output.data(),output.size())); });
    {
        std::unique_lock<std::mutex> lock(decoder.mutex);
        if (!decoder.cv.wait_for(lock,5s,[&]{return decoder.entered;})) {
            decoder.hold=false; decoder.cv.notify_all(); lock.unlock(); reading.join();
            check(false,"in-flight cancellation setup");
        }
    }
    s.session->stop();
    { std::lock_guard<std::mutex> lock(decoder.mutex); decoder.hold=false; decoder.cv.notify_all(); }
    reading.join();
    check(!accepted && output[0]==0xee && s.session->stats().cache.bytes==0,"in-flight cancellation exposed bytes");
}
class FailingSource final : public nx::RandomAccessSource {
public:
    std::shared_ptr<nx::DiskSource> source;
    std::atomic<bool> fail{false};
    std::atomic<int> successful_before_failure{-1};
    // Reads longer than this fail without using up the other triggers.
    std::atomic<size_t> fail_above{SIZE_MAX};
    uint64_t size() const noexcept override { return source->size(); }
    bool read_at(uint64_t at,uint8_t* output,size_t length) override {
        if (length>fail_above.load()) { if (length) output[0]=0x99; return false; }
        const auto remaining=successful_before_failure.load();
        if (fail.exchange(false) || (remaining>=0 && successful_before_failure.fetch_sub(1)==0)) {
            if (length) output[0]=0x99;
            return false;
        }
        return source->read_at(at,output,length);
    }
};
void io_failure(const std::filesystem::path& path) {
    Decoder decoder;
    auto disk=nx::DiskSource::open(path); check(bool(disk),"I/O disk open");
    auto source=std::make_shared<FailingSource>(); source->source=*disk.value;
    nx::SessionOptions options; options.sequential_prefetch=false; options.reader.atomic_reads=true;
    options.reader.zstd=&Decoder::decode; options.reader.zstd_context=&decoder;
    auto opened=nx::Session::open(source,options); check(bool(opened),"I/O session open");
    auto session=*opened.value; auto file=session->archive().resolve_file(0,"data.bin"); check(bool(file),"I/O resolve");
    source->fail=true;
    std::array<uint8_t,32> output; output.fill(0xee);
    auto result=session->read(file.value->id,0,output.size(),output.data(),output.size());
    check(!result && result.error.code==nx::ErrorCode::io && output[0]==0xee,"partial I/O changed caller bytes");
    check(session->stats().cache.bytes==0,"failed I/O cached");
    check(bool(session->read(file.value->id,0,output.size(),output.data(),output.size())) && output[0]==1,"I/O retry failed");

    // Fail after two good payloads in one request, then retry. In particular,
    // reused compressed scratch must not expose stale or partly-read bytes.
    options.reader.max_cached_block_bytes=0;
    opened=nx::Session::open(source,options); check(bool(opened),"uncached I/O session");
    session=*opened.value;
    file=session->archive().resolve_file(0,"data.bin"); check(bool(file),"uncached I/O resolve");
    std::vector<uint8_t> range(4*256*1024,0xee);
    source->fail_above=256*1024;  // a coalesced RAW run read fails; per-block retry
    source->successful_before_failure=2;
    result=session->read(file.value->id,0,range.size(),range.data(),range.size());
    check(!result && result.error.code==nx::ErrorCode::io,"late I/O failure accepted");
    check(std::all_of(range.begin(),range.end(),[](auto b){return b==0xee;}),"late I/O changed caller bytes");
    check(bool(session->read(file.value->id,0,range.size(),range.data(),range.size())),"late I/O retry failed");
    for (size_t i=0;i<range.size();++i) check(range[i]==i/(256*1024)+1,"stale scratch bytes");
    source->fail_above=SIZE_MAX;
    std::fill(range.begin(),range.end(),uint8_t{0xee});
    check(bool(session->read(file.value->id,0,range.size(),range.data(),range.size())),"coalesced retry failed");
    for (size_t i=0;i<range.size();++i) check(range[i]==i/(256*1024)+1,"coalesced bytes");
}
// lookup() must agree with enumerate() for every entry of every directory.
void lookup_agrees(const nx::Archive& archive,nx::DirectoryHandle dir,unsigned& checked) {
    std::vector<nx::DirectoryEntryView> entries;
    check(bool(archive.enumerate(dir,[&](const nx::DirectoryEntryView& e) { entries.push_back(e); return true; })),"enumerate");
    for (const auto& e:entries) {
        const auto hit=archive.lookup(dir,e.name);
        check(bool(hit) && hit.value->name==e.name && hit.value->type==e.type,"lookup type");
        if (e.type==nx::EntryType::directory) {
            check(hit.value->directory.directory_index==e.directory.directory_index,"lookup directory");
            lookup_agrees(archive,e.directory,checked);
        } else check(hit.value->file.value==e.file.value && hit.value->size==e.size,"lookup file");
        ++checked;
        // Prefixes and extensions of a real name are absent unless listed.
        const std::string longer=std::string(e.name)+std::string(1,char(1));
        check(!archive.lookup(dir,longer) && archive.lookup(dir,longer).error.code==nx::ErrorCode::not_found,"lookup extension");
    }
    check(!archive.lookup(dir,"") && !archive.lookup(dir,"~~ no such entry ~~"),"lookup missing");
}
void lookup(const std::filesystem::path& path) {
    Decoder decoder; auto s=open(path,decoder);
    const auto& archive=s.session->archive();
    unsigned checked=0;
    for (uint32_t snapshot=0;snapshot<archive.snapshots().size();++snapshot)
        lookup_agrees(archive,*archive.root_directory(snapshot).value,checked);
    check(checked>=2,"lookup coverage");
    check(archive.lookup({0,0xfffffff0u},"data.bin").error.code==nx::ErrorCode::bounds,"lookup bad handle");
    const auto root=*archive.root_directory(0).value;
    const auto data=archive.lookup(root,"data.bin");
    check(bool(data) && data.value->file.value==s.file.value,"lookup data.bin");
}
// ---- Partial (short-read) mode ----
constexpr size_t block_size=256*1024;
uint8_t expected(uint64_t at) { return static_cast<uint8_t>(at/block_size+1); }
// Checks the partial contract: verified prefix correct, the rest never written.
void check_prefix(const std::vector<uint8_t>& out,uint64_t at,size_t bytes,const char* message) {
    for (size_t i=0;i<bytes;++i) check(out[i]==expected(at+i),message);
    // Past the verified prefix: untouched, or zeroed by a direct RAW read.
    for (size_t i=bytes;i<out.size();++i) check(out[i]==0xee || out[i]==0,message);
}
// An emulator VFS read loop: bounded calls, returns the exact verified count.
size_t vfs_read(nx::Session& session,nx::FileId file,uint64_t at,uint8_t* output,size_t length) {
    size_t done=0;
    while (done<length) {
        const auto part=static_cast<size_t>(std::min<uint64_t>(length-done,session.max_read_bytes()));
        const auto result=session.read(file,at+done,part,output+done,part);
        done+=result.bytes;
        if (!result) break;
    }
    return done;
}
void partial_equivalence(const std::filesystem::path& path) {
    Decoder decoder; auto s=open(path,decoder);
    const auto size=s.session->archive().file_info(s.file).value->size;
    uint64_t seed=12345;
    const auto next=[&] { seed=seed*6364136223846793005ull+1442695040888963407ull; return seed>>33; };
    for (unsigned i=0;i<200;++i) {
        const uint64_t at=next()%size;
        const size_t n=static_cast<size_t>(std::min<uint64_t>(size-at,next()%(3*block_size)+1));
        std::vector<uint8_t> partial(n,0xee),atomic(n,0xee);
        const auto result=s.session->read(s.file,at,n,partial.data(),n);
        check(bool(result) && result.bytes==n && result.ok(),"partial read failed");
        check(bool(s.session->read(s.file,at,n,atomic.data(),n)),"atomic read failed");
        check(partial==atomic,"partial and atomic reads differ");
        check_prefix(partial,at,n,"partial read bytes");
    }
    // Zero length, invalid arguments and EOF: no bytes, output untouched.
    std::vector<uint8_t> out(32,0xee);
    auto empty=s.session->read(s.file,size,0,out.data(),out.size());
    check(bool(empty) && empty.bytes==0,"empty partial read");
    auto eof=s.session->read(s.file,size,1,out.data(),out.size());
    check(!eof && eof.bytes==0 && eof.error.code==nx::ErrorCode::bounds,"past-EOF partial read");
    auto undersized=s.session->read(s.file,0,out.size(),out.data(),out.size()-1);
    check(!undersized && undersized.bytes==0 && undersized.error.code==nx::ErrorCode::resource_limit,"short output accepted");
    auto huge=s.session->read(s.file,0,s.session->max_read_bytes()+1,out.data(),~size_t{0});
    check(!huge && huge.bytes==0,"read limit ignored");
    check(std::all_of(out.begin(),out.end(),[](auto b){return b==0xee;}),"invalid partial read changed output");
    // A VFS-style loop returns the whole file in max_read_bytes calls.
    std::vector<uint8_t> whole(static_cast<size_t>(size),0xee);
    check(vfs_read(*s.session,s.file,0,whole.data(),whole.size())==whole.size(),"vfs loop short");
    check_prefix(whole,0,whole.size(),"vfs loop bytes");
}
// corrupt.nx: block 1 is bad in its first slice. corrupt_mid.nx: in slice 5.
void partial_corrupt(const std::filesystem::path& first_slice,const std::filesystem::path& mid_slice) {
    Decoder decoder; auto s=open(first_slice,decoder);
    std::vector<uint8_t> out(2*block_size,0xee);
    auto result=s.session->read(s.file,100,out.size(),out.data(),out.size());
    check(!result && result.error.code==nx::ErrorCode::integrity && result.bytes==block_size-100,"block prefix count");
    check_prefix(out,100,result.bytes,"corrupt block prefix");
    // Reading from inside the bad block returns nothing.
    std::fill(out.begin(),out.end(),uint8_t{0xee});
    result=s.session->read(s.file,block_size+8,4096,out.data(),out.size());
    check(!result && result.bytes==0 && out[0]==0xee,"bad slice exposed");
    Decoder mid_decoder; auto m=open(mid_slice,mid_decoder);
    std::fill(out.begin(),out.end(),uint8_t{0xee});
    result=m.session->read(m.file,100,out.size(),out.data(),out.size());
    check(!result && result.error.code==nx::ErrorCode::integrity && result.bytes==block_size-100+5*16*1024,"slice prefix count");
    check_prefix(out,100,result.bytes,"corrupt slice prefix");
    // atomic_reads keeps its guarantees on the same data, single block included.
    Decoder atomic_decoder; auto a=open(mid_slice,atomic_decoder,false,64*1024*1024,1,false,true);
    std::fill(out.begin(),out.end(),uint8_t{0xee});
    result=a.session->read(a.file,100,out.size(),out.data(),out.size());
    check(!result && result.bytes==0,"atomic corruption accepted");
    check(!a.session->read(a.file,block_size,200*1024,out.data(),out.size()),"atomic single-block corruption accepted");
    check(std::all_of(out.begin(),out.end(),[](auto b){return b==0xee;}),"atomic failure changed output");
    // VFS loop: the short count is exact and the bytes after it are untouched.
    std::vector<uint8_t> whole(4*1024*1024,0xee);
    const auto count=vfs_read(*m.session,m.file,0,whole.data(),whole.size());
    check(count==block_size+5*16*1024,"vfs short count");
    check_prefix(whole,0,count,"vfs short bytes");
}
void partial_io_failure(const std::filesystem::path& path) {
    Decoder decoder;
    auto disk=nx::DiskSource::open(path); check(bool(disk),"partial I/O disk open");
    auto source=std::make_shared<FailingSource>(); source->source=*disk.value;
    nx::SessionOptions options; options.sequential_prefetch=false;
    options.reader.zstd=&Decoder::decode; options.reader.zstd_context=&decoder;
    options.reader.max_cached_block_bytes=0;  // no worker reads, deterministic call count
    auto opened=nx::Session::open(source,options); check(bool(opened),"partial I/O session");
    auto session=*opened.value; auto file=session->archive().resolve_file(0,"data.bin"); check(bool(file),"partial I/O resolve");
    std::vector<uint8_t> range(4*block_size,0xee);
    // RAW blocks stored back to back are fetched with one read; when it fails
    // the reader goes block by block, so the prefix is still exact.
    source->fail_above=block_size;
    source->successful_before_failure=2;
    auto result=session->read(file.value->id,0,range.size(),range.data(),range.size());
    check(!result && result.error.code==nx::ErrorCode::io && result.bytes==2*block_size,"late I/O prefix count");
    check_prefix(range,0,result.bytes,"late I/O prefix bytes");
    source->fail=true;
    std::fill(range.begin(),range.end(),uint8_t{0xee});
    result=session->read(file.value->id,10,range.size()-10,range.data(),range.size());
    check(!result && result.bytes==0 && range[0]==0xee && range[0x9999]==0xee,"first-block I/O failure exposed bytes");
    result=session->read(file.value->id,0,range.size(),range.data(),range.size());
    check(bool(result) && result.bytes==range.size(),"partial I/O block-by-block retry");
    source->fail_above=SIZE_MAX;
    std::fill(range.begin(),range.end(),uint8_t{0xee});
    result=session->read(file.value->id,0,range.size(),range.data(),range.size());
    check(bool(result) && result.bytes==range.size(),"partial I/O retry");
    check_prefix(range,0,range.size(),"partial retry bytes");
}
void partial_cancellation(const std::filesystem::path& path) {
    Decoder decoder; decoder.hold=true;  // the decoder holds on block 1 (value 2)
    auto source=nx::DiskSource::open(path); check(bool(source),"cancel disk open");
    nx::SessionOptions options; options.sequential_prefetch=false; options.split_reads=false;
    options.reader.zstd=&Decoder::decode; options.reader.zstd_context=&decoder;
    auto opened=nx::Session::open(*source.value,options); check(bool(opened),"cancel session");
    auto session=*opened.value; auto file=session->archive().resolve_file(0,"data.bin"); check(bool(file),"cancel resolve");
    std::vector<uint8_t> out(3*block_size,0xee);
    nx::PartialRead result;
    std::thread reading([&] { result=session->read(file.value->id,0,out.size(),out.data(),out.size()); });
    {
        std::unique_lock<std::mutex> lock(decoder.mutex);
        if (!decoder.cv.wait_for(lock,5s,[&]{return decoder.entered;})) {
            decoder.hold=false; decoder.cv.notify_all(); lock.unlock(); reading.join();
            check(false,"partial cancellation setup");
        }
    }
    session->stop();
    { std::lock_guard<std::mutex> lock(decoder.mutex); decoder.hold=false; decoder.cv.notify_all(); }
    reading.join();
    check(!result && result.error.code==nx::ErrorCode::cancelled && result.bytes==block_size,"cancel prefix count");
    check_prefix(out,0,result.bytes,"cancel prefix bytes");
    auto after=session->read(file.value->id,0,16,out.data(),out.size());
    check(!after && after.bytes==0,"read after stop");
}
// RAW multi-block reads must stop at the next block when cancelled.
class CancelAfterRead final : public nx::RandomAccessSource {
public:
    std::shared_ptr<nx::DiskSource> source;
    std::atomic<bool> armed{false},cancel{false};
    uint64_t size() const noexcept override { return source->size(); }
    bool read_at(uint64_t at,uint8_t* output,size_t length) override {
        const bool ok=source->read_at(at,output,length);
        if (armed) cancel=true;
        return ok;
    }
    static bool cancelled(void* self) { return static_cast<CancelAfterRead*>(self)->cancel.load(); }
};
void partial_raw_cancellation(const std::filesystem::path& path) {
    auto disk=nx::DiskSource::open(path); check(bool(disk),"raw cancel open");
    auto source=std::make_shared<CancelAfterRead>(); source->source=*disk.value;
    nx::SessionOptions options;
    options.reader.is_cancelled=&CancelAfterRead::cancelled; options.reader.cancellation_context=source.get();
    auto opened=nx::Session::open(source,options); check(bool(opened),"raw cancel session");
    auto session=*opened.value; auto file=session->archive().resolve_file(0,"data.bin"); check(bool(file),"raw cancel resolve");
    std::vector<uint8_t> out(3*block_size,0xee);
    source->armed=true;  // the first payload read requests cancellation
    auto result=session->read(file.value->id,0,out.size(),out.data(),out.size());
    check(!result && result.error.code==nx::ErrorCode::cancelled && result.bytes==block_size,"RAW cancellation between blocks");
    check_prefix(out,0,result.bytes,"RAW cancellation bytes");
}
void partial_concurrent(const std::filesystem::path& path) {
    Decoder decoder; auto s=open(path,decoder,true);
    std::atomic<unsigned> errors{0}; std::vector<std::thread> threads;
    for (unsigned t=0;t<8;++t) threads.emplace_back([&,t] {
        auto cursor=std::make_shared<nx::ReadCursor>();
        std::vector<uint8_t> out(600*1024);
        for (unsigned i=0;i<40;++i) {
            const uint64_t at=(uint64_t(t)*977*1024+uint64_t(i)*300*1024)%(3*1024*1024);
            std::fill(out.begin(),out.end(),uint8_t{0xee});
            const auto r=s.session->read(s.file,at,out.size(),out.data(),out.size(),cursor);
            if (!r || r.bytes!=out.size()) { ++errors; continue; }
            for (size_t k=0;k<out.size();k+=4093) if (out[k]!=expected(at+k)) { ++errors; break; }
        }
    });
    for (auto& t:threads) t.join();
    check(!errors,"concurrent partial reads");
}
// A cache of two blocks evicts on almost every miss while other threads copy
// from pinned blocks; recycled buffers must never change bytes being copied.
void eviction_stress(const std::filesystem::path& path) {
    Decoder decoder; auto s=open(path,decoder,false,2*256*1024);
    std::atomic<unsigned> errors{0}; std::vector<std::thread> threads;
    for (unsigned t=0;t<8;++t) threads.emplace_back([&,t] {
        std::vector<uint8_t> out(300*1024);
        uint32_t state=0x9e3779b9u*(t+1);
        for (unsigned i=0;i<300;++i) {
            state^=state<<13; state^=state>>17; state^=state<<5;
            const size_t n=1+state%out.size();
            const uint64_t at=state%(4*256*1024-n);
            const auto r=s.session->read(s.file,at,n,out.data(),out.size());
            if (!r || r.bytes!=n) { ++errors; continue; }
            for (size_t k=0;k<n;++k) if (out[k]!=expected(at+k)) { ++errors; break; }
        }
    });
    for (auto& t:threads) t.join();
    check(!errors,"eviction stress bytes");
    check(s.session->stats().cache.evictions>0,"eviction stress evicted nothing");
}
void disk_thread_lifetime(const std::filesystem::path& root) {
#if defined(_WIN32)
    DWORD handles_before=0,handles_after=0;
    check(bool(GetProcessHandleCount(GetCurrentProcess(),&handles_before)),"disk handle baseline");
#endif
    auto raw=nx::DiskSource::open(root/"raw.nx");
    auto zstd=nx::DiskSource::open(root/"zstd.nx");
    check(bool(raw) && bool(zstd),"disk lifetime open");
    std::array<uint8_t,8> expected{};
    check((*raw.value)->read_at(0,expected.data(),expected.size()),"disk lifetime header");
    check((*raw.value)->read_at((*raw.value)->size(),nullptr,0),"disk zero-byte EOF read");
    std::atomic<unsigned> errors{0};
    for (unsigned batch=0;batch<4;++batch) {
        std::vector<std::thread> workers;
        for (unsigned t=0;t<8;++t) workers.emplace_back([&] {
            std::array<uint8_t,8> bytes{};
            for (unsigned i=0;i<64;++i) {
                // Each pinned source owns a bounded pool of reusable events.
                auto source=i%2 ? *raw.value : *zstd.value;
                if (!source->read_at(0,bytes.data(),bytes.size()) || bytes!=expected) ++errors;
            }
        });
        for (auto& worker:workers) worker.join();
    }
    check(errors==0,"repeated positional reads across thread exits");
    raw.value.reset(); zstd.value.reset();
#if defined(_WIN32)
    check(bool(GetProcessHandleCount(GetCurrentProcess(),&handles_after)),"disk handle final");
    check(handles_after<=handles_before,"source-owned positional-read event leaked");
#endif
}
void benchmark(const std::filesystem::path& path,uint64_t cache) {
    Decoder decoder; auto s=open(path,decoder,false,cache);
    // Warm the OS cache through the source, without populating the NX cache.
    std::vector<uint8_t> warm(1024*1024);
    for (uint64_t at=0;at<s.source->size();) {
        const auto amount=static_cast<size_t>(std::min<uint64_t>(warm.size(),s.source->size()-at));
        check(s.source->read_at(at,warm.data(),amount),"benchmark warmup"); at+=amount;
    }
    std::vector<uint64_t> samples; samples.reserve(1000);
    const auto before=s.source->read_calls(); const auto started=std::chrono::steady_clock::now();
    for (unsigned i=0;i<1000;++i) {
        const auto at=std::chrono::steady_clock::now();
        read(s,((i*7)%16)*256ull*1024+100*1024,4096);
        samples.push_back(static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now()-at).count()));
    }
    const auto elapsed=std::chrono::duration<double>(std::chrono::steady_clock::now()-started).count();
    const auto stats=s.session->stats();
    std::sort(samples.begin(),samples.end());
    uint64_t rss=0;
#if defined(_WIN32)
    PROCESS_MEMORY_COUNTERS memory{}; memory.cb=sizeof(memory);
    if (GetProcessMemoryInfo(GetCurrentProcess(),&memory,sizeof(memory))) rss=memory.PeakWorkingSetSize;
#else
    struct rusage usage{};
    if (!getrusage(RUSAGE_SELF,&usage)) {
        rss=static_cast<uint64_t>(usage.ru_maxrss);
#if !defined(__APPLE__)
        rss*=1024;
#endif
    }
#endif
    std::cout<<"BENCH,"<<cache<<','<<elapsed<<','<<stats.cache.decompressions<<','<<stats.cache.hits<<','
        <<s.source->read_calls()-before<<','<<stats.cache.peak_bytes<<','<<stats.cache.misses<<','
        <<stats.cache.decompression_ns<<','<<samples[499]<<','<<samples[949]<<','<<samples[989]<<','<<rss<<','<<stats.cache.stored_bytes_read<<'\n';
}
}
int main(int argc,char** argv) {
    if (argc!=2) return 2;
    try {
        const auto root=std::filesystem::u8path(argv[1]);
        reuse(root/"raw.nx",false); reuse(root/"zstd.nx",true);
        diagnostic_timing(root/"zstd.nx");
        concurrent(root/"zstd.nx"); eviction(root/"zstd.nx"); prefetch(root/"zstd.nx"); split(root/"zstd.nx",root/"raw.nx"); corrupt(root/"corrupt.nx"); limits(root/"zstd.nx");
        cancellation(root/"zstd.nx"); io_failure(root/"zstd.nx"); io_failure(root/"raw.nx");
        lookup(root/"zstd.nx"); lookup(root/"raw.nx");
        partial_equivalence(root/"raw.nx"); partial_equivalence(root/"zstd.nx");
        partial_corrupt(root/"corrupt.nx",root/"corrupt_mid.nx");
        partial_io_failure(root/"zstd.nx"); partial_io_failure(root/"raw.nx");
        partial_cancellation(root/"zstd.nx"); partial_raw_cancellation(root/"raw.nx");
        partial_concurrent(root/"zstd.nx"); partial_concurrent(root/"raw.nx");
        eviction_stress(root/"zstd.nx");
        disk_thread_lifetime(root);
        benchmark(root/"zstd.nx",0); benchmark(root/"zstd.nx",64*1024*1024);
        std::cout<<"PASS persistent mounted indexes, shared LRU, coalesced/parallel reads, bounded prefetch/split/cancellation, RAW and atomic corruption rejection\n";
    } catch (const std::exception& error) { std::cerr<<error.what()<<'\n'; return 1; }
}
