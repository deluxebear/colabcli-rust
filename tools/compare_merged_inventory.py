#!/usr/bin/env python3
"""Run merged inventory commands against a per-account loopback fixture, with real cached OAuth loading."""
import argparse
from collections import Counter
import copy
from datetime import datetime, timedelta, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from urllib.parse import urlsplit
from capture_merged_help import fingerprint_sources, normalize
from compare_merged_accounts import load_app
from compare_merged_account_mutations import require_no_native_keyring

ROOT = Path(__file__).resolve().parents[1]


def cases():
    result = [(name, arguments, {}) for name, arguments in [
        ('status-json',['account','status','--json']), ('status-text',['acct','status']),
        ('sessions-all-json',['sessions','--all','--json']), ('sessions-all-text',['sessions','--all']),
        ('usage-all-json',['usage','--all','--json']), ('usage-all-text',['usage','--all']),
        ('sessions-all-selected-context',['-a','work','sessions','--all','--json']),
        ('usage-all-selected-context',['-a','work','usage','--all']),
        ('sessions-single-json',['-a','work','sessions','--json']), ('usage-single-json',['-a','work','usage','--json']),
    ]]
    result += [('sessions-single-error',['-a','work','sessions','--json'],{'failure':'assignments'}),
        ('usage-single-error',['-a','work','usage','--json'],{'failure':'ccu-info'}),
        ('status-assignment-error',['account','status','--json'],{'failure':'assignments'}),
        ('status-usage-error',['account','status','--json'],{'failure':'ccu-info'}),
        ('sessions-assignment-error',['sessions','--all','--json'],{'failure':'assignments'}),
        ('usage-error',['usage','--all','--json'],{'failure':'ccu-info'}),
        ('sessions-empty',['sessions','--all'],{'empty':True}),
        ('status-missing-auth',['account','status','--json'],{'missing_auth':True}),
        ('sessions-global-missing-text',['-a','missing','sessions'],{}),
        ('usage-global-missing-text',['-a','missing','usage'],{}),
        ('status-global-missing',['-a','missing','account','status','--json'],{}),
        ('sessions-global-missing',['-a','missing','sessions','--all','--json'],{})]
    return result


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value))
    path.chmod(0o600)


def fixture(home, base, case):
    write(home / 'config/accounts.json',{'version':1,'active':'default','default_drive_account':'work',
        'accounts':{'default':{'email':'default@example.invalid'},'work':{'email':'work@example.invalid','drive_account':'default'}}})
    client={'installed':{'client_id':'fixture-client','client_secret':'fixture-secret','auth_uri':'https://accounts.google.com/o/oauth2/auth','token_uri':base+'/oauth/token','redirect_uris':['http://localhost']}}
    write(home / '.colab-cli-oauth-config.json',client)
    for name in ('default','work'):
        root=home / 'config/accounts' / name
        token={'token':'fixture-bearer-'+name,'refresh_token':'fixture-refresh-'+name,'token_uri':base+'/oauth/token',
            'client_id':'fixture-client','client_secret':'fixture-secret','expiry':(datetime.now(timezone.utc)+timedelta(days=1)).isoformat().replace('+00:00','Z'),'scopes':['openid']}
        if not (case.get('missing_auth') and name=='work'):
            write(root/'oauth_token.json',token)
        write(root/'oauth_config.json',client)
        write(root/'sessions.json',{key:{'name':key,'endpoint':endpoint,'token':'old-proxy-'+name,'url':base+'/old-runtime','variant':'DEFAULT','accelerator':'NONE','machine_shape':'STANDARD'}
            for key,endpoint in [('cpu' if name=='default' else 'train','ep-'+name),('stale','gone-'+name)]})
        (root/'history').mkdir()
    write(home/'config/accounts/default/dfs_credentials.json',{})


class Backend(ThreadingHTTPServer):
    daemon_threads=True
    def __init__(self,case):
        super().__init__(('127.0.0.1',0),Handler)
        self.base='http://127.0.0.1:'+str(self.server_port)
        self.case=case
        self.trace=[]
        self.errors=[]


class Handler(BaseHTTPRequestHandler):
    def log_message(self,*_args):
        pass
    def do_GET(self):
        name=self.headers.get('Authorization','').removeprefix('Bearer fixture-bearer-')
        parsed=urlsplit(self.path)
        path=parsed.path
        if parsed.query != 'authuser=0': self.server.errors.append('missing fixed authuser=0 query')
        if name not in ('default','work') or path not in ('/tun/m/ccu-info','/tun/m/assignments'):
            self.server.errors.append('unexpected method, account bearer or path')
            self.send_error(400)
            return
        self.server.trace.append({'account':name,'path':path})
        status=503 if name=='work' and self.server.case.get('failure')==path.rsplit('/',1)[-1] else 200
        if status!=200:
            body={'error':'fixture-private-response'}
        elif path.endswith('ccu-info'):
            body={'currentBalance':12.5 if name=='default' else 3.25,'consumptionRateHourly':1.25 if name=='default' else 0.5,'assignmentsCount':0 if self.server.case.get('empty') else (2 if name=='default' else 1)}
        else:
            items=[] if self.server.case.get('empty') else [{'endpoint':'ep-'+name,'accelerator':'NONE' if name=='default' else 'T4','variant':0 if name=='default' else 1,'machineShape':0 if name=='default' else 1,
                'runtimeProxyInfo':{'token':'proxy-'+name,'tokenExpiresInSeconds':3600,'url':self.server.base+'/runtime/'+name}}]
            if name=='default' and items:
                items.append(dict(items[0],endpoint='orphan-default',accelerator='L4',variant=1))
            body={'assignments':items}
        encoded=json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(encoded)));self.end_headers();self.wfile.write(encoded)
    def do_POST(self):
        self.server.errors.append('inventory unexpectedly mutated or refreshed')
        self.send_error(400)


def state(home,base):
    rows={}
    for name in ('default','work'):
        sessions=json.loads((home/'config/accounts'/name/'sessions.json').read_text())
        rows[name]={'names':sorted(sessions),'refreshed':{key:value.get('token')=='proxy-'+name and value.get('url')==base+'/runtime/'+name for key,value in sessions.items()}}
    return rows


def child(args):
    app,violations=load_app(args,loopback_origin=args.base)
    import requests
    original=requests.Session.__init__
    def session_init(self,*values,**kwargs):
        original(self,*values,**kwargs)
        self.trust_env=False
    requests.Session.__init__=session_init
    from colab_cli import common
    from colab_cli.client import Prod
    common.Prod=lambda: Prod(domain=args.base,api=args.base)
    from typer.testing import CliRunner
    result=CliRunner().invoke(app,json.loads(args.arguments),prog_name='colab',color=False)
    import importlib.metadata
    versions={name:importlib.metadata.version(name) for name in ('google-auth','requests','typer','pydantic')}
    print(json.dumps({'exit_code':result.exit_code,'stdout':result.stdout,'stderr':result.stderr,'forbidden_operations':violations,'dependency_versions':versions,'python_version':sys.version.split()[0]}))


def expected_trace(name,arguments,case):
    profiles=('default','work') if '--all' in arguments or 'account' in arguments or 'acct' in arguments else ('work',)
    paths={profile:[] for profile in ('default','work')}
    if 'global-missing' in name: return paths
    for profile in profiles:
        if profile=='work' and case.get('missing_auth'): continue
        if 'sessions' not in arguments:
            paths[profile].append('/tun/m/ccu-info')
            if 'usage' in arguments or (profile=='work' and case.get('failure')=='ccu-info'): continue
        paths[profile].append('/tun/m/assignments')
    return paths


def clean(result,home,base,arguments):
    result={key:normalize(result[key],home).replace(base,'<LOOPBACK>') if isinstance(result[key],str) else result[key] for key in ('exit_code','stdout','stderr')}
    if '--json' in arguments and result['exit_code']==0:
        result['json']=json.loads(result.pop('stdout'))
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for key in ('cli','ext','kernel','python','deps'):
        parser.add_argument('--'+key,type=Path,required=key in ('cli','ext','kernel'))
    parser.add_argument('--rust',type=Path,default=ROOT/'target/no-keyring/colab')
    parser.add_argument('--output',type=Path,default=ROOT/'research/merged-inventory-comparison.json')
    parser.add_argument('--child',action='store_true');parser.add_argument('--base');parser.add_argument('--arguments')
    args=parser.parse_args()
    if args.child:return child(args)
    if not args.python:parser.error('--python is required')
    require_no_native_keyring(args.rust)
    sources=fingerprint_sources({key:getattr(args,key) for key in ('cli','ext','kernel')})
    args.deps=Path(subprocess.check_output([str(args.python),'-S','-c','import sysconfig; print(sysconfig.get_path("purelib"))'],text=True).strip())
    records=[]
    runtime=None
    for name,arguments,case in cases():
        row={'name':name,'arguments':arguments}
        for implementation in ('official','rust'):
            server=Backend(case)
            thread=threading.Thread(target=server.serve_forever,kwargs={'poll_interval':0.02},daemon=True);thread.start()
            try:
                with tempfile.TemporaryDirectory(prefix='merged-inventory-') as directory:
                    home=Path(directory).resolve();fixture(home,server.base,case)
                    credentials={path:path.read_bytes() for path in (home/'config/accounts').glob('*/oauth_token.json')}
                    registry=(home/'config/accounts.json').read_bytes()
                    env={'HOME':str(home),'PATH':os.defpath,'LANG':'C.UTF-8','COLAB_TOKEN_STORAGE':'file','COLAB_CLI_CONFIG_DIR':str(home/'config'),'NO_COLOR':'1','TERM':'dumb'}
                    if implementation=='official':
                        command=[str(args.python),'-S',str(Path(__file__).resolve()),'--child','--deps',str(args.deps),'--base',server.base,'--arguments',json.dumps(arguments)]
                        for key in ('cli','ext','kernel'):command+=['--'+key,str(getattr(args,key).resolve())]
                    else:
                        command=[str(args.rust.resolve()),'--token-storage','file','--control-plane-url',server.base,'--request-timeout','2','--assign-retries','0',*arguments]
                    run=subprocess.run(command,env=env,cwd=home,capture_output=True,text=True,timeout=30)
                    if implementation=='official':
                        if run.returncode:raise RuntimeError(run.stderr)
                        result=json.loads(run.stdout);row['forbidden_operations']=result['forbidden_operations']
                        observed={key:result[key] for key in ('dependency_versions','python_version')}
                        if runtime is not None: assert observed==runtime,'dependency versions changed during probe'
                        runtime=observed
                    else:result={'exit_code':run.returncode,'stdout':run.stdout,'stderr':run.stderr}
                    for secret in ('fixture-bearer','fixture-refresh','fixture-secret','proxy-default','proxy-work','fixture-private-response'):
                        assert secret not in result['stdout']+result['stderr'],name+': leaked fixture secret'
                    row[implementation]=clean(result,home,server.base,arguments)
                    row[implementation]['state']=state(home,server.base)
                    row[implementation]['trace']={profile:[item['path'] for item in server.trace if item['account']==profile] for profile in ('default','work')}
                    row[implementation]['credentials_unchanged']=all(path.read_bytes()==contents for path,contents in credentials.items())
                    row[implementation]['registry_unchanged']=(home/'config/accounts.json').read_bytes()==registry
                    assert row[implementation]['credentials_unchanged'] and row[implementation]['registry_unchanged'], name+': modified credentials or registry'
                    expected = expected_trace(name, arguments, case)
                    assert row[implementation]['trace'] == expected, name+': account request isolation or ordering mismatch'
                    for profile, paths in expected.items():
                        refreshed = '/tun/m/assignments' in paths and not (profile=='work' and case.get('failure')=='assignments')
                        names = [] if refreshed and case.get('empty') else ['cpu' if profile=='default' else 'train']
                        if not refreshed: names.append('stale')
                        assert row[implementation]['state'][profile]['names'] == sorted(names), name+': incorrect stale-session pruning'
                        assert all(flag is refreshed for flag in row[implementation]['state'][profile]['refreshed'].values()), name+': incorrect runtime proxy refresh'
                    assert not server.errors,server.errors
            finally:
                server.shutdown();server.server_close();thread.join(timeout=3)
        row['matched']=row['official']==row['rust']
        row['status']='matched' if row['matched'] else 'unexpected_difference'
        if not row['matched'] and (case.get('failure') or case.get('missing_auth')):
            official,rust=copy.deepcopy(row['official']),copy.deepcopy(row['rust'])
            left,right=official.get('json'),rust.get('json')
            pairs = [(left,right)] if isinstance(left,dict) and isinstance(right,dict) else list(zip(left,right)) if isinstance(left,list) and isinstance(right,list) and len(left)==len(right) else []
            if pairs:
                changed=[]
                for index,(a,b) in enumerate(pairs):
                    if a.get('error') and b.get('error') and a['error']!=b['error']:
                        changed.append(index)
                        a['error']=b['error']='<typed-account-error>'
                if changed and official==rust:
                    row['status']='preserved_error_text_difference'
                    row['error_row_indices']=changed
        records.append(row)
    report={**runtime,'sources':sources,'rust_binary_sha256':hashlib.sha256(args.rust.read_bytes()).hexdigest(),'rust_native_keyring_compiled':False,
        'scope':'actual merged account/session/usage probes, real cached synthetic OAuth loading; exact loopback origin only; no real credentials or resources',
        'normalization':['temporary HOME and exact loopback origin','cross-account request interleaving only; per-account request order retained','runtime cache compared by names and validated token/url refresh; time and hardware cache extras not equated'],
        'cases':records,'summary':{'cases':len(records),**dict(Counter(row['status'] for row in records))}}
    args.output.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n');print(json.dumps(report['summary']))
    if any(row['status']=='unexpected_difference' or row['forbidden_operations'] for row in records):raise SystemExit(1)


if __name__=='__main__':main()
