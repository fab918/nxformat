#include "nx_reader.hpp"
#include "real_zstd.hpp"

#include <array>
#include <algorithm>
#include <fstream>
#include <iostream>
#include <iterator>
#include <limits>
#include <mutex>
#include <string>
#include <thread>
#include <utility>
#include <atomic>
#include <cstdlib>
#include <new>
#include <vector>

// Allocation instrumentation deliberately implements new/delete with malloc/free.
// Keep the replacement functions out of line so GCC does not misdiagnose callers.
#if defined(__GNUC__) && !defined(__clang__)
#define NX_TEST_NOINLINE __attribute__((noinline))
#else
#define NX_TEST_NOINLINE
#endif
static std::atomic<bool> count_test_allocations{false};
static std::atomic<unsigned> test_allocations{0};
NX_TEST_NOINLINE void* operator new(std::size_t size) {
    if (!size) size = 1;
    void* pointer = std::malloc(size);
    if (!pointer) throw std::bad_alloc();
    if (count_test_allocations.load(std::memory_order_relaxed)) test_allocations.fetch_add(1, std::memory_order_relaxed);
    return pointer;
}
NX_TEST_NOINLINE void* operator new[](std::size_t size) { return ::operator new(size); }
NX_TEST_NOINLINE void operator delete(void* pointer) noexcept { std::free(pointer); }
NX_TEST_NOINLINE void operator delete[](void* pointer) noexcept { std::free(pointer); }
NX_TEST_NOINLINE void operator delete(void* pointer, std::size_t) noexcept { std::free(pointer); }
NX_TEST_NOINLINE void operator delete[](void* pointer, std::size_t) noexcept { std::free(pointer); }

class FileSource final : public nx::RandomAccessSource {
public:
    explicit FileSource(const std::string& path) : file_(path, std::ios::binary) {
        file_.seekg(0, std::ios::end);
        const auto end = file_.tellg();
        size_ = end < 0 ? 0 : static_cast<std::uint64_t>(end);
        file_.seekg(0, std::ios::beg);
    }
    bool good() const { return file_.is_open(); }
    std::uint64_t size() const noexcept override { return size_; }
    bool read_at(std::uint64_t offset, std::uint8_t* destination, std::size_t length) override {
        std::lock_guard<std::mutex> lock(mutex_);
        if (offset > size_ || length > size_ - offset || offset > static_cast<std::uint64_t>(std::numeric_limits<std::streamoff>::max())) return false;
        file_.clear();
        file_.seekg(static_cast<std::streamoff>(offset), std::ios::beg);
        if (!file_) return false;
        file_.read(reinterpret_cast<char*>(destination), static_cast<std::streamsize>(length));
        return file_.gcount() == static_cast<std::streamsize>(length);
    }
private:
    std::ifstream file_;
    std::uint64_t size_ = 0;
    std::mutex mutex_;
};

class MemorySource final : public nx::RandomAccessSource {
public:
    explicit MemorySource(const std::vector<std::uint8_t>& bytes) : bytes_(bytes) {}
    std::uint64_t size() const noexcept override { return bytes_.size(); }
    bool read_at(std::uint64_t offset,std::uint8_t* destination,std::size_t length) override {
        if (offset>bytes_.size() || length>bytes_.size()-offset) return false;
        if (length) std::copy(bytes_.begin()+static_cast<std::size_t>(offset),bytes_.begin()+static_cast<std::size_t>(offset+length),destination);
        return true;
    }
private:
    const std::vector<std::uint8_t>& bytes_;
};

class FailPayloadSource final : public nx::RandomAccessSource {
public:
    explicit FailPayloadSource(const std::vector<std::uint8_t>& bytes) : bytes_(bytes) {}
    std::uint64_t size() const noexcept override { return bytes_.size(); }
    // Fail every read that covers the offset (a coalesced run and its per-block retry).
    void fail_reads_covering(std::uint64_t offset) { fail_offset_ = offset; armed_ = true; }
    void disarm() { armed_ = false; }
    bool read_at(std::uint64_t offset, std::uint8_t* destination, std::size_t length) override {
        if (armed_ && offset <= fail_offset_ && fail_offset_ - offset < length) return false;
        if (offset > bytes_.size() || length > bytes_.size() - offset) return false;
        if (length) std::copy(bytes_.begin() + static_cast<std::size_t>(offset),
                              bytes_.begin() + static_cast<std::size_t>(offset + length), destination);
        return true;
    }
private:
    const std::vector<std::uint8_t>& bytes_;
    std::uint64_t fail_offset_ = std::numeric_limits<std::uint64_t>::max();
    bool armed_ = false;
};

class PartialFailSource final : public nx::RandomAccessSource {
public:
    explicit PartialFailSource(const std::vector<std::uint8_t>& bytes) : bytes_(bytes) {}
    std::uint64_t size() const noexcept override { return bytes_.size(); }
    void fail_partially_once_at(std::uint64_t offset, std::size_t written) {
        fail_offset_ = offset; partial_bytes_ = written; armed_ = true;
    }
    bool read_at(std::uint64_t offset, std::uint8_t* destination, std::size_t length) override {
        if (offset > bytes_.size() || length > bytes_.size() - offset) return false;
        if (armed_ && offset == fail_offset_) {
            armed_ = false;
            const auto amount = std::min(length, partial_bytes_);
            std::copy(bytes_.begin() + static_cast<std::size_t>(offset),
                      bytes_.begin() + static_cast<std::size_t>(offset + amount), destination);
            return false;
        }
        if (length) std::copy(bytes_.begin() + static_cast<std::size_t>(offset),
                              bytes_.begin() + static_cast<std::size_t>(offset + length), destination);
        return true;
    }
private:
    const std::vector<std::uint8_t>& bytes_;
    std::uint64_t fail_offset_ = std::numeric_limits<std::uint64_t>::max();
    std::size_t partial_bytes_ = 0;
    bool armed_ = false;
};

class CountingMemorySource final : public nx::RandomAccessSource {
public:
    explicit CountingMemorySource(const std::vector<std::uint8_t>& bytes)
        : bytes_(bytes), data_begin_(read_u64(bytes.data() + 56)), data_end_(data_begin_ + read_u64(bytes.data() + 64)) {}
    std::uint64_t size() const noexcept override { return bytes_.size(); }
    bool read_at(std::uint64_t offset, std::uint8_t* destination, std::size_t length) override {
        if (offset > bytes_.size() || length > bytes_.size() - offset) return false;
        if (length) std::copy(bytes_.begin() + static_cast<std::size_t>(offset),
                              bytes_.begin() + static_cast<std::size_t>(offset + length), destination);
        const auto end = offset + length;
        const auto begin = std::max(offset, data_begin_);
        const auto overlap_end = std::min(end, data_end_);
        if (overlap_end > begin) { data_bytes_ += overlap_end - begin; ++data_reads_; }
        return true;
    }
    void reset_data_counts() { data_bytes_ = 0; data_reads_ = 0; }
    std::uint64_t data_bytes() const { return data_bytes_; }
    unsigned data_reads() const { return data_reads_; }
private:
    static std::uint64_t read_u64(const std::uint8_t* p) {
        std::uint64_t value = 0;
        for (unsigned i = 0; i < 8; ++i) value |= std::uint64_t(p[i]) << (8 * i);
        return value;
    }
    const std::vector<std::uint8_t>& bytes_;
    std::uint64_t data_begin_ = 0, data_end_ = 0, data_bytes_ = 0;
    unsigned data_reads_ = 0;
};

struct TestShaContext {
    std::mutex mutex;
    bool fail = false;
    std::uint64_t calls = 0;
};

static bool synchronized_test_sha(void* opaque, const std::uint8_t* input, std::size_t length,
                                  std::uint8_t output[32]) {
    auto& context = *static_cast<TestShaContext*>(opaque);
    std::lock_guard<std::mutex> lock(context.mutex);
    if (context.fail) return false;
    const auto digest = nx::Sha256::digest(input, length);
    std::copy(digest.begin(), digest.end(), output);
    ++context.calls;
    return true;
}

static std::uint64_t read_u64_le(const std::uint8_t* p) {
    std::uint64_t value = 0;
    for (unsigned i = 0; i < 8; ++i) value |= std::uint64_t(p[i]) << (8 * i);
    return value;
}

static std::uint32_t read_u32_le(const std::uint8_t* p) {
    return std::uint32_t(p[0]) | (std::uint32_t(p[1]) << 8) |
           (std::uint32_t(p[2]) << 16) | (std::uint32_t(p[3]) << 24);
}

static std::uint32_t test_crc32c(const std::uint8_t* p, std::size_t n) {
    std::uint32_t crc=0xffffffffu;
    for (std::size_t i=0;i<n;++i) {
        crc^=p[i];
        for (int bit=0;bit<8;++bit) crc=(crc>>1)^((crc&1)?0x82f63b78u:0u);
    }
    return crc^0xffffffffu;
}

static bool load_archive_bytes(const std::string& path, std::vector<std::uint8_t>& bytes) {
    FileSource input(path);
    if (!input.good() || input.size() > 64ull * 1024 * 1024) return false;
    bytes.resize(static_cast<std::size_t>(input.size()));
    return input.read_at(0, bytes.data(), bytes.size());
}

static int raw_slice_tests(const std::string& path) {
    std::vector<std::uint8_t> bytes;
    if (!load_archive_bytes(path, bytes)) return 2;
    CountingMemorySource source(bytes);
    nx::OpenOptions options;
    options.verify_read_blocks = false;
    options.zstd = &RealZstd::decode;
    RealZstd decoder;
    if (!decoder.ready()) return 2;
    options.zstd_context = &decoder;
    auto opened = nx::Archive::open(source, options);
    if (!opened) { std::cerr << "FAIL mixed fixture open: " << opened.error.message << '\n'; return 1; }
    nx::Archive archive = std::move(*opened.value);
    if (!archive.mount()) { std::cerr << "FAIL mixed fixture mount\n"; return 1; }
    auto info = archive.resolve_file(0, "mixed.bin");
    if (!info || info.value->size != 512ull * 1024) return 1;
    std::array<std::uint8_t, 4096> small_read{};
    source.reset_data_counts();
    if (!archive.read_file(info.value->id, 0, small_read.size(), small_read.data(), small_read.size()) ||
        source.data_bytes() != small_read.size() || source.data_reads() != 1) {
        std::cerr << "FAIL unverified 4 KiB RAW slice read more than requested Data bytes\n"; return 1;
    }
    for (std::size_t i = 0; i < small_read.size(); ++i)
        if (small_read[i] != static_cast<std::uint8_t>((i * 31 + 7) & 0xff)) return 1;
    std::vector<std::uint8_t> full_raw(256 * 1024);
    source.reset_data_counts();
    if (!archive.read_file(info.value->id, 0, full_raw.size(), full_raw.data(), full_raw.size()) ||
        source.data_bytes() != full_raw.size()) {
        std::cerr << "FAIL full RAW block read did not fetch exactly one complete block\n"; return 1;
    }
    for (std::size_t i = 0; i < full_raw.size(); ++i)
        if (full_raw[i] != static_cast<std::uint8_t>((i * 31 + 7) & 0xff)) return 1;
    std::array<std::uint8_t, 8192> crossing{};
    source.reset_data_counts();
    const auto index_offset = read_u64_le(bytes.data() + 88);
    const auto second_stored = read_u32_le(bytes.data() + index_offset + 24 + 8);
    if (!archive.read_file(info.value->id, 256 * 1024 - 4, crossing.size(), crossing.data(), crossing.size()) ||
        source.data_reads() != 2 || source.data_bytes() != 4 + second_stored) {
        std::cerr << "FAIL mixed RAW/Zstandard read physical Data accounting: reads=" << source.data_reads()
                  << " bytes=" << source.data_bytes() << "\n"; return 1;
    }
    for (std::size_t i = 0; i < 4; ++i)
        if (crossing[i] != static_cast<std::uint8_t>(((256 * 1024 - 4 + i) * 31 + 7) & 0xff)) return 1;
    for (std::size_t i = 4; i < crossing.size(); ++i)
        if (crossing[i] != 'Z') return 1;

    std::vector<std::uint8_t> partial_bytes = bytes;
    PartialFailSource partial_source(partial_bytes);
    nx::OpenOptions partial_options = options;
    auto partial_open = nx::Archive::open(partial_source, partial_options);
    if (!partial_open) return 1;
    nx::Archive partial_archive = std::move(*partial_open.value);
    if (!partial_archive.mount()) return 1;
    std::array<std::uint8_t, 16> sentinel{};
    sentinel.fill(0xa5);
    const auto first_payload = read_u64_le(bytes.data() + index_offset);
    partial_source.fail_partially_once_at(first_payload, 3);
    auto failed = partial_archive.read_file(0, "mixed.bin", 0, sentinel.size(), sentinel.data(), sentinel.size());
    if (failed || failed.error.code != nx::ErrorCode::io ||
        !std::all_of(sentinel.begin(), sentinel.end(), [](std::uint8_t b) { return b == 0xa5; })) {
        std::cerr << "FAIL partial source write changed caller output after RAW I/O error\n"; return 1;
    }

    CountingMemorySource verified_source(bytes);
    auto verified_open = nx::Archive::open(verified_source);
    if (!verified_open) return 1;
    nx::Archive verified = std::move(*verified_open.value);
    if (!verified.mount()) return 1;
    verified_source.reset_data_counts();
    if (!verified.read_file(0, "mixed.bin", 0, small_read.size(), small_read.data(), small_read.size()) ||
        verified_source.data_bytes() != 16 * 1024) {
        std::cerr << "FAIL verified RAW read did not fetch exactly its 16 KiB checked slice\n"; return 1;
    }
    std::cout << "PASS RAW 4 KiB direct slice, full RAW block, mixed RAW/Zstandard crossing, partial-I/O output atomicity, and verified 16 KiB slice read\n";
    return 0;
}

static int sha_provider_tests(const std::string& raw_path, const std::string& corrupt_path) {
    const std::array<std::uint8_t, 3> abc = {'a','b','c'};
    const auto expected = nx::Sha256::digest(abc.data(), abc.size());
    const std::array<std::uint8_t, 32> independent_abc = {0xba,0x78,0x16,0xbf,0x8f,0x01,0xcf,0xea,
        0x41,0x41,0x40,0xde,0x5d,0xae,0x22,0x23,0xb0,0x03,0x61,0xa3,0x96,0x17,0x7a,0x9c,
        0xb4,0x10,0xff,0x61,0xf2,0x00,0x15,0xad};
    std::array<std::uint8_t, 32> actual{};
    TestShaContext context;
    if (!synchronized_test_sha(&context, abc.data(), abc.size(), actual.data()) ||
        actual != expected || actual != independent_abc) return 1;
    std::vector<std::uint8_t> raw_bytes;
    if (!load_archive_bytes(raw_path, raw_bytes)) return 2;
    MemorySource raw_source(raw_bytes);
    nx::OpenOptions options;
    options.block_sha256 = &synchronized_test_sha;
    options.block_sha256_context = &context;
    auto opened = nx::Archive::open(raw_source, options);
    if (!opened) return 1;
    nx::Archive archive = std::move(*opened.value);
    if (!archive.mount()) return 1;
    std::array<std::uint8_t, 4> data{};
    if (!archive.read_file(0, "data.bin", 0, data.size(), data.data(), data.size()) ||
        data != std::array<std::uint8_t, 4>{'N','X','!','?'}) return 1;
    std::atomic<bool> threads_ok{true};
    std::array<std::thread, 4> threads;
    for (auto& thread : threads) thread = std::thread([&archive, &threads_ok]() {
        for (unsigned i = 0; i < 50; ++i) {
            std::array<std::uint8_t, 4> result{};
            if (!archive.read_file(0, "data.bin", 0, result.size(), result.data(), result.size()) ||
                result != std::array<std::uint8_t, 4>{'N','X','!','?'}) threads_ok = false;
        }
    });
    for (auto& thread : threads) thread.join();
    // NX 0.2 verifies blocks with slice CRC-32C: the SHA-256 callback stays a
    // valid option but is not consulted on reads (one call above, for "abc").
    if (!threads_ok || context.calls != 1) return 1;

    std::vector<std::uint8_t> corrupt_bytes;
    if (!load_archive_bytes(corrupt_path, corrupt_bytes)) return 2;
    MemorySource corrupt_source(corrupt_bytes);
    TestShaContext corrupt_context;
    nx::OpenOptions corrupt_options;
    corrupt_options.block_sha256 = &synchronized_test_sha;
    corrupt_options.block_sha256_context = &corrupt_context;
    auto corrupt_open = nx::Archive::open(corrupt_source, corrupt_options);
    if (!corrupt_open) return 1;
    nx::Archive corrupt = std::move(*corrupt_open.value);
    if (!corrupt.mount()) return 1;
    std::array<std::uint8_t, 16> unchanged{};
    unchanged.fill(0xa5);
    const auto corrupt_read = corrupt.read_file(0, "data.bin", 0, 4, unchanged.data(), unchanged.size());
    // Failed bytes are untouched or zeroed by default reads, never unverified.
    if (corrupt_read || corrupt_read.error.code != nx::ErrorCode::integrity || corrupt_read.bytes != 0 ||
        !std::all_of(unchanged.begin(), unchanged.begin() + 4, [](std::uint8_t b) { return b == 0xa5 || b == 0; }) ||
        !std::all_of(unchanged.begin() + 4, unchanged.end(), [](std::uint8_t b) { return b == 0xa5; })) {
        std::cerr << "FAIL corrupted block exposed bytes or wrote past the request\n"; return 1;
    }
    std::cout << "PASS callback SHA-256 standard digest, unused for block reads, corrupted-block CRC rejection, and synchronized concurrent reads\n";
    return 0;
}

static int pre_mount_media_type_test(const std::string& path) {
    std::vector<std::uint8_t> bytes;
    if (!load_archive_bytes(path, bytes)) return 2;
    const auto media_offset = read_u64_le(bytes.data() + 40);
    if (!media_offset || media_offset + 34 > bytes.size()) return 2;
    bytes[static_cast<std::size_t>(media_offset + 32)] = 3;
    bytes[static_cast<std::size_t>(media_offset + 33)] = 0;
    MemorySource source(bytes);
    auto opened = nx::Archive::open(source);
    if (!opened) { std::cerr << "FAIL incompatible-media archive failed open rather than read_media\n"; return 1; }
    nx::Archive archive = std::move(*opened.value);
    if (archive.mounted()) return 1;
    const auto result = archive.read_media(0);
    if (result || result.error.code != nx::ErrorCode::format || archive.mounted()) {
        std::cerr << "FAIL incompatible Catalog/media type was not rejected by read_media before mount\n"; return 1;
    }
    std::cout << "PASS Catalog/media type mismatch rejected before mount\n";
    return 0;
}

static int metadata_history_association_test(const std::string& path) {
    std::vector<std::uint8_t> bytes;
    if (!load_archive_bytes(path,bytes)) return 2;
    const auto catalog_offset=read_u64_le(bytes.data()+24);
    const auto catalog_size=read_u64_le(bytes.data()+32);
    if (catalog_offset>bytes.size() || catalog_size>bytes.size()-catalog_offset || catalog_size<128) return 2;
    const auto* catalog=bytes.data()+catalog_offset;
    const auto extension_count=read_u32_le(catalog+40);
    const auto extension_table=read_u64_le(catalog+88);
    const auto application_table=read_u64_le(catalog+48);
    if (!extension_count || extension_table>catalog_size || extension_count>(catalog_size-extension_table)/32 || application_table>catalog_size || catalog_size-application_table<80) return 2;
    std::uint64_t history_payload=0;
    for (std::uint32_t i=0;i<extension_count;++i) {
        const auto row=extension_table+std::uint64_t(i)*32;
        const auto* descriptor=catalog+row;
        if (read_u32_le(descriptor)==0x00010004 && descriptor[8]==1 && descriptor[9]==0) {
            history_payload=read_u64_le(descriptor+16);
            break;
        }
    }
    if (!history_payload || history_payload>catalog_size || catalog_size-history_payload<72) return 2;
    const auto selected_snapshot=read_u32_le(catalog+application_table+68);
    for (unsigned i=0;i<4;++i) bytes[static_cast<std::size_t>(catalog_offset+history_payload+24+i)]=static_cast<std::uint8_t>(selected_snapshot>>(8*i));
    const auto digest=nx::Sha256::digest(bytes.data()+catalog_offset,static_cast<std::size_t>(catalog_size));
    std::copy(digest.begin(),digest.end(),bytes.begin()+120);
    const auto crc=test_crc32c(bytes.data(),252);
    for (unsigned i=0;i<4;++i) bytes[252+i]=static_cast<std::uint8_t>(crc>>(8*i));
    MemorySource source(bytes);
    const auto opened=nx::Archive::open(source);
    if (opened || opened.error.code!=nx::ErrorCode::format) {
        std::cerr << "FAIL history metadata referencing the selected snapshot was not rejected at open\n";
        return 1;
    }
    std::cout << "PASS snapshot metadata history cannot point at a different selected source\n";
    return 0;
}

static int captured_manifest_digest_test(const std::string& path) {
    std::vector<std::uint8_t> bytes;
    if (!load_archive_bytes(path,bytes) || bytes.size()<256) return 2;
    MemorySource source(bytes);
    auto opened=nx::Archive::open(source);
    if (!opened) return 1;
    nx::Archive archive=std::move(*opened.value);
    bytes[152]^=0x80; // Change the source header only after open() captured the validated digest.
    const auto mounted=archive.mount();
    if (!mounted || !archive.mounted()) {
        std::cerr << "FAIL mount reread an unchecked Manifest digest from the source header\n";
        return 1;
    }
    std::cout << "PASS mount uses the Manifest digest captured from the validated header at open\n";
    return 0;
}

static bool thread_local_zstd_decode(void*, const std::uint8_t* frame, std::size_t frame_size,
                                    std::uint8_t* output, std::size_t output_capacity,
                                    std::size_t* written, std::size_t* consumed) {
    thread_local RealZstd decoder;
    return decoder.ready() && RealZstd::decode(&decoder, frame, frame_size, output, output_capacity, written, consumed);
}

static bool check(bool condition, const char* message) {
    if (!condition) std::cerr << "FAIL " << message << '\n';
    return condition;
}

// A virtual 4 GiB+ archive exercises physical and logical offsets without
// writing gigabytes to disk or allocating a game-sized buffer.
class LargeOffsetSource final : public nx::RandomAccessSource {
public:
    explicit LargeOffsetSource(const std::vector<std::uint8_t>& seed) {
        auto put32=[](std::vector<std::uint8_t>& b,std::size_t at,std::uint32_t v){for(unsigned i=0;i<4;++i)b[at+i]=std::uint8_t(v>>(8*i));};
        auto put64=[](std::vector<std::uint8_t>& b,std::size_t at,std::uint64_t v){for(unsigned i=0;i<8;++i)b[at+i]=std::uint8_t(v>>(8*i));};
        auto align=[](std::uint64_t n,std::uint64_t a){return (n+a-1)&~(a-1);};
        // NX 0.2: header slot A in prefix_, slot B left zero (invalid), Data
        // at 8192, then the packed tail Catalog/Manifest/index/integrity.
        const auto catalog_offset=read_u64_le(seed.data()+24), catalog_size=read_u64_le(seed.data()+32);
        prefix_.assign(8192,0);
        std::copy(seed.begin(),seed.begin()+256,prefix_.begin());
        std::vector<std::uint8_t> catalog(seed.begin()+static_cast<std::ptrdiff_t>(catalog_offset),seed.begin()+static_cast<std::ptrdiff_t>(catalog_offset+catalog_size));
        const auto app=read_u64_le(catalog.data()+48);
        const auto program=read_u64_le(catalog.data()+56);
        const auto snapshot=read_u64_le(catalog.data()+64);
        const std::uint64_t logical=(1ull<<32)+1, count=4097, slices_per_block=64;
        const std::array<std::uint8_t,32> file_hash{{0x08,0x96,0xd3,0x3c,0x1e,0xf7,0x20,0xc5,0x23,0x38,0x59,0xb8,0xb5,0x9a,0xca,0xd5,0x7f,0xa9,0xfe,0xf0,0xcc,0x0c,0x5b,0x3f,0x90,0x10,0x55,0x9b,0x8c,0x2f,0xd1,0x07}};
        std::vector<std::uint8_t> manifest(static_cast<std::size_t>(align(200+count*4,8)+8),0);
        std::memcpy(manifest.data(),"NXMF",4);manifest[4]=1;manifest[6]=96;
        put64(manifest,8,manifest.size()); put32(manifest,16,1);put32(manifest,20,1);put32(manifest,24,1);put32(manifest,28,std::uint32_t(count));
        put64(manifest,32,96);put64(manifest,40,112);put64(manifest,48,176);put64(manifest,56,200);
        const auto path=align(200+count*4,8);put64(manifest,64,path);put64(manifest,72,8);
        put32(manifest,100,1);put64(manifest,112,logical);put32(manifest,124,std::uint32_t(count));manifest[128]=20;
        std::copy(file_hash.begin(),file_hash.end(),manifest.begin()+132);
        put64(manifest,176,path);put32(manifest,184,8);manifest[192]=2;
        for(std::uint32_t i=0;i<count;++i)put32(manifest,200+std::size_t(i)*4,i);
        std::memcpy(manifest.data()+path,"data.bin",8);
        // Identity v3 depends only on logical paths, file digest and scope.
        std::vector<std::uint8_t> identity;
        const std::string tag("NX-SNAPSHOT-ID\0",15); identity.insert(identity.end(),tag.begin(),tag.end());
        auto append=[&](std::uint64_t v,unsigned n){for(unsigned i=0;i<n;++i)identity.push_back(std::uint8_t(v>>(8*i)));};
        append(3,2);append(catalog[app+8],1);append(read_u64_le(catalog.data()+app),8);append(read_u64_le(catalog.data()+program),8);
        append(0,1);append(0,4);identity.insert(identity.end(),16,0);append(2,4);
        append(1,1);append(0,4);append(2,1);append(8,4);const std::string name="data.bin";
        identity.insert(identity.end(),name.begin(),name.end());append(logical,8);identity.insert(identity.end(),file_hash.begin(),file_hash.end());
        const auto id=nx::Sha256::digest(identity.data(),identity.size());
        std::copy(id.begin(),id.end(),catalog.begin()+static_cast<std::size_t>(snapshot+56));put64(catalog,static_cast<std::size_t>(snapshot+16),logical);
        data_offset_=8192;last_data_=data_offset_+(1ull<<32);tail_offset_=align(last_data_+1,16);
        const auto manifest_at=align(tail_offset_+catalog.size(),16);
        const auto index_at=align(manifest_at+manifest.size(),16),integrity_at=align(index_at+count*24,16);
        const auto checks=(count-1)*slices_per_block+1;
        total_size_=integrity_at+checks*4;tail_.resize(static_cast<std::size_t>(total_size_-tail_offset_),0);
        std::copy(catalog.begin(),catalog.end(),tail_.begin());
        std::copy(manifest.begin(),manifest.end(),tail_.begin()+static_cast<std::ptrdiff_t>(manifest_at-tail_offset_));
        std::vector<std::uint8_t> zeros(16*1024,0);const auto zero_crc=test_crc32c(zeros.data(),zeros.size());
        const std::uint8_t last=127;const auto last_crc=test_crc32c(&last,1);
        for(std::uint32_t i=0;i<count;++i){
            const auto at=static_cast<std::size_t>(index_at-tail_offset_+std::uint64_t(i)*24);
            put64(tail_,at,data_offset_+std::uint64_t(i)*1024*1024);put32(tail_,at+8,i==4096?1:1024*1024);put32(tail_,at+12,i==4096?1:1024*1024);
            put32(tail_,at+20,std::uint32_t(i*slices_per_block));
        }
        for(std::uint64_t k=0;k<checks;++k)
            put32(tail_,static_cast<std::size_t>(integrity_at-tail_offset_+k*4),k+1==checks?last_crc:zero_crc);
        put64(prefix_,16,total_size_);put64(prefix_,24,tail_offset_);put64(prefix_,40,0);put64(prefix_,48,0);
        put64(prefix_,56,data_offset_);put64(prefix_,64,logical);put64(prefix_,72,manifest_at);put64(prefix_,80,manifest.size());
        put64(prefix_,88,index_at);put64(prefix_,96,count);put64(prefix_,104,integrity_at);put64(prefix_,112,checks*4);
        const auto ch=nx::Sha256::digest(catalog.data(),catalog.size());std::copy(ch.begin(),ch.end(),prefix_.begin()+120);
        const auto mh=nx::Sha256::digest(manifest.data(),manifest.size());std::copy(mh.begin(),mh.end(),prefix_.begin()+152);
        nx::Sha256 ih;ih.update(tail_.data()+index_at-tail_offset_,static_cast<std::size_t>(count*24));ih.update(tail_.data()+integrity_at-tail_offset_,static_cast<std::size_t>(checks*4));
        const auto digest=ih.finish();std::copy(digest.begin(),digest.end(),prefix_.begin()+184);put32(prefix_,252,test_crc32c(prefix_.data(),252));
    }
    std::uint64_t size()const noexcept override{return total_size_;}
    bool read_at(std::uint64_t at,std::uint8_t* out,std::size_t n)override{
        if(at>total_size_ || n>total_size_-at)return false;
        std::fill(out,out+n,std::uint8_t(0));
        auto overlay=[&](std::uint64_t start,const std::vector<std::uint8_t>& b){const auto lo=std::max(at,start),hi=std::min(at+n,start+b.size());if(lo<hi)std::memcpy(out+lo-at,b.data()+lo-start,static_cast<std::size_t>(hi-lo));};
        overlay(0,prefix_);overlay(tail_offset_,tail_);
        if(at<=last_data_ && last_data_-at<n)out[last_data_-at]=127;
        return true;
    }
private:
    std::vector<std::uint8_t> prefix_,tail_;
    std::uint64_t data_offset_=0,last_data_=0,tail_offset_=0,total_size_=0;
};

static int review_regressions(const std::string& directory) {
    std::vector<std::uint8_t> raw;
    if (!load_archive_bytes(directory+"/raw_only.nx",raw)) return 2;
    LargeOffsetSource large(raw);
    auto big=nx::Archive::open(large);
    if (!big || !big.value->mount()) { std::cerr<<"FAIL virtual 4 GiB archive mount\n"; return 1; }
    std::array<std::uint8_t,2> boundary{};
    if (!check(big.value->read_file(nx::FileId{0},(1ull<<32)-1,2,boundary.data(),boundary.size()).ok() && boundary[0]==0 && boundary[1]==127,
               "physical/logical offsets above 4 GiB were truncated")) return 1;
    MemorySource source(raw);
    auto opened=nx::Archive::open(source);
    if (!opened || !opened.value->mount()) return 1;
    const auto data=read_u64_le(raw.data()+56), digests=read_u64_le(raw.data()+104);
    raw[static_cast<std::size_t>(data)]^=1;
    const auto changed_crc=test_crc32c(raw.data()+data,4);
    for(unsigned i=0;i<4;++i)raw[static_cast<std::size_t>(digests)+i]=std::uint8_t(changed_crc>>(8*i));
    std::uint8_t out=0xa5;
    auto read=opened.value->read_file(nx::FileId{0},0,1,&out,1);
    if (!check(!read && read.error.code==nx::ErrorCode::integrity && out==0xa5,
               "post-mount replacement of payload plus digest bypassed authenticated index")) return 1;

    std::vector<std::uint8_t> bytes;
    if (!load_archive_bytes(directory+"/multi_block_file.nx",bytes)) return 2;
    CountingMemorySource counted(bytes);
    nx::OpenOptions cached;
    cached.max_cached_block_bytes=65536; cached.max_cached_blocks=1;
    auto cache_open=nx::Archive::open(counted,cached);
    if (!cache_open || !cache_open.value->mount()) return 1;
    auto& archive=*cache_open.value;
    counted.reset_data_counts();
    // RAW reads touch only the verified 16 KiB slice; RAW is never cached.
    if (!check(archive.read_file(nx::FileId{0},0,1,&out,1) && counted.data_bytes()==16384,"RAW read is not slice sized")) return 1;
    if (!check(archive.read_file(nx::FileId{0},1,1,&out,1) && counted.data_bytes()==32768,"RAW slice reread")) return 1;
    if (!check(archive.read_file(nx::FileId{0},65536,1,&out,1) && counted.data_bytes()==32769,"short RAW tail block")) return 1;
    if (!check(archive.read_file(nx::FileId{0},0,1,&out,1) && counted.data_bytes()==49153 && archive.cache_stats().entries==0,"RAW block entered the decoded cache")) return 1;
    // verify_all must read physical payloads even when verified blocks are cached.
    bytes[static_cast<std::size_t>(read_u64_le(bytes.data()+56))]^=1;
    if (!check(!archive.verify_all(),"full verification trusted a stale decoded cache")) return 1;

    bytes[static_cast<std::size_t>(read_u64_le(bytes.data()+56))]^=1;
    MemorySource threaded_source(bytes);
    cached.max_cached_block_bytes=65537; cached.max_cached_blocks=2;
    auto threaded=nx::Archive::open(threaded_source,cached);
    if (!threaded || !threaded.value->mount()) return 1;
    std::atomic<bool> start{false}, threads_ok{true}; std::vector<std::thread> workers;
    for (unsigned i=0;i<4;++i) workers.emplace_back([&] {
        while(!start.load()) std::this_thread::yield();
        for(unsigned j=0;j<40;++j) {
            std::uint8_t byte=0;
            if(!threaded.value->read_file(nx::FileId{0},j%2?65536:0,1,&byte,1) || byte!=7) threads_ok=false;
        }
    });
    start=true; for(auto& worker:workers)worker.join();
    if(!check(threads_ok,"concurrent verified cache miss/hit failed"))return 1;
    std::vector<std::uint8_t> mixed;
    if(!load_archive_bytes(directory+"/mixed_raw_zstd.nx",mixed))return 2;
    CountingMemorySource compressed_source(mixed); RealZstd decoder;
    if(!decoder.ready())return 2;
    nx::OpenOptions compressed_options; compressed_options.zstd=&RealZstd::decode;
    compressed_options.zstd_context=&decoder; compressed_options.max_cached_block_bytes=262144;
    auto compressed=nx::Archive::open(compressed_source,compressed_options);
    if(!compressed || !compressed.value->mount())return 1;
    if(!check(compressed.value->read_file(nx::FileId{0},262144,1,&out,1)&&out=='Z'&&compressed.value->cache_stats().decompressions==1,"Zstandard miss"))return 1;
    compressed_source.reset_data_counts();
    if(!check(compressed.value->read_file(nx::FileId{0},262145,1,&out,1)&&out=='Z'&&compressed.value->cache_stats().decompressions==1&&compressed_source.data_bytes()==0,"Zstandard cache hit"))return 1;
    std::vector<std::uint8_t> shared;
    if (!load_archive_bytes(directory+"/shared_snapshots.nx",shared)) return 2;
    MemorySource shared_source(shared);
    nx::OpenOptions limited; limited.max_total_snapshot_nodes=3;
    auto many=nx::Archive::open(shared_source,limited);
    if (!many) return 1;
    auto mounted=many.value->mount();
    if (!check(!mounted && mounted.error.code==nx::ErrorCode::resource_limit,"cumulative snapshot node budget was ignored")) return 1;
    limited=nx::OpenOptions{}; limited.max_total_snapshot_path_bytes=0;
    many=nx::Archive::open(shared_source,limited);
    if (!many || many.value->mount()) return 1;
    limited=nx::OpenOptions{}; limited.max_index_bytes=27;
    many=nx::Archive::open(shared_source,limited);
    if (!many || many.value->mount()) return 1;
    bool cancel=true; limited=nx::OpenOptions{};
    limited.is_cancelled=[](void* state){return *static_cast<bool*>(state);};
    limited.cancellation_context=&cancel;
    many=nx::Archive::open(shared_source,limited);
    if (!many) return 1;
    mounted=many.value->mount();
    if (!check(!mounted && mounted.error.code==nx::ErrorCode::cancelled && !many.value->mounted(),"cancelled mount retained partial state")) return 1;
    cancel=false;
    if (!many.value->mount()) return 1;
    std::cout<<"PASS 4 GiB offsets, authenticated index mutation, RAW/Zstandard cache hits/eviction/concurrency, uncached full verification, cumulative budgets and mount cancellation/retry\n";
    return 0;
}

static int fuzz_mutations(const std::string& path) {
    FileSource file(path);
    if (!file.good() || file.size()>64ull*1024*1024) return 2;
    std::vector<std::uint8_t> original(static_cast<std::size_t>(file.size()));
    if (!file.read_at(0,original.data(),original.size())) return 2;
    std::uint64_t state=0x4e5841525a535444ull;
    auto random=[&]() { state^=state<<13; state^=state>>7; state^=state<<17; return state; };
    for (unsigned iteration=0;iteration<1000;++iteration) {
        auto mutated=original;
        const unsigned changes=1+static_cast<unsigned>(random()%4);
        for (unsigned j=0;j<changes;++j) {
            const auto at=static_cast<std::size_t>(random()%mutated.size());
            mutated[at]^=static_cast<std::uint8_t>(1u<<(random()%8));
        }
        MemorySource source(mutated);
        nx::OpenOptions options;
        auto opened=nx::Archive::open(source,options);
        if (opened) { nx::Archive archive=std::move(*opened.value); (void)archive.mount(); }
    }
    std::cout << "PASS 1000 deterministic parser mutations " << path << '\n';
    return 0;
}

class FaultOnceSource final : public nx::RandomAccessSource {
public:
    explicit FaultOnceSource(std::vector<std::uint8_t> bytes)
        : bytes_(std::move(bytes)), index_offset_(read_u64(bytes_.data() + 72)) {}
    std::uint64_t size() const noexcept override { return bytes_.size(); }
    bool read_at(std::uint64_t offset, std::uint8_t* destination, std::size_t length) override {
        if (armed_ && offset == index_offset_ && ++index_reads_ == 1) {
            armed_ = false;
            return false;
        }
        if (offset > bytes_.size() || length > bytes_.size() - offset) return false;
        std::copy(bytes_.begin() + static_cast<std::size_t>(offset), bytes_.begin() + static_cast<std::size_t>(offset + length), destination);
        return true;
    }
private:
    static std::uint64_t read_u64(const std::uint8_t* p) {
        std::uint64_t value = 0;
        for (unsigned i = 0; i < 8; ++i) value |= std::uint64_t(p[i]) << (8 * i);
        return value;
    }
    std::vector<std::uint8_t> bytes_;
    std::uint64_t index_offset_ = 0;
    unsigned index_reads_ = 0;
    bool armed_ = true;
};

static int mount_retry(const std::string& path) {
    FileSource input(path);
    if (!input.good() || input.size() > 64ull * 1024 * 1024) return 2;
    std::vector<std::uint8_t> bytes(static_cast<std::size_t>(input.size()));
    if (!input.read_at(0, bytes.data(), bytes.size())) return 2;
    FaultOnceSource source(std::move(bytes));
    auto opened = nx::Archive::open(source);
    if (!opened) return 1;
    nx::Archive archive = std::move(*opened.value);
    const auto first = archive.mount();
    if (first || first.error.code != nx::ErrorCode::io || archive.mounted()) {
        std::cerr << "FAIL injected parse-time I/O error did not leave a clean unmounted archive\n";
        return 1;
    }
    if (!archive.mount() || !archive.mounted()) {
        std::cerr << "FAIL second mount did not recover after source access returned\n";
        return 1;
    }
    std::array<std::uint8_t, 4> data{};
    if (!archive.read_file(0, "data.bin", 0, data.size(), data.data(), data.size()) ||
        data != std::array<std::uint8_t, 4>{'N', 'X', '!', '?'}) {
        std::cerr << "FAIL retry-mounted archive cannot read its file\n";
        return 1;
    }
    std::cout << "PASS mount retry after Manifest parsing I/O failure\n";
    return 0;
}

static int atomic_multiblock_read(const std::string& path) {
    FileSource file(path);
    if (!file.good() || file.size() > 64ull * 1024 * 1024) return 2;
    std::vector<std::uint8_t> bytes(static_cast<std::size_t>(file.size()));
    if (!file.read_at(0, bytes.data(), bytes.size()) || bytes.size() < 256) return 2;
    auto read_u64 = [](const std::uint8_t* p) {
        std::uint64_t value = 0;
        for (unsigned i = 0; i < 8; ++i) value |= std::uint64_t(p[i]) << (8 * i);
        return value;
    };
    const std::uint64_t index_offset = read_u64(bytes.data() + 88);
    if (index_offset > bytes.size() || bytes.size() - index_offset < 48) return 2;
    const std::uint64_t second_block_payload = read_u64(bytes.data() + index_offset + 24);
    FailPayloadSource source(bytes);
    nx::OpenOptions atomic_options;
    atomic_options.atomic_reads = true;
    auto opened = nx::Archive::open(source, atomic_options);
    if (!opened) { std::cerr << "FAIL atomic-read fixture open: " << opened.error.message << '\n'; return 1; }
    nx::Archive archive = std::move(*opened.value);
    if (!archive.mount()) { std::cerr << "FAIL atomic-read fixture mount\n"; return 1; }
    auto resolved = archive.resolve_file(0, "large.bin");
    if (!resolved || resolved.value->size < 65537) {
        std::cerr << "FAIL atomic-read target resolution: " << (resolved ? "unexpected file size " + std::to_string(resolved.value->size) : resolved.error.message) << '\n';
        return 1;
    }
    std::array<std::uint8_t, 3> output = {0xa5, 0x5a, 0xc3};
    source.fail_reads_covering(second_block_payload);
    const auto failed = archive.read_file(resolved.value->id, 65534, output.size(), output.data(), output.size());
    source.disarm();
    if (failed || failed.error.code != nx::ErrorCode::io || failed.bytes != 0 ||
        output != std::array<std::uint8_t, 3>{0xa5, 0x5a, 0xc3}) {
        std::cerr << "FAIL later-block I/O error changed caller output or returned wrong status\n"; return 1;
    }
    const auto retried = archive.read_file(resolved.value->id, 65534, output.size(), output.data(), output.size());
    if (!retried || output[0] != static_cast<std::uint8_t>(((65534u * 31u + 7u) & 0xffu)) ||
        output[1] != static_cast<std::uint8_t>(((65535u * 31u + 7u) & 0xffu)) ||
        output[2] != static_cast<std::uint8_t>(((65536u * 31u + 7u) & 0xffu))) {
        std::cerr << "FAIL atomic-read retry returned wrong bytes\n"; return 1;
    }
    // The default (fast) mode keeps the verified first block and reports it exactly.
    auto fast_open = nx::Archive::open(source);
    if (!fast_open) return 1;
    nx::Archive fast = std::move(*fast_open.value);
    if (!fast.mount()) return 1;
    const auto expected_byte = [](std::uint64_t at) { return static_cast<std::uint8_t>((at * 31u + 7u) & 0xffu); };
    output = {0xa5, 0x5a, 0xc3};
    source.fail_reads_covering(second_block_payload);
    const auto partial = fast.read_file(resolved.value->id, 65534, output.size(), output.data(), output.size());
    source.disarm();
    if (partial || partial.error.code != nx::ErrorCode::io || partial.bytes != 2 ||
        output[0] != expected_byte(65534) || output[1] != expected_byte(65535) || (output[2] != 0xc3 && output[2] != 0)) {
        std::cerr << "FAIL partial read did not keep exactly the verified prefix after I/O failure\n"; return 1;
    }
    // A successful partial multi-block read stages nothing: only RAW slice scratch.
    std::vector<std::uint8_t> range(static_cast<std::size_t>(std::min<std::uint64_t>(resolved.value->size, 3 * 65536)) - 100, 0);
    test_allocations.store(0, std::memory_order_relaxed);
    count_test_allocations.store(true, std::memory_order_relaxed);
    const auto whole = fast.read_file(resolved.value->id, 100, range.size(), range.data(), range.size());
    count_test_allocations.store(false, std::memory_order_relaxed);
    if (!whole || whole.bytes != range.size() || test_allocations.load(std::memory_order_relaxed) > 1) {
        std::cerr << "FAIL partial multi-block read failed or allocated a staging buffer\n"; return 1;
    }
    for (std::size_t i = 0; i < range.size(); ++i)
        if (range[i] != expected_byte(100 + i)) { std::cerr << "FAIL partial multi-block read returned wrong bytes\n"; return 1; }
    std::vector<std::uint8_t> atomic_range(range.size(), 0);
    if (!archive.read_file(resolved.value->id, 100, atomic_range.size(), atomic_range.data(), atomic_range.size()) || atomic_range != range) {
        std::cerr << "FAIL atomic and partial multi-block reads differ\n"; return 1;
    }
    std::cout << "PASS multi-block output remains unchanged after second-block I/O failure; partial read keeps the verified prefix\n";
    return 0;
}

static int concurrent_zstd_reads(const std::string& path) {
    FileSource file(path);
    if (!file.good() || file.size() > 64ull * 1024 * 1024) return 2;
    std::vector<std::uint8_t> bytes(static_cast<std::size_t>(file.size()));
    if (!file.read_at(0, bytes.data(), bytes.size())) return 2;
    MemorySource source(bytes);
    nx::OpenOptions options;
    options.zstd = &thread_local_zstd_decode;
    // Exercise simultaneous cache misses and hits on independent decoder contexts.
    options.max_cached_block_bytes = 256 * 1024;
    options.max_cached_blocks = 2;
    auto opened = nx::Archive::open(source, options);
    if (!opened) { std::cerr << "FAIL concurrent Zstandard fixture open\n"; return 1; }
    nx::Archive archive = std::move(*opened.value);
    if (!archive.mount()) { std::cerr << "FAIL concurrent Zstandard fixture mount\n"; return 1; }
    auto file_info = archive.resolve_file(0, "compressed.bin");
    if (!file_info) { std::cerr << "FAIL concurrent Zstandard file lookup\n"; return 1; }
    std::atomic<bool> ok{true};
    std::array<std::thread, 4> readers;
    for (auto& thread : readers) thread = std::thread([&archive, &ok, id=file_info.value->id]() {
        for (unsigned i = 0; i < 80; ++i) {
            std::array<std::uint8_t, 16> output{};
            if (!archive.read_file(id, 1024, output.size(), output.data(), output.size()) ||
                !std::all_of(output.begin(), output.end(), [](std::uint8_t value) { return value == 'A'; })) ok = false;
        }
    });
    for (auto& thread : readers) thread.join();
    if (!check(ok, "concurrent Zstandard reads failed with thread-local decoder contexts")) return 1;
    std::cout << "PASS concurrent Zstandard reads with immutable memory source and per-thread decoder contexts\n";
    return 0;
}

static int zstd_cases(const std::string& directory, RealZstd& decoder) {
    const std::array<std::pair<const char*, bool>, 8> cases = {{{"valid.zst", true}, {"truncated.zst", false},
        {"wrong_size.zst", false},
        {"concatenated.zst", false}, {"trailing.zst", false}, {"skippable.zst", false},
        {"dictionary.zst", false}, {"large_window.zst", false}}};
    for (const auto& item : cases) {
        std::ifstream input(directory + "/" + item.first, std::ios::binary);
        if (!input) { std::cerr << "FAIL missing Zstandard decoder case " << item.first << '\n'; return 2; }
        std::vector<std::uint8_t> frame((std::istreambuf_iterator<char>(input)), std::istreambuf_iterator<char>());
        const std::string name(item.first);
        const std::size_t expected = name == "dictionary.zst" ? 4352 :
            (name == "large_window.zst" ? 896000 : 4096);
        std::vector<std::uint8_t> output(expected);
        std::size_t written = 0, consumed = 0;
        const bool result = RealZstd::decode(&decoder, frame.data(), frame.size(), output.data(), output.size(), &written, &consumed);
        if (result != item.second || (result && (written != expected || consumed != frame.size()))) {
            std::cerr << "FAIL Zstandard decoder rule " << item.first << '\n'; return 1;
        }
    }
    std::cout << "PASS real Zstandard decoder: valid, truncated, wrong-size, concatenated, trailing, skippable, dictionary, and window-limit cases\n";
    return 0;
}

static int generation_recovery_test(const std::string& path) {
    std::vector<std::uint8_t> original;
    if (!load_archive_bytes(path, original)) return 2;
    // Append a copy of the metadata tail and publish it in slot B, sharing
    // payload blocks with the original generation.
    constexpr std::size_t older = 0, newest = 4096;
    const auto tail = static_cast<std::size_t>(read_u64_le(original.data()+24));
    const std::vector<std::uint8_t> tail_bytes(original.begin()+tail, original.end());
    const auto next_tail = (original.size()+15) & ~std::size_t(15);
    const auto delta = next_tail-tail;
    original.resize(next_tail, 0);
    original.insert(original.end(), tail_bytes.begin(), tail_bytes.end());
    std::copy_n(original.data()+older, 256, original.data()+newest);
    auto put = [&](std::size_t field, std::uint64_t value, unsigned bytes) {
        for (unsigned i=0; i<bytes; ++i) original[newest+field+i]=std::uint8_t(value>>(8*i));
    };
    for (auto field : {24, 72, 88, 104}) put(field, read_u64_le(original.data()+older+field)+delta, 8);
    put(16, original.size(), 8);
    put(216, 1, 8);
    put(252, test_crc32c(original.data()+newest, 252), 4);
    MemorySource healthy_source(original);
    auto healthy = nx::Archive::open(healthy_source);
    if (!check(healthy && healthy.value->generation() == 1 && healthy.value->mount(),
               "healthy archive must use the newest generation")) return 1;

    class FailedHeaderPage final : public nx::RandomAccessSource {
    public:
        FailedHeaderPage(const std::vector<std::uint8_t>& bytes, unsigned failed)
            : source_(bytes), failed_(failed) {}
        std::uint64_t size() const noexcept override { return source_.size(); }
        bool read_at(std::uint64_t offset, std::uint8_t* out, std::size_t count) override {
            if (offset < 8192) ++header_reads;
            for (unsigned slot=0; slot<2; ++slot) {
                const auto start = std::uint64_t(slot)*4096;
                if ((failed_ & (1u << slot)) && offset < start + 4096 && start < offset + count)
                    return false;
            }
            return source_.read_at(offset, out, count);
        }
        unsigned header_reads = 0;
    private:
        MemorySource source_;
        unsigned failed_;
    };
    FailedHeaderPage intact(original, 0u);
    if (!check(nx::Archive::open(intact) && intact.header_reads == 1,
               "healthy header pages should be read together")) return 1;

    for (const auto failed : {1u, 2u}) {
        FailedHeaderPage source(original, failed);
        auto opened = nx::Archive::open(source);
        if (!check(opened && opened.value->generation() == (failed == 1u ? 1u : 0u) &&
                   opened.value->mount(), "one unreadable header page must leave the other usable"))
            return 1;
    }

    FailedHeaderPage unavailable(original, 3u);
    const auto failed = nx::Archive::open(unavailable);
    if (!check(!failed && failed.error.code == nx::ErrorCode::io,
               "two unreadable header pages must report an I/O error")) return 1;

    for (const auto field : {24, 72, 88, 104}) {
        auto damaged = original;
        const auto offset = read_u64_le(damaged.data()+newest+field);
        if (!check(offset != 0 && offset != read_u64_le(damaged.data()+older+field),
                   "fixture must have independent generation tails")) return 1;
        damaged[offset] ^= 1;
        MemorySource source(damaged);
        auto opened = nx::Archive::open(source);
        if (!check(opened && opened.value->generation() == 0 && opened.value->mount(),
                   "damaged newest tail must recover the previous generation")) return 1;

        damaged[read_u64_le(damaged.data()+older+field)] ^= 1;
        MemorySource both_damaged(damaged);
        if (!check(!nx::Archive::open(both_damaged),
                   "two damaged tails must be rejected")) return 1;
    }
    std::cout << "PASS recovery from damaged Catalog, Manifest, index, and integrity table\n";
    return 0;
}

int main(int argc,char** argv) {
    RealZstd zstd;
    if (!zstd.ready()) { std::cerr << "FAIL real Zstandard decoder unavailable: " << zstd.error() << '\n'; return 2; }
    if (argc==3 && std::string(argv[1])=="--fuzz") return fuzz_mutations(argv[2]);
    if (argc==3 && std::string(argv[1])=="--review-regressions") return review_regressions(argv[2]);
    if (argc==3 && std::string(argv[1])=="--mount-retry") return mount_retry(argv[2]);
    if (argc==3 && std::string(argv[1])=="--atomic-multiblock") return atomic_multiblock_read(argv[2]);
    if (argc==3 && std::string(argv[1])=="--concurrent-zstd") return concurrent_zstd_reads(argv[2]);
    if (argc==3 && std::string(argv[1])=="--zstd-cases") return zstd_cases(argv[2], zstd);
    if (argc==3 && std::string(argv[1])=="--raw-slice") return raw_slice_tests(argv[2]);
    if (argc==3 && std::string(argv[1])=="--media-type") return pre_mount_media_type_test(argv[2]);
    if (argc==3 && std::string(argv[1])=="--metadata-history") return metadata_history_association_test(argv[2]);
    if (argc==3 && std::string(argv[1])=="--generation-recovery") return generation_recovery_test(argv[2]);
    if (argc==3 && std::string(argv[1])=="--header-digest") return captured_manifest_digest_test(argv[2]);
    if (argc==4 && std::string(argv[1])=="--sha-provider") return sha_provider_tests(argv[2], argv[3]);
    if (argc!=3) { std::cerr << "usage: nx_reader_smoke <archive.nx> <accept|reject_open|reject_mount|reject_verify>\n"; return 2; }
    const std::string path=argv[1], expected=argv[2];
    FileSource source(path);
    if (!source.good()) { std::cerr << "FAIL cannot open fixture: " << path << '\n'; return 2; }
    nx::OpenOptions options; options.zstd=&RealZstd::decode; options.zstd_context=&zstd;
    auto opened=nx::Archive::open(source,options);
    if (expected=="reject_open") {
        if (opened) { std::cerr << "FAIL fixture unexpectedly passed open: " << path << '\n'; return 1; }
        std::cout << "PASS open rejection " << path << '\n'; return 0;
    }
    if (!opened) { std::cerr << "FAIL open rejected " << path << ": " << opened.error.message << '\n'; return 1; }
    nx::Archive archive=std::move(*opened.value);
    const auto file_name=path.substr(path.find_last_of("/\\")==std::string::npos?0:path.find_last_of("/\\")+1);
    if (file_name=="switch2_localized_icons.nx") {
        if (!check(archive.title(0)=="Synthetic Switch 2 Title" && archive.localized_entries().size()==2 &&
                   archive.localized_language(0)=="fr" && archive.localized_language(1)=="ja" &&
                   archive.localized_title(1)=="\xE6\x9E\xB6\xE7\xA9\xBA\xE3\x81\xAE\xE4\xBD\x9C\xE5\x93\x81" &&
                   archive.localized_entries()[0].icon_index==0 && archive.localized_entries()[1].icon_index==0 &&
                   archive.applications()[0].metadata_snapshot==0,
                   "Switch 2 Catalog metadata is not available immediately after open")) return 1;
        auto icon=archive.read_media(archive.localized_entries()[0].icon_index);
        if (!check(icon && icon.value->size()>8 && (*icon.value)[0]==0x89 && (*icon.value)[1]=='P',
                   "on-demand icon read before mount failed")) return 1;
    } else if (file_name=="jpeg_icon.nx") {
        auto icon = archive.read_media(archive.applications()[0].icon_index);
        const std::array<std::uint8_t, 32> exact_digest = {0x54,0x7f,0x70,0x3d,0xf4,0x4e,0x03,0xad,
            0x59,0x47,0xfa,0xf6,0x27,0x20,0xa7,0x7d,0x6c,0xb1,0xe3,0x32,0x48,0x51,0x79,0x97,
            0xb9,0x78,0xe0,0xef,0x26,0x14,0xb6,0xb6};
        if (!check(icon && icon.value->size() == 285 && (*icon.value)[0] == 0xff && (*icon.value)[1] == 0xd8 &&
                   (*icon.value)[283] == 0xff && (*icon.value)[284] == 0xd9 &&
                   nx::Sha256::digest(icon.value->data(), icon.value->size()) == exact_digest,
                   "on-demand JPEG bytes changed or were recompressed")) return 1;
    }
    auto mounted=archive.mount();
    if (expected=="reject_mount") {
        if (mounted) { std::cerr << "FAIL fixture unexpectedly passed mount: " << path << '\n'; return 1; }
        std::cout << "PASS mount rejection " << path << '\n'; return 0;
    }
    if (!mounted) { std::cerr << "FAIL mount rejected " << path << ": " << mounted.error.message << '\n'; return 1; }
    if (expected=="reject_verify") {
        auto verify=archive.verify_all();
        if (verify) { std::cerr << "FAIL fixture unexpectedly passed full verification: " << path << '\n'; return 1; }
        if (file_name=="corrupted_block_payload.nx") {
            auto file_info=archive.resolve_file(0,"data.bin");
            if (!file_info || file_info.value->size>16) { std::cerr << "FAIL corrupted block test file lookup\n"; return 1; }
            std::array<std::uint8_t,16> output{}; output.fill(0xa5);
            const auto length=static_cast<std::size_t>(file_info.value->size);
            const auto read=archive.read_file(file_info.value->id,0,length,output.data(),output.size());
            // Default reads leave failed bytes untouched or zeroed, never unverified.
            if (read || read.error.code!=nx::ErrorCode::integrity || read.bytes!=0 ||
                !std::all_of(output.begin(),output.begin()+static_cast<std::ptrdiff_t>(length),[](std::uint8_t b){return b==0xa5 || b==0;})) {
                std::cerr << "FAIL corrupt block was not rejected before modifying read output\n"; return 1;
            }
        }
        std::cout << "PASS full-verification rejection " << path << '\n'; return 0;
    }
    if (expected!="accept") { std::cerr << "FAIL unknown expected state\n"; return 2; }
    if (!check(archive.verify_all().ok(),"valid fixture failed full verification")) return 1;

    if (file_name=="raw_only.nx") {
        auto root = archive.root_directory(0);
        if (!check(root.ok(), "root directory access failed")) return 1;
        unsigned enumerated = 0;
        nx::FileId enumerated_id{};
        test_allocations.store(0, std::memory_order_relaxed);
        count_test_allocations.store(true, std::memory_order_relaxed);
        const auto enumeration = archive.enumerate(*root.value, [&](const nx::DirectoryEntryView& entry) {
            ++enumerated;
            if (entry.name != "data.bin" || entry.type != nx::EntryType::file || entry.size != 4) return false;
            enumerated_id = entry.file;
            return true;
        });
        count_test_allocations.store(false, std::memory_order_relaxed);
        if (!check(enumeration.ok() && enumerated == 1, "VFS enumeration returned wrong file metadata") ||
            !check(test_allocations.load(std::memory_order_relaxed) == 0, "VFS enumeration allocated for a small directory")) return 1;
        auto resolved = archive.resolve_file(0, "data.bin");
        if (!check(resolved && resolved.value->id.value == enumerated_id.value && resolved.value->size == 4,
                   "path resolution and directory enumeration disagree")) return 1;
        auto info = archive.file_info(enumerated_id);
        if (!check(info && info.value->type == nx::EntryType::file && info.value->size == 4, "file_info rejected an enumerated FileId")) return 1;
        auto invalid_info = archive.file_info(nx::FileId{0xffffffffu});
        std::array<std::uint8_t, 1> invalid_output{};
        auto invalid_read = archive.read_file(nx::FileId{0xffffffffu}, 0, 0, invalid_output.data(), invalid_output.size());
        if (!check(!invalid_info && invalid_info.error.code == nx::ErrorCode::bounds &&
                   !invalid_read && invalid_read.error.code == nx::ErrorCode::bounds,
                   "out-of-range FileId was not rejected with bounds errors")) return 1;
        std::array<std::uint8_t,4> bytes{};
        if (!check(archive.read_file(0,"data.bin",0,bytes.size(),bytes.data(),bytes.size()).ok() && bytes==std::array<std::uint8_t,4>{'N','X','!','?'},"RAW random read mismatch")) return 1;
        bytes.fill(0);
        test_allocations.store(0, std::memory_order_relaxed);
        count_test_allocations.store(true, std::memory_order_relaxed);
        const auto raw_single_read = archive.read_file(enumerated_id, 0, bytes.size(), bytes.data(), bytes.size());
        count_test_allocations.store(false, std::memory_order_relaxed);
        if (!check(raw_single_read.ok() && bytes==std::array<std::uint8_t,4>{'N','X','!','?'}, "FileId random read mismatch") ||
            !check(test_allocations.load(std::memory_order_relaxed) == 0, "single-block RAW read allocated (the direct read needs no buffer)")) return 1;
        std::atomic<bool> concurrent_ok{true};
        std::array<std::thread, 4> readers;
        for (auto& thread : readers) thread = std::thread([&archive, &concurrent_ok, enumerated_id]() {
            for (unsigned i = 0; i < 100; ++i) {
                std::array<std::uint8_t, 4> result{};
                if (!archive.read_file(enumerated_id, 0, result.size(), result.data(), result.size()) ||
                    result != std::array<std::uint8_t, 4>{'N', 'X', '!', '?'}) concurrent_ok = false;
            }
        });
        for (auto& thread : readers) thread.join();
        if (!check(concurrent_ok, "concurrent RAW reads failed with a thread-safe source")) return 1;
        nx::OpenOptions limited_options;
        limited_options.max_read_bytes = 3;
        auto limited_open = nx::Archive::open(source, limited_options);
        if (!check(limited_open.ok(), "low read-budget archive open failed")) return 1;
        nx::Archive limited_archive = std::move(*limited_open.value);
        if (!check(limited_archive.mount().ok(), "low read-budget archive mount failed")) return 1;
        std::array<std::uint8_t, 4> too_large{};
        auto limited_read = limited_archive.read_file(0, "data.bin", 0, too_large.size(), too_large.data(), too_large.size());
        if (!check(!limited_read && limited_read.error.code == nx::ErrorCode::resource_limit,
                   "configured read-buffer limit was not enforced")) return 1;
    } else if (file_name=="nested_vfs.nx") {
        auto root = archive.root_directory(0);
        if (!check(root.ok(), "nested archive root lookup failed")) return 1;
        nx::DirectoryHandle folder{};
        unsigned root_entries = 0;
        auto root_status = archive.enumerate(*root.value, [&](const nx::DirectoryEntryView& entry) {
            ++root_entries;
            if (entry.name == "folder" && entry.type == nx::EntryType::directory) folder = entry.directory;
            return true;
        });
        if (!check(root_status.ok() && root_entries == 2 && folder.directory_index != 0xffffffffu,
                   "directory enumeration did not return child directory and file")) return 1;
        nx::FileId child{};
        unsigned child_entries = 0;
        auto child_status = archive.enumerate(folder, [&](const nx::DirectoryEntryView& entry) {
            ++child_entries;
            if (entry.name == "child.bin" && entry.type == nx::EntryType::file && entry.size == 12) child = entry.file;
            return true;
        });
        if (!check(child_status.ok() && child_entries == 1 && child.value != 0xffffffffu,
                   "child directory enumeration returned wrong file")) return 1;
        auto resolved = archive.resolve_file(0, "folder/child.bin");
        std::array<std::uint8_t, 12> contents{};
        if (!check(resolved && resolved.value->id.value == child.value &&
                   archive.read_file(child, 0, contents.size(), contents.data(), contents.size()).ok() &&
                   std::string(contents.begin(), contents.end()) == "nested child",
                   "resolved nested path could not be read through FileId")) return 1;
    } else if (file_name=="multi_block_file.nx") {
        std::array<std::uint8_t,4> first{},last{};
        if (!check(archive.read_file(0,"large.bin",0,first.size(),first.data(),first.size()).ok(),"first chunk read failed")) return 1;
        if (!check(archive.read_file(0,"large.bin",65533,last.size(),last.data(),last.size()).ok(),"cross-chunk read failed")) return 1;
        for (std::size_t i=0;i<first.size();++i) if (first[i]!=static_cast<std::uint8_t>((i*31+7)&0xff)) return 1;
        for (std::size_t i=0;i<last.size();++i) if (last[i]!=static_cast<std::uint8_t>(((65533+i)*31+7)&0xff)) return 1;
    } else if (file_name=="zstd_single.nx") {
        std::array<std::uint8_t,16> bytes{};
        auto compressed_file=archive.resolve_file(0,"compressed.bin");
        if (!check(compressed_file.ok(),"compressed FileId resolution failed")) return 1;
        test_allocations.store(0, std::memory_order_relaxed);
        count_test_allocations.store(true, std::memory_order_relaxed);
        const auto compressed_read=archive.read_file(compressed_file.value->id,1024,bytes.size(),bytes.data(),bytes.size());
        count_test_allocations.store(false, std::memory_order_relaxed);
        if (!check(compressed_read.ok(),"Zstandard callback read failed") ||
            !check(test_allocations.load(std::memory_order_relaxed)==2,"single-block Zstandard read allocated beyond stored and decoded block buffers")) return 1;
        for (auto b:bytes) if (b!='A') return 1;
        const auto& build=archive.snapshots()[0].build_id;
        if (!check(build[0]==0x00 && build[1]==0x11 && build[30]==0xfe && build[31]==0x0f,"full 32-byte snapshot Build ID was not preserved")) return 1;
    } else if (file_name=="multiple_programs.nx") {
        std::array<std::uint8_t,11> bytes{};
        if (!check(archive.read_file(0,"one.bin",0,bytes.size(),bytes.data(),bytes.size()).ok(),"program snapshot read failed") ||
            !check(std::string(bytes.begin(),bytes.end())=="program one","program file contents differ")) return 1;
    } else if (file_name=="metadata_snapshot_tie.nx") {
        if (!check(archive.applications()[0].metadata_snapshot==1,
                   "equal-version metadata source did not use greatest Catalog snapshot index") ||
            !check(archive.title(0)=="Current source title" && archive.publisher(0)=="Current source publisher" &&
                   archive.localized_entries().size()==1 && archive.localized_language(0)=="fr",
                   "selected snapshot metadata was not kept in the fast application row")) return 1;
        auto historical_icon=archive.read_media(1);
        auto historical_nacp=archive.read_media(2);
        if (!check(historical_icon && historical_icon.value->size()>8 && (*historical_icon.value)[0]==0x89 && (*historical_icon.value)[1]=='P',
                   "historical source icon reference is invalid") ||
            !check(historical_nacp && std::string(historical_nacp.value->begin(),historical_nacp.value->end())=="synthetic native NACP for snapshot zero",
                   "historical source NACP reference is invalid")) return 1;
    } else if (file_name=="switch2_localized_icons.nx") {
        if (!check(archive.title(0)=="Synthetic Switch 2 Title" && archive.localized_entries().size()==2 &&
                   archive.localized_language(0)=="fr" && archive.localized_language(1)=="ja" &&
                   archive.localized_title(1)=="\xE6\x9E\xB6\xE7\xA9\xBA\xE3\x81\xAE\xE4\xBD\x9C\xE5\x93\x81" && archive.applications()[0].metadata_snapshot==0,
                   "Switch 2 listing metadata missing")) return 1;
        auto icon=archive.read_media(0);
        if (!check(icon && icon.value->size()>8 && (*icon.value)[0]==0x89 && (*icon.value)[1]=='P',"on-demand icon read failed")) return 1;
    } else if (file_name=="distinct_dlc_same_tree.nx") {
        if (!check(archive.snapshots().size()==3 &&
                   archive.snapshots()[1].identity!=archive.snapshots()[2].identity,
                   "distinct DLC content IDs produced the same snapshot identity")) return 1;
    } else if (file_name=="dlc_with_program.nx") {
        if (!check(archive.snapshots().size()==2 &&
                   archive.snapshots()[1].program_index==0,
                   "explicit DLC Program ID was not preserved")) return 1;
    } else if (file_name=="dlc_without_program.nx") {
        if (!check(archive.snapshots().size()==2 && archive.snapshots()[1].program_index==0xffffffffu,"unassociated DLC Program ID was not preserved")) return 1;
    } else if (file_name=="dlc_only_application.nx") {
        if (!check(archive.programs().empty() && archive.applications()[0].program_count==0 &&
                   archive.applications()[0].first_program==0xffffffffu && archive.snapshots()[0].program_index==0xffffffffu,
                   "DLC-only application gained a fabricated Program row")) return 1;
    }
    std::cout << "PASS reader " << path << '\n';
    return 0;
}
