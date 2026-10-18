#!/usr/bin/env python3
"""API latency while rustnzb downloads (WI-1294).

Samples /api/status and /api/queue every 250 ms and /api/history every 5 s
(the web UI's poll rate), first with an idle queue and then while NJOBS
synthetic NZBs of SIZE bytes download from `benchnzb synth-nntp`. Prints
p50/p95/p99/max per phase and endpoint. Total download is NJOBS * SIZE bytes,
so keep it within the disk you can spare and clean the download dir after.

    benchnzb synth-nntp --port 11119 --health-port 18080 &
    rustnzb -c rustnzb.toml &   # server = 127.0.0.1:11119, fresh data_dir
    python3 ui-latency.py http://127.0.0.1:19090 NJOBS SIZE IDLE_SECS MAX_SECS
"""
import json,sys,time,threading,urllib.request,uuid,statistics,os
BASE=sys.argv[1]; NJOBS=int(sys.argv[2]); SIZE=int(sys.argv[3]); IDLE=int(sys.argv[4]); MAXRUN=int(sys.argv[5])
def req(method,path,body=None,headers={},timeout=30):
    r=urllib.request.Request(BASE+path,data=body,method=method,headers=headers)
    with urllib.request.urlopen(r,timeout=timeout) as f: return f.read()
def login():
    try: return json.loads(req("POST","/api/auth/setup",json.dumps({"username":"bench","password":"benchpass-123456"}).encode(),{"Content-Type":"application/json"}))["access_token"]
    except Exception: return json.loads(req("POST","/api/auth/login",json.dumps({"username":"bench","password":"benchpass-123456"}).encode(),{"Content-Type":"application/json"}))["access_token"]
tok=login()
H={"Authorization":"Bearer "+tok}
def nzb(seq):
    pre=f"stress-{SIZE}-f{seq:06d}"; n=-(-SIZE//750000); fn=f"stress_f{seq:06d}.bin"
    segs="".join(f'<segment bytes="{min(750000,SIZE-(p-1)*750000)}" number="{p}">{pre}-p{p:05d}@benchnzb</segment>' for p in range(1,n+1))
    return (f'<?xml version="1.0" encoding="UTF-8"?><nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"><file poster="b@b" date="1" subject="{fn} (1/{n}) - &quot;{fn}&quot; yEnc (1/{n})"><groups><group>alt.binaries.test</group></groups><segments>{segs}</segments></file></nzb>').encode()
def add(seq):
    b="----x"+uuid.uuid4().hex
    body=(f"--{b}\r\nContent-Disposition: form-data; name=\"nzbfile\"; filename=\"s{seq}.nzb\"\r\nContent-Type: application/x-nzb\r\n\r\n").encode()+nzb(seq)+f"\r\n--{b}--\r\n".encode()
    req("POST","/api/queue/add",body,{**H,"Content-Type":"multipart/form-data; boundary="+b})
EPS=["/api/status","/api/queue","/api/history?limit=50&offset=0"]
samples={}; phase=["idle"]; stop=threading.Event()
def sampler(ep):
    iv=5.0 if ep.startswith("/api/history") else 0.25
    while not stop.is_set():
        t=time.perf_counter()
        try: req("GET",ep,headers=H,timeout=20); ok=True
        except Exception: ok=False
        samples.setdefault((phase[0],ep),[]).append((time.perf_counter()-t)*1000 if ok else 20000)
        time.sleep(iv)
th=[threading.Thread(target=sampler,args=(e,),daemon=True) for e in EPS]; [t.start() for t in th]
time.sleep(IDLE)
phase[0]="load"; t0=time.time()
for i in range(1,NJOBS+1): add(i)
peak=0
while time.time()-t0<MAXRUN:
    s=json.loads(req("GET","/api/status",headers=H)); peak=max(peak,s.get("speed_bps",0))
    if s.get("queue_size",1)==0 and time.time()-t0>10: break
    time.sleep(2)
elapsed=time.time()-t0; stop.set(); time.sleep(0.5)
hist=json.loads(req("GET","/api/history?limit=50&offset=0",headers=H))
print(f"load phase {elapsed:.0f}s, peak speed {peak/2**20:.0f} MiB/s, history rows {hist.get('total',hist.get('count','?'))}")
def pct(v,p): v=sorted(v); return v[min(len(v)-1,int(len(v)*p))]
print(f"{'phase':5} {'endpoint':32} {'n':>5} {'p50':>7} {'p95':>7} {'p99':>7} {'max':>8}  ms")
for (ph,ep),v in sorted(samples.items()):
    print(f"{ph:5} {ep:32} {len(v):5} {pct(v,.5):7.1f} {pct(v,.95):7.1f} {pct(v,.99):7.1f} {max(v):8.1f}")
