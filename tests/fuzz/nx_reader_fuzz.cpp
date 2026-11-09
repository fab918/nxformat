// Coverage-guided reader fuzzer.
// Build: clang++ -std=c++17 -fsanitize=fuzzer,address,undefined -Idist
//        -Itests/support tests/fuzz/nx_reader_fuzz.cpp -ldl
//
// Each input runs twice: as given, then with its header CRCs and region
// SHA-256 digests recomputed. The second pass gets mutations past the hash
// checks into the Catalog, Manifest, index and block parsers. A mounted
// archive is then walked and read: lookups, file ranges, media, source
// resources and full verification. Any crash, sanitizer report or a read
// that claims more bytes than asked is a bug.
#include "nx_reader.hpp"
#include "real_zstd.hpp"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <vector>

namespace {

using Bytes = std::vector<std::uint8_t>;

class FuzzSource final : public nx::RandomAccessSource {
public:
    explicit FuzzSource(const Bytes& input) : input_(input) {}
    std::uint64_t size() const noexcept override { return input_.size(); }
    bool read_at(std::uint64_t offset, std::uint8_t* output, std::size_t length) override {
        if (offset > input_.size() || length > input_.size() - offset) return false;
        if (length) std::memcpy(output, input_.data() + offset, length);
        return true;
    }
private:
    const Bytes& input_;
};

std::uint64_t rd64(const Bytes& b, std::size_t at) {
    std::uint64_t v = 0;
    for (int i = 7; i >= 0; --i) v = (v << 8) | b[at + static_cast<std::size_t>(i)];
    return v;
}

void wr32(Bytes& b, std::size_t at, std::uint32_t v) {
    for (int i = 0; i < 4; ++i) b[at + static_cast<std::size_t>(i)] = static_cast<std::uint8_t>(v >> (8 * i));
}

// Recompute what a mutation invalidated in each header slot: the Catalog,
// Manifest and index digests, then the header CRC-32C (SPEC §2).
void resign(Bytes& b) {
    if (b.size() < 8192) return;
    for (std::size_t slot : {std::size_t(0), std::size_t(4096)}) {
        if (std::memcmp(b.data() + slot, "NXAR", 4) != 0) continue;
        auto region = [&](std::uint64_t offset, std::uint64_t length, std::size_t digest_at) {
            if (offset > b.size() || length > b.size() - offset) return;
            const auto digest = nx::Sha256::digest(b.data() + offset, static_cast<std::size_t>(length));
            std::memcpy(b.data() + slot + digest_at, digest.data(), digest.size());
        };
        region(rd64(b, slot + 24), rd64(b, slot + 32), 120);
        region(rd64(b, slot + 72), rd64(b, slot + 80), 152);
        const auto index = rd64(b, slot + 88), blocks = rd64(b, slot + 96);
        const auto integrity = rd64(b, slot + 104), integrity_size = rd64(b, slot + 112);
        if (blocks <= b.size() / 24 && index <= b.size() && blocks * 24 <= b.size() - index &&
            integrity <= b.size() && integrity_size <= b.size() - integrity) {
            nx::Sha256 hash;
            hash.update(b.data() + index, static_cast<std::size_t>(blocks * 24));
            hash.update(b.data() + integrity, static_cast<std::size_t>(integrity_size));
            const auto digest = hash.finish();
            std::memcpy(b.data() + slot + 184, digest.data(), digest.size());
        }
        wr32(b, slot + 252, nx::detail::crc32c(b.data() + slot, 252));
    }
}

void check_read(const nx::PartialRead& read, std::size_t asked) {
    if (read.bytes > asked) std::abort();
}

// Deterministic per input: offsets come from the file size and a counter.
void exercise_file(nx::Archive& archive, nx::FileId file, std::uint64_t size, std::uint32_t salt, Bytes& buffer) {
    const std::uint64_t starts[] = {0, size / 2, size ? size - 1 : 0, (size / 7) * (salt % 7)};
    const std::size_t lengths[] = {1, 4096, 16 * 1024 + 3, 1u << 20};
    for (std::size_t i = 0; i < 4; ++i) {
        const auto offset = std::min(starts[i], size);
        const auto length = static_cast<std::size_t>(std::min<std::uint64_t>(lengths[(i + salt) % 4], size - offset));
        buffer.resize(length + 1);
        check_read(archive.read_file(file, offset, length, buffer.data(), length), length);
    }
    // Out-of-range and undersized requests must fail cleanly.
    buffer.resize(2);
    check_read(archive.read_file(file, size, 1, buffer.data(), 1), 1);
    if (size > 1) check_read(archive.read_file(file, 0, 2, buffer.data(), 1), 2);
    (void)archive.file_digest(file);
    (void)archive.block_needs_decode(file, 0);
}

void exercise(const Bytes& bytes, RealZstd& zstd) {
    FuzzSource source(bytes);
    nx::OpenOptions options;
    if (zstd.ready()) { options.zstd = &RealZstd::decode; options.zstd_context = &zstd; }
    options.max_manifest_bytes = 8 * 1024 * 1024;
    options.max_directories = 100'000;
    options.max_files = 100'000;
    options.max_entries = 200'000;
    options.max_block_references = 500'000;
    options.max_blocks = 100'000;
    options.max_snapshot_nodes = 200'000;
    options.max_cached_blocks = 2;
    auto opened = nx::Archive::open(source, options);
    if (!opened) return;
    nx::Archive archive = std::move(*opened.value);
    for (std::uint32_t i = 0; i < archive.applications().size(); ++i) { (void)archive.title(i); (void)archive.publisher(i); }
    for (std::uint32_t i = 0; i < 64; ++i) (void)archive.read_media(i);
    if (!archive.mount()) return;

    Bytes buffer;
    std::uint32_t budget = 256;  // files read per input
    for (std::uint32_t s = 0; s < archive.snapshots().size(); ++s) {
        (void)archive.snapshot_display_version(s);
        (void)archive.dependencies(s, [](const nx::Dependency&) { return true; });
        auto root = archive.root_directory(s);
        if (!root) continue;
        std::vector<nx::DirectoryHandle> stack{*root.value};
        while (!stack.empty()) {
            const auto directory = stack.back();
            stack.pop_back();
            std::vector<nx::DirectoryEntryView> items;
            (void)archive.enumerate(directory, [&](const nx::DirectoryEntryView& view) { items.push_back(view); return true; });
            for (const auto& item : items) {
                (void)archive.lookup(directory, item.name);
                if (item.type == nx::EntryType::directory) { if (stack.size() < 4096) stack.push_back(item.directory); continue; }
                if (budget) { --budget; exercise_file(archive, item.file, item.size, budget, buffer); }
            }
        }
    }
    for (std::uint32_t r = 0; r < archive.source_resources().size(); ++r) {
        auto file = archive.source_resource_file(r, "partition.hfs0");
        if (file) exercise_file(archive, file.value->id, file.value->size, r, buffer);
    }
    for (std::uint32_t id = 0; id < archive.block_count() && id < 32; ++id) (void)archive.verify_block(id);
    (void)archive.verify_all();
}

}  // namespace

extern "C" int LLVMFuzzerTestOneInput(const std::uint8_t* data, std::size_t size) {
    if (!size || size > 16 * 1024 * 1024) return 0;
    static RealZstd zstd;
    Bytes bytes(data, data + size);
    exercise(bytes, zstd);
    resign(bytes);
    exercise(bytes, zstd);
    return 0;
}
