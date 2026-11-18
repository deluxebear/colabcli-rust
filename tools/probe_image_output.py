#!/usr/bin/env python3
"""固定上游图片显示纯函数与自有临时文件；不导入 CLI，不访问账户或网络。"""
import argparse
import ast
import base64
import hashlib
import io
import json
import logging
from pathlib import Path
import socket
import tempfile
from types import SimpleNamespace

PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRz8AAAAASUVORK5CYII='

class Output(io.StringIO):
    def __init__(self, tty):
        super().__init__()
        self.tty = tty
    def isatty(self):
        return self.tty


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cli', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    inventory = json.loads((Path(__file__).resolve().parents[1]/'research/source-inventory.json').read_text())
    relative = 'src/colab_cli/utils.py'
    source = (args.cli/relative).read_bytes()
    expected = next(row['sha256'] for row in inventory['repositories']['cli']['files'] if row['path']==relative)
    assert hashlib.sha256(source).hexdigest()==expected, '固定源码哈希不匹配'
    functions = [node for node in ast.parse(source).body if isinstance(node,ast.FunctionDef) and node.name in {'print_kitty','handle_image'}]
    assert len(functions)==2
    def forbidden(*args, **kwargs):
        raise AssertionError('图片探针禁止网络')
    socket.socket = forbidden
    socket.create_connection = forbidden
    socket.getaddrinfo = forbidden
    namespace = {'base64':base64,'logging':logging}
    exec(compile(ast.Module(body=functions,type_ignores=[]),str(args.cli/relative),'exec'),namespace)
    png = base64.b64decode(PNG)
    cases = []
    for name,tty,data in [('pipe',False,png),('terminal-small',True,png),('terminal-large-wire-fixture',True,png+b'x'*10000)]:
        output = Output(tty)
        namespace['sys'] = SimpleNamespace(stdout=output)
        namespace['print_kitty'](data)
        text = output.getvalue()
        if tty:
            control,payload = text.strip('\n').removeprefix('\x1b_G').removesuffix('\x1b\\').split(';',1)
            assert control=='a=T,f=100'
            assert base64.b64decode(payload)==data
        else:
            assert text==''
        cases.append({'name':name,'tty':tty,'input_base64':base64.b64encode(data).decode(),
                      'official_stdout':text,'scope':'transmitted bytes only; no visual PNG decoding'})
    artifacts = []
    for name,tty,mime,data,explicit in [
        ('png-fallback',False,'image/png',png,False),
        ('png-explicit',False,'image/png',png,True),
        ('jpeg-fallback',False,'image/jpeg',b'\xff\xd8\xff\x00\x01\x02',False),
        ('jpeg-terminal',True,'image/jpeg',b'\xff\xd8\xff\x00\x01\x02',False),
    ]:
        with tempfile.TemporaryDirectory(prefix='colab-image-probe-') as temporary:
            root = Path(temporary)
            output = Output(tty)
            namespace['sys'] = SimpleNamespace(stdout=output)
            namespace['tempfile'] = SimpleNamespace(NamedTemporaryFile=lambda **kw:tempfile.NamedTemporaryFile(dir=root,**kw))
            namespace['print'] = lambda *values,**kw:print(*values,file=output,**kw)
            target = root/'explicit.png' if explicit else None
            namespace['handle_image'](base64.b64encode(data).decode()+'\n',mime,target_path=str(target) if target else None)
            files = list(root.iterdir())
            assert len(files)==1 and files[0].read_bytes()==data
            saved = files[0]
            text = output.getvalue().replace(str(saved),'<IMAGE_FILE>')
            assert '[Image saved to: <IMAGE_FILE>]' in text
            artifacts.append({'name':name,'tty':tty,'mime':mime,'explicit':explicit,'input_base64':base64.b64encode(data).decode(),
                              'official_stdout':text,'saved_extension':saved.suffix,'saved_mode':oct(saved.stat().st_mode&0o777),
                              'scope':'synthetic bytes; no JPEG visual decoding'})
    args.output.write_text(json.dumps({'source_commit':inventory['repositories']['cli']['commit'],
        'source_path':relative,'source_sha256':expected,'kitty_cases':cases,'artifact_cases':artifacts,
        'isolation':'verified AST functions; fake stdout; owned TemporaryDirectory; no CLI import/credentials/network',
        'intentional_differences':['Rust chunks Kitty payloads at 4096 base64 bytes and suppresses terminal replies',
                                  'Rust saves JPEG but does not label JPEG bytes as Kitty PNG',
                                  'Rust image presentation/automatic-save failures do not make execution uncertain']},ensure_ascii=False,indent=2)+'\n')
    print(json.dumps({'kitty_cases':len(cases),'artifact_cases':len(artifacts),'network':'forbidden'}))

if __name__=='__main__':
    main()
