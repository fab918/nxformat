"""Benchmark bounded batch merging and two existing Session prefetch policies.
Private inputs and request manifests remain under --output.
"""
import argparse,hashlib,json,random,subprocess,sys
from pathlib import Path
from nup_vs_nx import parse,walk
ROOT=Path(__file__).resolve().parents[1]

def main():
 p=argparse.ArgumentParser();p.add_argument('--nup',type=Path,required=True);p.add_argument('--nx',type=Path,required=True);p.add_argument('--synthetic',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--repetitions',type=int,default=5);p.add_argument('--warm',action='store_true');p.add_argument('--modes',nargs='+',choices=('off','batch','adaptive','prefetch','guarded'),default=['off','batch','prefetch','guarded']);p.add_argument('--think',type=int,nargs='+',default=[0,50]);a=p.parse_args();
 if not 1<=a.repetitions<=20:p.error('repetitions must be 1..20')
 a.output=a.output.resolve();a.output.mkdir(exist_ok=True,parents=True)
 sys.path.insert(0,str(ROOT/'tests/conformance'));from test_native_conversion import compile_reader
 exe=compile_reader(a.output,ROOT/'benchmarks/batch_prefetch_probe.cpp')
 packages,d,m,c,_=parse(a.nup);tree=[];walk(d,m,c,next(x for x in packages if x['kind']==0)['romfs'],'romfs',tree)
 files=sorted([(p,f) for p,f in tree if f['codec']==1],key=lambda x:x[1]['size'],reverse=True)[:8]
 rng=random.Random(0x4e585517);cases=[]
 for kind in ('real','raw','mixed'):
  members=files if kind=='real' else [(f'romfs/asset-{i}.bin',{'size':4*2**20}) for i in range(8)]
  nx=a.nx if kind=='real' else a.synthetic/('raw.nx' if kind=='raw' else 'zstd.nx')
  traces={}
  traces['random4k']=[(p,rng.randrange(f['size']-4096+1),4096) for p,f in rng.choices(members,k=512)]
  traces['sequential64k']=[];total=0
  for path,f in members:
   for at in range(0,f['size']-65536+1,65536):
    if total>=32*2**20:break
    traces['sequential64k'].append((path,at,65536));total+=65536
   if total>=32*2**20:break
  if kind=='real':
   traces['clustered4k']=[];traces['bursty4k']=[]
   for batch in range(32):
    path,f=rng.choice(members);at=rng.randrange((f['size']-65536)//65536)*65536
    traces['clustered4k'] += [(path,at+rng.randrange(65536-4096+1),4096) for _ in range(16)]
   for burst in range(64):
    path,f=rng.choice(members);at=rng.randrange((f['size']-65536)//65536)*65536
    traces['bursty4k'] += [(path,at+i*4096,4096) for i in range(8)]
  for name,trace in traces.items():
   manifest=a.output/f'{kind}-{name}-private.tsv';manifest.write_text(''.join(f'{p}\t{o}\t{n}\n' for p,o,n in trace))
   for budget in ((0,8,32) if name=='clustered4k' else (8,32)):
    for think in (0,50):cases.append((kind,name,nx,manifest,budget,think))
 if a.warm:cases=[c for c in cases if c[4]==32 and c[5]==0]
 cases=[c for c in cases if c[5] in a.think]
 records=[];expected={};modes=a.modes
 for kind,name,nx,manifest,budget,think in cases:
  key=(kind,name)
  for rep in range(a.repetitions):
   rotation=modes[rep%len(modes):]+modes[:rep%len(modes)]
   for mode in rotation:
    output=a.output/'output.bin'
    run=subprocess.run([str(exe),str(nx),str(manifest),mode,str(budget),str(think),str(output),'16',*(['warm'] if a.warm else [])],capture_output=True,text=True,check=True)
    content=output.read_bytes();digest=hashlib.sha256(content).hexdigest()
    if key in expected:
     if digest!=expected[key]:raise RuntimeError('full output mismatch')
    else:expected[key]=digest
    records.append(dict(corpus=kind,scenario=name,budget_mib=budget,think_us=think,repetition=rep,mode=mode,metrics=json.loads(run.stdout),output_sha256=digest))
  print(kind,name,budget,think,'done',flush=True)
 (a.output/'public-results.json').write_text(json.dumps({'method':'Rotated repetitions per configuration; same requests and mapped provider. Batch horizon=16 already pending requests, no artificial queue-fill wait. Modes off/batch/existing Session prefetch after 3 sequential reads/guarded threshold 8. Split reads off. One background worker. Caller work 0 or synthetic busy-wait 50 us per logical request, paid after each common 16-request window. Every mode uses the same 16-request timing windows and output buffers. Generated traces, not recorded gameplay. Read service includes batch planning and merge/scatter. Session/worker shutdown is measured separately; total including shutdown is also reported. Original-order output staging copies are timed, common full-output storage is included in process RAM. Output file is written only after workers stop and measurements finish, then checked via SHA-256. OS cache uncontrolled. CPU and memory are entire-process peaks/costs; speculative payload stats sampled before worker drain. Decoder count after drain. No production changes.', 'warmup':a.warm,'repetitions':a.repetitions,'probe_sha256':hashlib.sha256((ROOT/'benchmarks/batch_prefetch_probe.cpp').read_bytes()).hexdigest(),'reader_sha256':hashlib.sha256((ROOT/'dist/nx_reader.hpp').read_bytes()).hexdigest(),'session_sha256':hashlib.sha256((ROOT/'dist/nx_session.hpp').read_bytes()).hexdigest(),'records':records},indent=2)+'\n')
 print('completed',len(records),'runs',flush=True)
if __name__=='__main__':main()
