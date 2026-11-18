"""CLI tests on extracted trees."""
from pathlib import Path
import subprocess
import tempfile
from test_native_conversion import compile_reader
from build_fixtures import SLOT_B
ROOT = Path(__file__).resolve().parents[2]
def main():
    cargo = ['cargo', 'run', '--release', '--locked', '--manifest-path', str(ROOT / 'packer/Cargo.toml')]
    with tempfile.TemporaryDirectory(prefix='nx-tree-cli-') as tmp:
        root = Path(tmp)
        subprocess.run(cargo + ['--example','extracted_cases','--',str(root/'fixture'),'1'],cwd=ROOT,check=True)
        subprocess.run(['cargo','build','--release','--locked','--manifest-path',str(ROOT/'packer/Cargo.toml'),'--bin','nxconvert'],cwd=ROOT,check=True)
        import os
        exe = ROOT/'packer/target/release'/('nxconvert.exe' if os.name=='nt' else 'nxconvert')
        source=root/'fixture/sources'; manifest=root/'fixture/declarations.nximport'
        archive=root/'converted.nx'; receipt=root/'converted.nxreceipt'
        def cli(*args, ok=True):
            result=subprocess.run([str(exe),*map(str,args)],capture_output=True,text=True)
            if ok: assert result.returncode==0, result.stdout+result.stderr
            else: assert result.returncode!=0, result.stdout+result.stderr
            return result.stdout
        cli('--help')
        os.environ['NX_TREE_PLATFORM']='switch2'
        auto=root/'automatic.nx'
        cli('pack-tree',source/'base',auto)
        cli('verify',auto,source/'base',str(auto)+'.nxreceipt')
        assert 'presumed' in Path(str(auto)+'.assumptions.txt').read_text().lower()
        cli('pack-tree',source,root/'ambiguous.nx',ok=False)
        cli('inspect-tree',source/'base','3072')
        assert 'DECLARED' in cli('inspect-tree',source,manifest,'b')
        cli('pack-tree',source,manifest,archive,receipt,'--memory-mib','96')
        info=cli('info',archive)
        assert 'version=20' in info and 'Dlc' in info and 'dependencies=' in info
        cli('verify',archive)
        assert 'compared_bytes=' in cli('verify',archive,source,receipt)
        reader=compile_reader(root)
        subprocess.run([str(reader),str(archive),str(root/'fixture/expected.tsv'),str(root/'fixture/selection.tsv')],cwd=ROOT,check=True)
        old=archive.read_bytes(); old_receipt=receipt.read_bytes()
        cli('pack-tree',source,manifest,archive,receipt,ok=False)
        assert archive.read_bytes()==old and receipt.read_bytes()==old_receipt
        cli('pack-tree',source,manifest,root/'latest.nx',root/'latest.receipt','--latest','--dlc','d')
        assert 'version=10 ' not in cli('info',root/'latest.nx')
        cli('verify',root/'latest.nx',source,root/'latest.receipt')
        # NX 0.2 survives one damaged header slot; break both.
        with archive.open('r+b') as f:
            f.seek(0); f.write(b'BROKEN!!'); f.seek(SLOT_B); f.write(b'BROKEN!!')
        cli('verify',archive,ok=False)
        cli('pack-tree',source,manifest,root/'failed.nx',root/'failed.receipt','--temp-mib','0',ok=False)
        assert not (root/'failed.nx').exists() and not (root/'failed.receipt').exists()
        # Declaration-free audit never promotes observed Program ID to application ID.
        assert 'UNKNOWN application/platform/version' in cli('inspect-tree',source/'base','3072')
    print('PASS minimal tree CLI, receipt preservation, Strict/source verification and independent C++ byte comparison')
if __name__ == '__main__': main()
