"""Targeted real compressed-member traces; matching local NUP/NX inputs required.
Uses a previously built native adapter. No private member paths are published.
"""
import argparse,csv,json,random,statistics,sys
from pathlib import Path
from nup_vs_nx import parse,walk
from run_low_memory_matrix import execute

def main():
 p=argparse.ArgumentParser();p.add_argument('--nup',type=Path,required=True);p.add_argument('--nx',type=Path,required=True);p.add_argument('--adapter',type=Path,required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args()
 a.output.mkdir(parents=True,exist_ok=True)
 packages,dirs,members,children,_=parse(a.nup);tree=[]
 walk(dirs,members,children,next(x for x in packages if x['kind']==0)['romfs'],'romfs',tree)
 selected=sorted([(path,f) for path,f in tree if f['codec']==1],key=lambda x:x[1]['size'],reverse=True)[:8]
 if len(selected)!=8:raise RuntimeError('need eight compressed members')
 rng=random.Random(0x4e581516)
 traces={'first_touch_4k':[(path,f['size']-65536,4096) for path,f in selected],
         'scatter_4k':[(path,rng.randrange(f['size']-4096+1),4096) for _ in range(64) for path,f in selected]}
 seq=[];total=0
 for path,f in selected:
  for offset in range(0,f['size'],262144):
   size=min(262144,f['size']-offset)
   if total+size>32*2**20:break
   seq.append((path,offset,size));total+=size
 traces['sequential_256k']=seq
 records=[];compared=0
 for name,trace in traces.items():
  listing=a.output/(name+'-private.tsv');listing.write_text(''.join(f'{p}\t{o}\t{n}\n' for p,o,n in trace))
  for budget in (32,128):
   metrics,_=execute(a.adapter,(a.nup,a.nx),listing,a.output/'check.csv',budget,'both',True,True,True);compared+=metrics['verified_reads']
   for repetition in range(3):
    for mode in (('nup','nx') if repetition%2==0 else ('nx','nup')):
     metrics,rows=execute(a.adapter,(a.nup,a.nx),listing,a.output/f'{name}-{budget}-{repetition}-{mode}.csv',budget,mode,True,mode=='nx',True)
     phases=[]
     for phase in (0,1):
      ns=sorted(int(r[mode+'_ns']) for r in rows if int(r['pass'])==phase)
      phases.append(dict(pass_index=phase,reads=len(ns),total_ms=sum(ns)/1e6,p50_us=statistics.median(ns)/1000,max_us=max(ns)/1000))
     records.append(dict(scenario=name,budget_mib=budget,repetition=repetition,reader=mode,metrics=metrics,phases=phases))
   print(name,budget,'done',flush=True)
 result=dict(method='Targeted architecture scenario: eight largest Zstd members of real archive A, not an overall-game workload. Same bounded mapped input; resolved handles; OS cache uncontrolled. Three fresh processes per reader/configuration. Initial and repeat trace passes. NUP unchanged; NX verification cache enabled.',member_sizes=[f['size'] for _,f in selected],fully_compared_reads=compared,records=records)
 (a.output/'public-results.json').write_text(json.dumps(result,indent=2)+'\n')
 for name in traces:
  for budget in (32,128):
   for mode in ('nup','nx'):
    rows=[r for r in records if (r['scenario'],r['budget_mib'],r['reader'])==(name,budget,mode)]
    print(name,budget,mode,'first/repeat ms',*[round(statistics.median(r['phases'][phase]['total_ms'] for r in rows),4) for phase in (0,1)],'RSS',round(statistics.median(r['metrics']['peak_working_set']/2**20 for r in rows),1))
if __name__=='__main__':main()
