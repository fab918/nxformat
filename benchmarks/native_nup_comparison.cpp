// SPDX-License-Identifier: GPL-3.0-or-later
// Benchmark adapter for Eglantine's unmodified GPL NupContainer and NX's reader.
// No emulator or game content is vendored. See run_native_nup_comparison.py.
#include "core/file_sys/nup_container.h"
#if defined(NX_BENCH_ORIGINAL)
#include "original_nx_reader.hpp"
#else
#include "../dist/nx_reader.hpp"
#endif
#include "../dist/nx_mapped_views.hpp"
#include <zstd.h>
#include <algorithm>
#include <chrono>
#include <cstdio>
#include <fstream>
#include <iostream>
#include <sstream>
#include <cstdlib>
#include <windows.h>
#include <psapi.h>
#include <share.h>
#include <io.h>
#pragma comment(lib,"psapi.lib")

class Input final : public FileSys::VfsFile, public nx::RandomAccessSource {
    mutable std::FILE* file;
    std::size_t length;
    std::unique_ptr<nx::detail::MappedViews> views;
public:
    explicit Input(const char* path) : file(_fsopen(path, "rb", _SH_DENYWR)), length(0) {
        if (!file) throw std::runtime_error("cannot open input");
        _fseeki64(file, 0, SEEK_END); length = static_cast<std::size_t>(_ftelli64(file));
        if (std::getenv("NX_BENCH_MAPPED")) {
            views=std::make_unique<nx::detail::MappedViews>();
            if (!views->open(reinterpret_cast<HANDLE>(_get_osfhandle(_fileno(file))),length)) {
                std::fclose(file);throw std::runtime_error("mapping failed");
            }
        }
    }
    ~Input() override { views.reset(); std::fclose(file); }
#if !defined(NX_BENCH_ORIGINAL)
    bool immutable_reads() const noexcept override { return true; }
    nx::ImmutableReadView immutable_view(std::uint64_t offset,std::size_t n) const noexcept override {
        if(!views)return {};
        auto view=views->pin(offset,n);return {view.data,std::move(view.owner),&nx::detail::MappedViews::guarded_access,&nx::detail::MappedViews::copy};
    }
#endif
    std::string GetName() const override { return "benchmark"; }
    std::size_t GetSize() const override { return length; }
    std::uint64_t size() const noexcept override { return length; }
    bool Resize(std::size_t) override { return false; }
    FileSys::VirtualDir GetContainingDirectory() const override { return {}; }
    bool IsReadable() const override { return true; }
    bool IsWritable() const override { return false; }
    bool Rename(std::string_view) override { return false; }
    std::size_t Write(const u8*, std::size_t, std::size_t) override { return 0; }
    std::size_t Read(u8* data, std::size_t n, std::size_t offset) const override {
        if (offset>length || n>length-offset) return 0;
        if (views) { auto view=views->pin(offset,n); if(view.data) { return nx::detail::MappedViews::copy(view.data,data,n) ? n : 0; } }
        if (_fseeki64(file, static_cast<long long>(offset), SEEK_SET)) return 0;
        return std::fread(data, 1, n, file);
    }
    bool read_at(std::uint64_t offset, std::uint8_t* data, std::size_t n) override {
        return Read(data, n, offset) == n;
    }
};
bool decode(void* context, const std::uint8_t* input, std::size_t n,
            std::uint8_t* output, std::size_t cap, std::size_t* written, std::size_t* consumed) {
    const auto frame = ZSTD_findFrameCompressedSize(input, n);
    if (ZSTD_isError(frame)) return false;
    const auto result = ZSTD_decompressDCtx(static_cast<ZSTD_DCtx*>(context), output, cap, input, frame);
    if (ZSTD_isError(result)) return false;
    *written = result; *consumed = frame; return true;
}
int main(int argc, char** argv) try {
    if (argc != 8) throw std::runtime_error("usage: bench NUP NX READS.tsv OUTPUT.csv CACHE_MIB PASSES SPILL_DIR");
    const auto budget = std::stoull(argv[5]) << 20;
    const auto open_started=std::chrono::steady_clock::now();
    const std::string mode=std::getenv("NX_BENCH_ONLY") ? std::getenv("NX_BENCH_ONLY") : "both";
    if (mode!="both" && mode!="nx" && mode!="nup") throw std::runtime_error("invalid reader mode");
    const bool do_nup=mode!="nx", do_nx=mode!="nup";
    std::shared_ptr<Input> input;
    std::unique_ptr<Input> nx_input;
    std::unique_ptr<FileSys::NupContainer> nup;
    FileSys::VirtualDir romfs;
    std::optional<nx::Archive> archive;
    if (do_nup) {
        input=std::make_shared<Input>(argv[1]);
        nup=std::make_unique<FileSys::NupContainer>(input,budget,argv[7]);
        if (nup->GetStatus()!=Loader::ResultStatus::Success) throw std::runtime_error("NUP open failed");
        auto base=nup->FindBasePackage();
        if (!base) throw std::runtime_error("no NUP base");
        romfs=nup->GetRomFSDir(*base);
    }
    auto* decoder=do_nx ? ZSTD_createDCtx() : nullptr;
    if (do_nx && !decoder) throw std::runtime_error("Zstd context allocation failed");
    nx::OpenOptions options;
#if !defined(NX_BENCH_ORIGINAL)
    options.cache_raw_verification = std::getenv("NX_BENCH_RAW_VERIFY") != nullptr;
#endif
    options.collect_read_stats = true;
    options.zstd = decode; options.zstd_context = decoder;
    options.max_cached_block_bytes = budget;
    options.max_cached_blocks = static_cast<std::size_t>(budget / 65536 + 1);
    if (do_nx) {
        nx_input=std::make_unique<Input>(argv[2]);
        auto opened=nx::Archive::open(*nx_input,options);
        if (!opened) throw std::runtime_error(opened.error.message);
        archive.emplace(std::move(*opened.value));
        auto mounted=archive->mount();
        if (!mounted) throw std::runtime_error(mounted.error.message);
    }
    const auto open_ns=std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now()-open_started).count();
    const bool resolved=std::getenv("NX_BENCH_RESOLVED")!=nullptr;
    struct Request { std::string path; std::size_t offset, size; FileSys::VirtualFile nup_file; nx::FileId nx_file{}; };
    std::vector<Request> requests;
    std::ifstream list(argv[3]); std::string line;
    while (std::getline(list, line)) {
        std::istringstream row(line); Request r;
        std::getline(row, r.path, '\t'); row >> r.offset >> r.size;
        if (!row || r.path.rfind("romfs/", 0) != 0 || !r.size) throw std::runtime_error("bad request");
        requests.push_back(r);
    }
    if (requests.empty()) throw std::runtime_error("empty requests");
    if(resolved) {
        std::unordered_map<std::string,FileSys::VirtualFile> nup_files;
        std::unordered_map<std::string,nx::FileId> nx_files;
        for(auto& request:requests) {
            if(do_nup) {
                auto& file=nup_files[request.path];
                if(!file) file=romfs->GetFileRelative(request.path.substr(6));
                if(!file) throw std::runtime_error("cannot resolve NUP file");
                request.nup_file=file;
            }
            if(do_nx) {
                auto found=nx_files.find(request.path);
                if(found==nx_files.end()) {
                    auto file=archive->resolve_file(0,request.path);
                    if(!file)throw std::runtime_error("cannot resolve NX file");
                    found=nx_files.emplace(request.path,file.value->id).first;
                }
                request.nx_file=found->second;
            }
        }
    }
    std::ofstream out(argv[4]);
    out << "pass,request,bytes,nup_ns,nx_ns\n";
    using Clock = std::chrono::steady_clock;
    std::vector<u8> a, b;
    std::uint64_t sample_checksum=0;
    for (unsigned pass = 0; pass < std::stoul(argv[6]); ++pass) {
        for (std::size_t i = 0; i < requests.size(); ++i) {
            const auto& r = requests[i];
            if (do_nup) a.resize(r.size);
            if (do_nx) b.resize(r.size);
            std::int64_t nup_ns = 0, nx_ns = 0;
            auto read_nup = [&] {
                const auto start = Clock::now();
                auto file = resolved ? r.nup_file : romfs->GetFileRelative(r.path.substr(6));
                if (!file || file->Read(a.data(), r.size, r.offset) != r.size) throw std::runtime_error("NUP short read");
                nup_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now()-start).count();
            };
            auto read_nx = [&] {
                const auto start = Clock::now();
                auto read = resolved ? archive->read_file(r.nx_file,r.offset,r.size,b.data(),b.size())
                                     : archive->read_file(0,r.path,r.offset,r.size,b.data(),b.size());
                if (!read.ok() || read.bytes != r.size) throw std::runtime_error("NX short read");
                nx_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now()-start).count();
            };
            if ((pass+i)%2) { if(do_nx) read_nx(); if(do_nup) read_nup(); }
            else { if(do_nup) read_nup(); if(do_nx) read_nx(); }
            if (do_nup && do_nx && a != b) throw std::runtime_error("read bytes differ");
            const auto& sampled=do_nx ? b : a;
            for (auto at:{std::size_t(0),sampled.size()/2,sampled.size()-1})
                sample_checksum=(sample_checksum*0x100000001b3ull)^sampled[at];
            out << pass << ',' << i << ',' << r.size << ',' << nup_ns << ',' << nx_ns << '\n';
        }
    }
    ZSTD_freeDCtx(decoder);
    const auto stats=do_nx ? archive->cache_stats() : nx::CacheStats{};
    std::uint64_t spill_bytes=0;
    if (std::filesystem::exists(argv[7]))
        for (const auto& file:std::filesystem::directory_iterator(argv[7]))
            if (file.is_regular_file()) spill_bytes+=file.file_size();
    PROCESS_MEMORY_COUNTERS_EX memory{};
    GetProcessMemoryInfo(GetCurrentProcess(),reinterpret_cast<PROCESS_MEMORY_COUNTERS*>(&memory),sizeof(memory));
    std::cout << "nx_cache_bytes=" << stats.bytes << " nx_cache_peak=" << stats.peak_bytes
#if !defined(NX_BENCH_ORIGINAL)
              << " verification_bytes=" << stats.raw_verification_bytes
#endif
              << " chunk_log2=" << (do_nx ? static_cast<unsigned>(archive->resolve_file(0,requests[0].path).value->chunk_log2) : 0)
              << " nx_cache_hits=" << stats.hits << " nx_raw_reads=" << stats.raw_reads
              << " open_mount_ns=" << open_ns << " spill_bytes=" << spill_bytes << " resolved=" << resolved << " reader_mode=" << mode << " peak_working_set=" << memory.PeakWorkingSetSize
              << " peak_private_commit=" << memory.PeakPagefileUsage << " sample_checksum=" << sample_checksum << '\n';
    std::cout << (mode=="both" ? "verified_reads=" : "sampled_reads=") << requests.size()*std::stoul(argv[6]) << " zstd=" << ZSTD_versionString() << '\n';
    return out.good() ? 0 : 1;
} catch (const std::exception& e) { std::cerr << e.what() << '\n'; return 1; }
