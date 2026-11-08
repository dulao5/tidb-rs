import re, sys
G='/Users/du-zhigang/go/pkg/mod/github.com/pingcap/tidb@v1.1.0-beta.0.20260827064614-8b857efa2036/pkg/sessionctx/variable/'
RS='/Users/du-zhigang/.slock/agents/4e290aa1-0152-4fca-95a4-03ab60358460/projects/tidb-rs-d052/crates/sessionctx/src/variable/'
TSV='/Users/du-zhigang/.slock/agents/4e290aa1-0152-4fca-95a4-03ab60358460/projects/tracker-d052/data/sysvar_hooks.tsv'
# entries: name, file, line
entries={}
for l in open(TSV):
    if l.startswith('#') or l.startswith('name\t'): continue
    f=l.rstrip('\n').split('\t')
    fn,ln=f[1].split(':'); entries.setdefault(fn,[]).append((int(ln),f[0]))
# fields
fields={}; ftype={}
lines=open(RS+'session_fields.rs').read().split('\n')
for i,l in enumerate(lines):
    m=re.match(r'\s*/// Go: `(?:\w+\.)?(\w+) ',l)
    if m:
        for j in range(i+1,i+4):
            m2=re.match(r'\s*pub (\w+): ([\w&\']+),',lines[j])
            if m2: fields[m.group(1)]=m2.group(1); ftype[m.group(1)]=m2.group(2); break
consts={}
for f in ['tidb_defs.rs','tidb_vars.rs']:
    for m in re.finditer(r'pub const (\w+): ([\w&\']+) =',open(RS+f).read()): consts[m.group(1)]=m.group(2)
FN={'TiDBOptOn':('tidb_opt_on',[],'bool'),
    'tidbOptFloat64':('tidb_opt_float64',['f64'],'f64'),'TidbOptFloat64':('tidb_opt_float64',['f64'],'f64'),
    'tidbOptPositiveInt32':('tidb_opt_positive_int32',['isize'],'isize'),
    'TidbOptInt':('tidb_opt_int',['isize'],'isize'),'TidbOptInt64':('tidb_opt_int64',['i64'],'i64'),
    'TidbOptUint64':('tidb_opt_uint64',['u64'],'u64'),'TiDBOptOnOffWarn':('tidb_opt_on_off_warn',[],'i64')}
CAST={'int':'i64','uint64':'u64','int64':'i64','uint':'u64','int32':'i32','float64':'f64'}
class Unsup(Exception): pass
def split_args(s):
    out=[];d=0;cur=''
    for ch in s:
        if ch=='(' : d+=1
        if ch==')' : d-=1
        if ch==',' and d==0: out.append(cur.strip()); cur=''
        else: cur+=ch
    if cur.strip(): out.append(cur.strip())
    return out
def tr(e):
    """returns (rust_expr, rust_type)"""
    e=e.strip()
    if e=='val': return ('val','str')
    if re.fullmatch(r'-?\d+',e): return (e,'intlit')
    if re.fullmatch(r'-?\d+\.\d*',e): return (e,'f64')
    if e in ('true','false'): return (e,'bool')
    if e=='logutil.DefaultSlowTxnThreshold': return ('DefaultSlowTxnThreshold','u64')
    if e in consts: return (e,consts[e])
    m=re.fullmatch(r'([\w.]+)\((.*)\)',e,re.S)
    if not m: raise Unsup(e)
    f,args=m.group(1),split_args(m.group(2))
    if f in CAST and len(args)==1:
        x,t=tr(args[0]); T=CAST[f]
        if t==T or t=='intlit': return (x,T)
        if t=='str': raise Unsup(e)
        return (f'({x}) as {T}' if ' ' in x else f'{x} as {T}',T)
    if f=='strings.ToUpper' and len(args)==1:
        x,t=tr(args[0])
        if t!='str': raise Unsup(e)
        return (f'gostd::to_upper({x})','String')
    if f in FN:
        rf,ptypes,ret=FN[f]
        if len(args)!=1+len(ptypes): raise Unsup(e)
        a0,t0=tr(args[0])
        if t0!='str': raise Unsup(e)
        rargs=[a0]
        for a,pt in zip(args[1:],ptypes):
            x,t=tr(a)
            if t=='intlit' or t==pt: rargs.append(x if t!='intlit' or pt!='f64' else x+'.0')
            elif t in ('i64','i32','u64','isize','f64','u32') : rargs.append(f'{x} as {pt}')
            else: raise Unsup(e)
        return (f'{rf}({", ".join(rargs)})',ret)
    raise Unsup(e)
def bodies(src,key):
    for m in re.finditer(key+r': func\(s \*SessionVars, val string\) error \{',src):
        i=m.end(); d=1
        while d:
            if src[i]=='{': d+=1
            elif src[i]=='}': d-=1
            i+=1
        yield src[:m.start()].count('\n')+1, src[m.end():i-1]
def snake(n): return re.sub(r'[^a-z0-9]+','_',n.lower()).strip('_')
out=[]; table=[]; pend=[]
for gofile in ['sysvar.go','noop.go']:
    src=open(G+gofile).read()
    ents=sorted(entries.get(gofile,[]))
    for ln,body in bodies(src,'SetSession'):
        name=[n for (l,n) in ents if l<=ln][-1]
        ls=[x.strip() for x in body.strip().split('\n') if x.strip() and not x.strip().startswith('//')]
        try:
            # inline `tmp := EXPR` temporaries used once in the assignment
            while len(ls)==3 and re.fullmatch(r'\w+ := .*',ls[0]):
                tn,te=ls[0].split(' := ',1)
                ls=[re.sub(r'\b'+tn+r'\b',te,ls[1])]+ls[2:]
            if len(ls)!=2 or ls[1]!='return nil': raise Unsup('shape')
            m=re.fullmatch(r's\.(\w+) = (.*)',ls[0])
            if not m: raise Unsup('lhs')
            fld,rhs=m.group(1),m.group(2)
            if fld not in fields: raise Unsup('field '+fld)
            x,t=tr(rhs); T=ftype[fld]
            if T=='String':
                if t=='String': pass
                elif t!='str' or x!='val': raise Unsup('string')
                else: x='val.to_owned()'
            elif t=='intlit': pass
            elif t!=T:
                if t in ('bool','str') or T=='bool': raise Unsup(f'type {t}->{T}')
                x=f'({x}) as {T}' if ' ' in x and not x.endswith(')') else f'{x} as {T}'
            fn='ss_'+snake(name)
            vn='val' if re.search(r'\bval\b',x) else '_val'
            out.append(f'// go-part: {"defaultSysVars" if gofile=="sysvar.go" else "noopSysVars"} ({gofile}:L{ln} {name} SetSession closure) L1\nfn {fn}(s: &mut SessionVars, {vn}: &str) -> Result<(), Error> {{\n    s.{fields[fld]} = {x};\n    Ok(())\n}}\n')
            table.append((name,fn))
        except Unsup as ex:
            pend.append((name,gofile,ln,str(ex)))
open('ss_gen.rs','w').write('\n'.join(out))
open('ss_table.txt','w').write('\n'.join(f'        "{n}" => {f},' for n,f in table))
open('ss_pending.txt','w').write('\n'.join(f'{n}\t{g}:{l}\t{r}' for n,g,l,r in pend))
print(len(table),'translated;',len(pend),'pending')
