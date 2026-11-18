#!/usr/bin/env python3
"""对照服务须检测错误的账户令牌、路径编码及上传字节。"""
import importlib.util
import json
from pathlib import Path
import sys
import threading
import unittest
from urllib.error import HTTPError
from urllib.request import build_opener, ProxyHandler, Request
from urllib.parse import quote

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
SPEC = importlib.util.spec_from_file_location('merged_files', TOOLS / 'compare_merged_files.py')
files = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(files)


class FileOracleTests(unittest.TestCase):
    def setUp(self):
        self.server = files.Backend('directory')
        self.server.operation = 'ls'
        self.server.expected_remote = files.REMOTE
        thread = threading.Thread(target=self.server.serve_forever, kwargs={'poll_interval': 0.01}, daemon=True)
        thread.start()
        def cleanup():
            self.server.shutdown()
            self.server.server_close()
            thread.join(timeout=3)
        self.addCleanup(cleanup)
        self.opener = build_opener(ProxyHandler({}))
        self.url = self.server.base + '/runtime/work/api/contents/' + quote(files.REMOTE, safe='/')
        self.query = '?authuser=0&colab-runtime-proxy-token=proxy-work'

    def test_compute_bearer_or_wrong_proxy_is_rejected(self):
        for request in (Request(self.url + self.query, headers={'Authorization': 'Bearer synthetic-control'}),
                        Request(self.url + self.query.replace('proxy-work', 'proxy-default'))):
            with self.assertRaises(HTTPError) as caught:
                self.opener.open(request, timeout=2)
            self.assertEqual(caught.exception.code, 400)
            caught.exception.close()
        self.assertEqual(len(self.server.errors), 2)
        self.assertFalse(self.server.contents_trace)

    def test_double_encoded_path_is_flagged(self):
        url = self.url.replace('%23', '%2523') + self.query
        with self.opener.open(url, timeout=2) as response:
            self.assertEqual(response.status, 200)
        self.assertEqual(self.server.errors, ['remote path encoding mismatch'])

    def test_wrong_upload_bytes_are_flagged(self):
        self.server.mode = self.server.operation = 'upload'
        data = json.dumps({'content': 'wrong-fixture'}).encode()
        with self.opener.open(Request(self.url + self.query, data=data, method='PUT'), timeout=2) as response:
            self.assertEqual(response.status, 200)
        self.assertEqual(self.server.errors, ['unexpected upload payload or bytes'])


if __name__ == '__main__':
    unittest.main()
