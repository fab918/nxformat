#include "nx_disk_source.hpp"
#include "real_zstd.hpp"
#include <fstream>
#include <iterator>
#include <iostream>
int main(int argc,char** argv) {
    if (argc!=3) return 2;
    const bool reject=std::string(argv[2])=="reject";
    auto source=nx::DiskSource::open(std::filesystem::path(argv[1]));
    if (!source) return reject?0:1;
    RealZstd zstd;
    nx::OpenOptions options;options.zstd=&RealZstd::decode;options.zstd_context=&zstd;
    auto opened=nx::Archive::open(**source.value,options);
    if (!opened) return reject?0:1;
    auto archive=std::move(*opened.value);
    if (!archive.mount()) return reject?0:1;
    if (reject) return 1;
    if (archive.source_resources().size()!=1 || archive.source_resources()[0].kind!=1) return 1;
    auto file=archive.source_resource_file(0,"partition.hfs0");if (!file) return 1;
    {
        auto missing=archive.source_resource_file(0,"other.bin");
        if (missing || missing.error.code!=nx::ErrorCode::not_found) return 1;
        auto out=archive.source_resource_file(1,"partition.hfs0");
        if (out || out.error.code!=nx::ErrorCode::bounds) return 1;
    }
    std::ifstream expected(argv[2],std::ios::binary);
    std::vector<std::uint8_t> bytes{std::istreambuf_iterator<char>(expected),{}};
    std::vector<std::uint8_t> actual(bytes.size());
    if (bytes.size()!=file.value->size || !archive.read_file(file.value->id,0,actual.size(),actual.data(),actual.size()) || actual!=bytes || !archive.verify_all()) return 1;
    for (std::uint32_t s=0;s<archive.snapshots().size();++s) {
        auto root=archive.root_directory(s);if (!root) return 1;
        bool source_leaked=false;
        auto status=archive.enumerate(*root.value,[&](const nx::DirectoryEntryView& e) {source_leaked|=e.name=="partition.hfs0";return true;});
        if (!status || source_leaked) return 1;
    }
    std::cout<<"PASS C++ source resource bytes, independent ownership and full verification\n";
}
