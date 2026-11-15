// Intentionally uses only pre-extra-file public API. Compile unchanged against
// an old reader header to prove that an optional extra tree is ignored.
#include "nx_reader.hpp"

#include <algorithm>
#include <array>
#include <cstdint>
#include <fstream>
#include <iterator>
#include <iostream>
#include <utility>
#include <vector>

struct ArchiveBytes : nx::RandomAccessSource {
    explicit ArchiveBytes(const char* filename) {
        std::ifstream input(filename, std::ios::binary);
        bytes.assign(std::istreambuf_iterator<char>(input), std::istreambuf_iterator<char>());
    }
    std::uint64_t size() const noexcept override { return bytes.size(); }
    bool read_at(std::uint64_t offset, std::uint8_t* output, std::size_t length) override {
        if (offset > bytes.size() || length > bytes.size() - offset) return false;
        std::copy_n(bytes.data() + static_cast<std::size_t>(offset), length, output);
        return true;
    }
    std::vector<std::uint8_t> bytes;
};

int main(int argc, char** argv) {
    if (argc != 2) return 2;
    ArchiveBytes source(argv[1]);
    auto opened = nx::Archive::open(source);
    if (!opened) { std::cerr << "open failed: " << opened.error.message << '\n'; return 1; }
    auto archive = std::move(*opened.value);
    const auto mounted = archive.mount();
    if (!mounted) { std::cerr << "mount failed: " << mounted.error.message << '\n'; return 1; }
    auto info = archive.resolve_file(0, "game.bin");
    if (!info || info.value->size != 4) return 1;
    std::array<std::uint8_t, 4> result{};
    auto read = archive.read_file(info.value->id, 0, result.size(), result.data(), result.size());
    if (!read || result != std::array<std::uint8_t, 4>{'G', 'A', 'M', 'E'}) return 1;
    if (!archive.verify_all()) return 1;
    std::cout << "PASS optional extra files do not change the game\n";
    return 0;
}
