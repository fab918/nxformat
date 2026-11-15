"""Render the two README figures from published native-reader measurements."""
from pathlib import Path
import json
import statistics
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
from matplotlib.ticker import MaxNLocator

ROOT=Path(__file__).resolve().parents[1]
OUT=ROOT/'docs/benchmarks/low-memory-reader-cache'
NX='#3659E3';NUP='#D48728';INK='#19243C';MUTED='#626F85';GRID='#E8EDF5'
plt.rcParams.update({'font.family':'DejaVu Sans','font.size':11,'text.color':INK,'axes.labelcolor':MUTED,'xtick.color':MUTED,'ytick.color':INK,'axes.edgecolor':GRID,'axes.spines.top':False,'axes.spines.right':False,'axes.spines.left':False,'axes.spines.bottom':False,'figure.facecolor':'#F6F8FC','axes.facecolor':'white','savefig.facecolor':'#F6F8FC'})

def header(fig,title,subtitle):
 fig.text(.055,.95,'NX  /  NUP v1',fontsize=12,fontweight='bold',color=NX)
 fig.text(.055,.895,title,fontsize=26,fontweight='bold')
 fig.text(.055,.85,subtitle,fontsize=11,color=MUTED)
 fig.text(.77,.95,'●  NUP v1',color=NUP,fontsize=11,fontweight='bold')
 fig.text(.9,.95,'●  NX',color=NX,fontsize=11,fontweight='bold')

def bars(ax,labels,nup,nx,unit):
 for i,(label,n,x) in enumerate(zip(labels,nup,nx)):
  ax.barh(i+.16,n,height=.25,color=NUP,zorder=3)
  ax.barh(i-.16,x,height=.25,color=NX,zorder=3)
  ax.text(n+max(nup+nx)*.025,i+.16,f'{n:g}',va='center',color=INK,fontsize=11)
  ax.text(x+max(nup+nx)*.025,i-.16,f'{x:g}',va='center',color=INK,fontsize=11,fontweight='bold')
 ax.set_yticks(range(len(labels)),labels);ax.invert_yaxis();ax.tick_params(axis='both',length=0,pad=9)
 ax.set_xlim(0,max(nup+nx)*1.23);ax.xaxis.set_major_locator(MaxNLocator(4));ax.grid(axis='x',color=GRID,zorder=0);ax.set_xlabel(unit,labelpad=12)

r=json.loads((OUT/'results.json').read_text())
def get(s,v,field):return next(x[field] for x in r['aggregates'] if (x['corpus'],x['scenario'],x['cache_mib'],x['variant'],x['phase'])==('real_A',s,32,v,1))
scenarios=['random_4096','sequential_32mib','interleaved_8files']
labels=['Random 4 KiB','Sequential 256 KiB','Interleaved 64 KiB']
fig=plt.figure(figsize=(13,7.5));header(fig,'Fast repeated reads. A smaller memory footprint.','Real archive A  ·  32 MiB cache budget  ·  Repeated access patterns')
for rect,field,title,unit in [([.19,.27,.31,.46],'p99_us','Read latency · P99','Microseconds · lower is better'),([.66,.27,.28,.46],'peak_resident_mib','Peak resident memory','MiB · lower is better')]:
 ax=fig.add_axes(rect);bars(ax,labels if field=='p99_us' else ['','',''],[round(get(s,'nup_mapped',field),1) for s in scenarios],[round(get(s,'nx_verify_mapped',field),1) for s in scenarios],unit);ax.set_title(title,loc='left',fontsize=14,fontweight='bold',pad=18)
 if field=='p99_us':
  ratio=get('interleaved_8files','nup_mapped',field)/get('interleaved_8files','nx_verify_mapped',field)
  ax.text(3.2,2,f'NX: ~{ratio:.1f}× faster (P99)',va='center',color=NX,fontsize=10,fontweight='bold')
ratio=get('random_4096','nup_mapped','peak_resident_mib')/get('random_4096','nx_verify_mapped','peak_resident_mib')
fig.text(.055,.115,f'Random reads: NX uses 32% less resident memory. NUP v1 uses ~{ratio:.1f}× as much.',fontsize=13,fontweight='bold')
fig.text(.055,.065,'Native C++ readers · same mapped input · median of 3 runs · OS cache not flushed.',fontsize=10,color=MUTED)
fig.text(.055,.035,'Generated access patterns on real content. NUP v1 also wins reads shown here. These are not gameplay measurements.',fontsize=9,color=MUTED)
fig.savefig(OUT/'readme-repeated-reads.png',dpi=170);plt.close(fig)

r=json.loads((OUT/'real-compressed-results.json').read_text())
def measure(s,m,p):return statistics.median(x['phases'][p]['total_ms'] for x in r['records'] if (x['scenario'],x['budget_mib'],x['reader'])==(s,128,m))
fig=plt.figure(figsize=(13,9));header(fig,'Read the block you need. Skip the rest.','Real archive A  ·  Eight NUP-compressed files  ·  128 MiB cache fits all eight')
cases=[('first_touch_4k',0,'8 first accesses · 4 KiB','~109× faster for these initial reads'),('scatter_4k',0,'512 scattered reads · first pass','~6.8× faster on this trace'),('scatter_4k',1,'512 scattered reads · repeated','NUP v1 is faster once files are cached'),('sequential_256k',0,'Sequential · first 32 MiB','~1.9× faster on this trace')]
for i,(s,p,title,note) in enumerate(cases):
 col=i%2;row=i//2;left=.12+col*.46;bottom=.54-row*.30
 ax=fig.add_axes([left,bottom,.33,.15]);n=measure(s,'nup',p);x=measure(s,'nx',p)
 bars(ax,[''],[n],[x],'Total read time · ms · lower is better')
 for t in ax.texts:
  v=x if t.get_fontweight()=='bold' else n
  t.set_text(f'{v:.3f}' if v<1 else f'{v:.2f}')
 ax.set_title(title,loc='left',fontsize=13,fontweight='bold',pad=15)
 winner='NX' if n>x else 'NUP v1'
 ratio=max(n,x)/min(n,x)
 value=f'{ratio:.0f}' if ratio>=100 else f'{ratio:.1f}'
 note=f'{winner}: ~{value}× faster' + (' on these initial reads' if i==0 else ' on this trace')
 fig.text(left,bottom+.21,note,fontsize=11,color=NX if winner=='NX' else NUP,fontweight='bold')
fig.text(.055,.10,'NX reads RAW or Zstd blocks; NUP v1 decodes whole compressed files and caches them.',fontsize=12,fontweight='bold')
fig.text(.055,.06,'Eight largest compressed members · generated traces · median of 3 runs · OS cache not flushed.',fontsize=10,color=MUTED)
fig.text(.055,.03,'Each panel starts at zero and uses its own scale. The 109× result is a targeted case, not an overall-game speedup.',fontsize=9,color=MUTED)
fig.savefig(OUT/'readme-first-access.png',dpi=170);plt.close(fig)
