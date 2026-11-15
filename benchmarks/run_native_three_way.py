"""Fresh NUP/main/feature comparison with an explicit 16-pending-read contract."""
import argparse,hashlib,json,os,subprocess,tempfile
from pathlib import Path
from run_native_nup_comparison import ROOT,build_tool

def main():
 p=argparse.ArgumentParser();p.add_argument('--egl-build',type=Path,required=True);p.add_argument('--nup',type=Path,required=True);p.add_argument('--nx',type=Path,required=True);p.add_argument('--synthetic',type=Path,required=True);p.add_argument('--traces',type=Path,required=True);p.add_argument('--first-touch',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--repetitions',type=int,default=5);p.add_argument('--main-ref',default='origin/main');a=p.parse_args();a.output=a.output.resolve();a.output.mkdir(parents=True,exist_ok=True)
 if not 1<=a.repetitions<=20:p.error('repetitions must be 1..20')
 main_sha=subprocess.check_output(['git','rev-parse',a.main_ref],cwd=ROOT,text=True).strip()
 feature_sha=subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip()
 executables={};provenance={}
 for key,original in [('main',True),('feature',False)]:
  folder=a.output/key;folder.mkdir(exist_ok=True)
  executables[key],provenance[key]=build_tool(a.egl_build.resolve(),folder,original,main_sha,ROOT/'benchmarks/native_three_way_batch.cpp')
  if provenance[key]['nup_sources_dirty']:raise RuntimeError('external NUP source must be unchanged')
 cases=[]
 for corpus in ('real','raw','mixed'):
  pair=(a.nup,a.nx) if corpus=='real' else (a.synthetic/('raw.nup' if corpus=='raw' else 'zstd.nup'),a.synthetic/('raw.nx' if corpus=='raw' else 'zstd.nx'))
  for scenario in (('random4k','clustered4k','bursty4k','sequential64k') if corpus=='real' else ('random4k','sequential64k')):
   for budget in ((0,8,32) if scenario=='clustered4k' else ((8,32,128) if corpus=='real' and scenario=='random4k' else (8,32))):
    for warm in (False,True):cases.append((corpus,scenario,pair,a.traces/f'{corpus}-{scenario}-private.tsv',budget,warm))
 for warm in (False,True):cases.append(('real','first_touch4k',(a.nup,a.nx),a.first_touch,128,warm))
 records=[];expected={};variants=['nup','main','current','selective']
 for corpus,scenario,pair,trace,budget,warm in cases:
  for rep in range(a.repetitions):
   rotation=variants[rep%4:]+variants[:rep%4]
   for variant in rotation:
    exe=executables['main' if variant in ('main','nup') else 'feature'];output=a.output/'output.bin'
    env=os.environ.copy();env['NX_BENCH_MAPPED']='1'
    with tempfile.TemporaryDirectory(prefix='spill-',dir=a.output) as spill:
     result=subprocess.run([str(exe),*map(str,pair),str(trace),str(output),str(budget),spill,variant,str(int(warm))],capture_output=True,text=True,check=True,env=env)
    digest=hashlib.sha256(output.read_bytes()).hexdigest();key=(corpus,scenario)
    if key in expected and expected[key]!=digest:raise RuntimeError('full original-order output mismatch')
    expected[key]=digest
    records.append(dict(corpus=corpus,scenario=scenario,budget_mib=budget,warmup=warm,repetition=rep,variant=variant,metrics=json.loads(result.stdout),output_sha256=digest))
   (a.output/'progress.json').write_text(json.dumps({'records':records},indent=2))
  print(corpus,scenario,budget,warm,'completed',flush=True)
 report=dict(main_commit=main_sha,feature_commit=feature_sha,provenance=provenance,method='Native C++ readers, same mapped input provider/compiler/static Zstd. Same pre-resolved requests, common 16-pending-request timing windows, buffers and original-order staging copy. Off/main/current are ordinary per-request reads; selective explicitly runs the feature batching planner. Current RAW verification reuse opt-in enabled; main retains original per-read checks. No prefetch or Session scheduling here. Full output SHA-256 checked for every configuration. Warmup primes the same reader before timing. No OS-cache flush; initial pass is not cold disk. Output storage and IO occur after timing/memory sampling. Memory peaks include common output buffers, metadata, priming and temporary decoder buffers. Live NUP decoded temporary files counted separately. No game bytes or paths published.',records=records)
 (a.output/'public-results.json').write_text(json.dumps(report,indent=2)+'\n');print('completed',len(records),'isolated runs')
if __name__=='__main__':main()
