#!/usr/bin/env python3
"""固定上游 MIME 文本渲染样本；只加载经哈希核对的纯函数，不导入 CLI 或读取凭据。"""
import argparse
import ast
import hashlib
import importlib.metadata
import io
import json
from pathlib import Path
import socket
from typing import Union


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cli', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    inventory = json.loads((Path(__file__).resolve().parents[1] / 'research/source-inventory.json').read_text())
    relative = 'src/colab_cli/utils.py'
    source = (args.cli / relative).read_bytes()
    expected = next(row['sha256'] for row in inventory['repositories']['cli']['files'] if row['path'] == relative)
    assert hashlib.sha256(source).hexdigest() == expected, '固定上游源码哈希不匹配'
    import html2text
    from rich.console import Console
    from rich.markdown import Markdown
    from rich.text import Text

    def forbidden(*args, **kwargs):
        raise AssertionError('离线渲染探针禁止网络')
    socket.socket = forbidden
    socket.create_connection = forbidden
    socket.getaddrinfo = forbidden
    function = next(node for node in ast.parse(source).body if isinstance(node, ast.FunctionDef) and node.name == 'render_display_data')
    namespace = {'Union': Union, 'Markdown': Markdown, 'Text': Text, 'html2text': html2text}
    exec(compile(ast.Module(body=[function], type_ignores=[]), str(args.cli / relative), 'exec'), namespace)
    cases = [
        ('plain', {'text/plain': 'plain output'}, 'whitespace', ['plain output'], []),
        ('unicode', {'text/plain': '中文 café 😀'}, 'whitespace', ['中文 café 😀'], []),
        ('markdown', {'text/markdown': 'Hello **world**'}, 'whitespace', ['Hello world'], ['**']),
        ('html', {'text/html': '<p>Hello <b>world</b></p>'}, 'whitespace', ['Hello world'], ['<p>']),
        ('markdown-priority', {'text/markdown': 'chosen **markdown**', 'text/html': '<p>wrong html</p>', 'text/plain': 'wrong plain'}, 'whitespace', ['chosen markdown'], ['wrong']),
        ('html-priority', {'text/html': '<p>chosen <b>html</b></p>', 'text/plain': 'wrong plain'}, 'whitespace', ['chosen html'], ['wrong']),
        ('markdown-link', {'text/markdown': '[local label](https://example.invalid/no-fetch)'}, 'whitespace', ['local label'], ['no-fetch']),
        ('html-entities', {'text/html': '<p>中文 &amp; 2 &lt; 3</p>'}, 'whitespace', ['中文 & 2 < 3'], ['&amp;']),
        ('code', {'text/markdown': '```python\nanswer = 42\n```'}, 'whitespace', ['answer = 42'], ['```']),
        ('heading', {'text/markdown': '# Heading\n\nParagraph'}, 'layout_difference', ['Heading','Paragraph'], ['<h1>']),
        ('list', {'text/markdown': '- first\n- second'}, 'layout_difference', ['first','second'], ['<li>']),
        ('table', {'text/markdown': '| Name | Value |\n|---|---|\n| alpha | 7 |'}, 'layout_difference', ['Name','Value','alpha','7'], ['<table>']),
        ('inert-html', {'text/html': '<style>hidden-style</style><script>hidden-script</script><p>visible only</p>'}, 'whitespace', ['visible only'], ['hidden-style','hidden-script']),
        ('html-line-breaks', {'text/html': '<p>first<br>second</p><p>third</p>'}, 'whitespace', ['first','second','third'], ['<br>']),
        ('image-only', {'image/png': 'synthetic-not-an-image'}, 'whitespace', [], ['synthetic-not-an-image']),
        ('empty-markdown', {'text/markdown': '', 'text/plain': 'wrong fallback'}, 'whitespace', [], ['wrong fallback']),
    ]
    rows = []
    for name, data, comparison, required, forbidden_text in cases:
        output = io.StringIO()
        rendered = namespace['render_display_data'](data)
        if rendered is not None:
            Console(file=output, width=80, force_terminal=False, color_system=None, highlight=False).print(rendered)
        text = output.getvalue()
        normalized = ' '.join(text.split())
        assert all(fragment in normalized for fragment in required), (name, text)
        assert not any(fragment in text for fragment in forbidden_text), (name, text)
        rows.append({'name':name,'data':data,'comparison':comparison,'official_text':text,
                     'required_fragments':required,'forbidden_fragments':forbidden_text})
    args.output.write_text(json.dumps({'source_commit': inventory['repositories']['cli']['commit'],
        'source_path': relative,'source_sha256':expected,'console_width':80,
        'dependencies':{name:importlib.metadata.version(name) for name in ['rich','html2text']},
        'scope':'fixed pure render_display_data + actual Rich Console; synthetic MIME only; network forbidden',
        'cases':rows}, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps({'captured_cases':len(rows),'network':'forbidden'}))


if __name__ == '__main__':
    main()
